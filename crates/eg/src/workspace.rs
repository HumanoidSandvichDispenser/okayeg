//! A confined view of a directory tree.
//!
//! The bridge talks to the filesystem only through [`Workspace`], never
//! `std::fs` directly. All paths are relative to the workspace root and nothing
//! can escape it. Production uses [`CapWorkspace`], which confines every
//! operation at the syscall layer with cap-std. Tests use [`MemWorkspace`], an
//! in-memory tree with no disk and no symlinks.

use std::io;
use std::path::Path;

#[cfg(test)]
use std::collections::BTreeMap;
#[cfg(test)]
use std::path::PathBuf;

/// Whether a path is a file or a directory.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    File,
    Dir,
}

/// One entry in a directory listing.
pub enum Entry {
    File(String),
    Dir(String),
}

/// A confined directory tree. Paths are relative to the root.
pub trait Workspace {
    /// List the entries directly under `rel`.
    fn read_dir(&self, rel: &Path) -> io::Result<Vec<Entry>>;
    /// Read the bytes of the file at `rel`.
    fn read_file(&self, rel: &Path) -> io::Result<Vec<u8>>;
    /// Write `contents` to the file at `rel`, creating parents as needed.
    fn write_file(&self, rel: &Path, contents: &[u8]) -> io::Result<()>;
    /// Write `contents` to `rel` as an owner-only (0600) file, creating or
    /// overwriting, for repo-private state like the doc and trust set.
    fn write_private(&self, rel: &Path, contents: &[u8]) -> io::Result<()>;
    /// Create the directory at `rel` (and any missing parents).
    fn create_dir(&self, rel: &Path) -> io::Result<()>;
    /// Remove the file at `rel`. `NotFound` when there is none.
    fn remove_file(&self, rel: &Path) -> io::Result<()>;
    /// Remove the directory at `rel`; it must be empty. `NotFound` when there
    /// is none, `DirectoryNotEmpty` (or equivalent) when it holds children.
    fn remove_dir(&self, rel: &Path) -> io::Result<()>;
    /// What is at `rel` right now: a file, a directory, or nothing.
    fn kind(&self, rel: &Path) -> io::Result<Option<Kind>>;
}

/// A cap-std confined workspace rooted at a real directory.
pub struct CapWorkspace {
    dir: cap_std::fs::Dir,
}

impl CapWorkspace {
    /// Open `root` as a confined workspace.
    ///
    /// Uses ambient authority once, here, to obtain the root handle; every
    /// later operation is confined to it.
    pub fn open(root: &Path) -> io::Result<Self> {
        let dir = cap_std::fs::Dir::open_ambient_dir(root, cap_std::ambient_authority())?;
        Ok(Self { dir })
    }
}

impl Workspace for CapWorkspace {
    fn read_dir(&self, rel: &Path) -> io::Result<Vec<Entry>> {
        // cap-std has no notion of the root as a path, so list it via `entries`.
        let read_dir = if rel.as_os_str().is_empty() {
            self.dir.entries()?
        } else {
            self.dir.read_dir(rel)?
        };
        let mut out = Vec::new();
        for entry in read_dir {
            let entry = entry?;
            let name = entry.file_name().to_string_lossy().into_owned();
            if entry.file_type()?.is_dir() {
                out.push(Entry::Dir(name));
            } else {
                out.push(Entry::File(name));
            }
        }
        Ok(out)
    }

    fn read_file(&self, rel: &Path) -> io::Result<Vec<u8>> {
        self.dir.read(rel)
    }

    fn write_file(&self, rel: &Path, contents: &[u8]) -> io::Result<()> {
        if let Some(parent) = rel.parent() {
            if !parent.as_os_str().is_empty() {
                self.dir.create_dir_all(parent)?;
            }
        }
        self.dir.write(rel, contents)
    }

    fn write_private(&self, rel: &Path, contents: &[u8]) -> io::Result<()> {
        use cap_std::fs::{OpenOptions, OpenOptionsExt};
        use std::os::unix::fs::PermissionsExt;
        if let Some(parent) = rel.parent() {
            if !parent.as_os_str().is_empty() {
                self.dir.create_dir_all(parent)?;
            }
        }
        let mut opts = OpenOptions::new();
        opts.write(true).create(true).truncate(true).mode(0o600);
        let mut file = self.dir.open_with(rel, &opts)?;
        std::io::Write::write_all(&mut file, contents)?;
        let perms = cap_std::fs::Permissions::from_std(std::fs::Permissions::from_mode(0o600));
        self.dir.set_permissions(rel, perms)
    }

    fn create_dir(&self, rel: &Path) -> io::Result<()> {
        if rel.as_os_str().is_empty() {
            return Ok(());
        }
        self.dir.create_dir_all(rel)
    }

    fn remove_file(&self, rel: &Path) -> io::Result<()> {
        self.dir.remove_file(rel)
    }

    fn remove_dir(&self, rel: &Path) -> io::Result<()> {
        if rel.as_os_str().is_empty() {
            return Ok(());
        }
        self.dir.remove_dir(rel)
    }

    fn kind(&self, rel: &Path) -> io::Result<Option<Kind>> {
        match self.dir.metadata(rel) {
            Ok(meta) if meta.is_dir() => Ok(Some(Kind::Dir)),
            Ok(_) => Ok(Some(Kind::File)),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e),
        }
    }
}

/// An in-memory workspace: a flat map of paths to file bytes, plus a set of
/// directory paths. No disk, no symlinks. For tests.
#[cfg(test)]
#[derive(Default)]
pub struct MemWorkspace {
    files: std::cell::RefCell<BTreeMap<PathBuf, Vec<u8>>>,
    dirs: std::cell::RefCell<std::collections::BTreeSet<PathBuf>>,
}

#[cfg(test)]
impl MemWorkspace {
    pub fn new() -> Self {
        Self::default()
    }

    /// Remove a file or directory entry (test helper).
    pub fn remove(&self, rel: &Path) {
        self.files.borrow_mut().remove(rel);
        self.dirs.borrow_mut().remove(rel);
    }
}

/// The direct child of `prefix` that `path` belongs to, if any, and whether
/// that child is itself a directory (i.e. `path` is nested below it).
#[cfg(test)]
fn direct_child(prefix: &Path, path: &Path) -> Option<(String, bool)> {
    let stripped = if prefix.as_os_str().is_empty() {
        path
    } else {
        path.strip_prefix(prefix).ok()?
    };
    let mut comps = stripped.components();
    let first = comps.next()?.as_os_str().to_string_lossy().into_owned();
    let nested = comps.next().is_some();
    Some((first, nested))
}

#[cfg(test)]
impl Workspace for MemWorkspace {
    fn read_dir(&self, rel: &Path) -> io::Result<Vec<Entry>> {
        let files = self.files.borrow();
        let dirs = self.dirs.borrow();
        let mut out = Vec::new();
        let mut seen = std::collections::BTreeSet::new();
        for path in dirs.iter().chain(files.keys()) {
            if let Some((name, nested)) = direct_child(rel, path) {
                if !seen.insert(name.clone()) {
                    continue;
                }
                let is_dir = nested || dirs.contains(&rel.join(&name));
                out.push(if is_dir {
                    Entry::Dir(name)
                } else {
                    Entry::File(name)
                });
            }
        }
        Ok(out)
    }

    fn read_file(&self, rel: &Path) -> io::Result<Vec<u8>> {
        self.files
            .borrow()
            .get(rel)
            .cloned()
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, rel.display().to_string()))
    }

    fn write_file(&self, rel: &Path, contents: &[u8]) -> io::Result<()> {
        if let Some(parent) = rel.parent() {
            if !parent.as_os_str().is_empty() {
                self.create_dir(parent)?;
            }
        }
        self.files
            .borrow_mut()
            .insert(rel.to_path_buf(), contents.to_vec());
        Ok(())
    }

    fn write_private(&self, rel: &Path, contents: &[u8]) -> io::Result<()> {
        self.write_file(rel, contents)
    }

    fn create_dir(&self, rel: &Path) -> io::Result<()> {
        if !rel.as_os_str().is_empty() {
            self.dirs.borrow_mut().insert(rel.to_path_buf());
        }
        Ok(())
    }

    fn remove_file(&self, rel: &Path) -> io::Result<()> {
        if self.files.borrow_mut().remove(rel).is_some() {
            Ok(())
        } else {
            Err(io::Error::new(io::ErrorKind::NotFound, rel.display().to_string()))
        }
    }

    fn remove_dir(&self, rel: &Path) -> io::Result<()> {
        // Empty iff no file or directory entry nests under `rel`.
        let rel = rel.to_path_buf();
        let files = self.files.borrow();
        let dirs = self.dirs.borrow();
        let occupied = files
            .keys()
            .any(|p| p.starts_with(&rel) && *p != rel)
            || dirs.iter().any(|p| p.starts_with(&rel) && *p != rel);
        if occupied {
            return Err(io::Error::new(
                io::ErrorKind::DirectoryNotEmpty,
                rel.display().to_string(),
            ));
        }
        drop(files);
        drop(dirs);
        let was = self.dirs.borrow_mut().remove(&rel);
        if was {
            Ok(())
        } else {
            Err(io::Error::new(io::ErrorKind::NotFound, rel.display().to_string()))
        }
    }

    fn kind(&self, rel: &Path) -> io::Result<Option<Kind>> {
        if self.files.borrow().contains_key(rel) {
            Ok(Some(Kind::File))
        } else if self.dirs.borrow().contains(rel) {
            Ok(Some(Kind::Dir))
        } else {
            Ok(None)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// cap-std must confine every access to the workspace root. This is the
    /// security boundary `MemWorkspace` cannot exercise, so it runs on real
    /// disk with real symlinks, mirroring teamtype's `can_not_read_outside_dir`.
    #[test]
    fn cap_workspace_cannot_escape_the_root() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("secret"), b"private").unwrap();

        let ws = CapWorkspace::open(root.path()).unwrap();

        // Inside the root is fine.
        ws.write_file(Path::new("inside.txt"), b"ok").unwrap();
        assert_eq!(ws.read_file(Path::new("inside.txt")).unwrap(), b"ok");

        // Absolute paths and `..` traversal are refused.
        assert!(
            ws.read_file(outside.path().join("secret").as_path())
                .is_err()
        );
        assert!(ws.write_file(Path::new("../escape.txt"), b"x").is_err());
        assert!(ws.read_file(Path::new("../../etc/passwd")).is_err());

        // A symlink inside the root pointing outside it does not let reads through.
        std::os::unix::fs::symlink(outside.path(), root.path().join("link")).unwrap();
        assert!(ws.read_file(Path::new("link/secret")).is_err());
    }
}
