//! `eg`, the okayeg command line.
//!
//! For now it bridges a directory and an okayeg doc, so you can take a real
//! folder of text files into a doc and write it back out. All filesystem
//! access goes through [`Workspace`], which confines it to the workspace root.

mod ignore;
mod net;
mod trust;
mod watch;
mod workspace;

use std::fs;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use okayeg::{Doc, NodeKind, TreeID};

use ignore::Ignorer;
use workspace::{CapWorkspace, Entry, Workspace};

/// Where a repo keeps its private state, hidden under the served directory:
/// the node key now, the doc snapshot and trust set later. Never imported as
/// doc content.
const EG_DIR: &str = ".eg";

/// The okayeg command line.
#[derive(clap::Parser)]
#[command(name = "eg", version, about = "Sync a directory of text files as an okayeg doc.")]
struct Cli {
    /// Act as if run from <dir>, used by serve/pull/join/id/trust
    #[arg(short = 'C', value_name = "dir", global = true)]
    dir: Option<PathBuf>,

    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(clap::Subcommand)]
enum Cmd {
    /// Take a directory into a doc snapshot.
    Snapshot { dir: PathBuf, out: PathBuf },
    /// Write a doc snapshot back to a directory.
    Restore { input: PathBuf, dir: PathBuf },
    /// Keep a doc snapshot in sync with a directory.
    Watch { dir: PathBuf, out: PathBuf },
    /// Serve this repo over iroh, live, for peers to sync.
    Serve,
    /// Pull current state from a peer, then exit.
    Pull { peer: String },
    /// Clone if empty, then sync live with a peer.
    Join { peer: String },
    /// Print this repo's endpoint id.
    Id,
    /// Grant a peer access (default both).
    Trust {
        peer: String,
        /// Any of `pull` / `push`; empty grants both.
        flags: Vec<String>,
    },
}

fn main() -> ExitCode {
    let cli = <Cli as clap::Parser>::parse();
    let dir = cli.dir.as_deref();
    run(match cli.cmd {
        Cmd::Snapshot { dir, out } => snapshot(&dir, &out),
        Cmd::Restore { input, dir } => restore(&input, &dir),
        Cmd::Watch { dir, out } => watch::watch(&dir, &out),
        Cmd::Serve => with_repo(dir, net::serve),
        Cmd::Pull { peer } => with_fresh(dir, |d| net::pull(d, &peer)),
        Cmd::Join { peer } => with_fresh(dir, |d| net::join(d, &peer)),
        Cmd::Id => with_repo(dir, net::id),
        Cmd::Trust { peer, flags } => with_repo(dir, |d| net::trust(d, &peer, &flags)),
    })
}

/// Resolve the enclosing repo, then run `f` against it.
///
/// Starts at `cdir` (or the cwd) and walks up to the nearest `.eg/`, so these
/// commands work from anywhere inside a repo. With none up the tree, the start
/// directory itself is used (and `.eg/` created there on first use).
fn with_repo<F>(cdir: Option<&Path>, f: F) -> std::io::Result<()>
where
    F: FnOnce(&Path) -> std::io::Result<()>,
{
    let start = abs(cdir)?;
    let dir = enclosing_repo(&start).unwrap_or(start);
    f(&dir)
}

/// Resolve a directory to clone into, then run `f` against it.
///
/// Unlike [`with_repo`] this does not retarget to a parent: cloning into a spot
/// nested inside an existing repo is refused, so a stray `join` from within a
/// repo cannot graft one tree onto another. Acting on the repo root itself is
/// allowed, which is how a `join` resumes a live session.
fn with_fresh<F>(cdir: Option<&Path>, f: F) -> std::io::Result<()>
where
    F: FnOnce(&Path) -> std::io::Result<()>,
{
    let start = abs(cdir)?;
    if let Some(root) = enclosing_repo(&start) {
        if root != start {
            return Err(std::io::Error::new(
                std::io::ErrorKind::AlreadyExists,
                format!(
                    "inside the repo at {}; clone into a fresh directory",
                    root.display()
                ),
            ));
        }
    }
    f(&start)
}

/// Absolutize `cdir` (or the cwd), canonicalizing when the path already exists.
fn abs(cdir: Option<&Path>) -> std::io::Result<PathBuf> {
    let start = match cdir {
        Some(dir) if dir.is_absolute() => dir.to_path_buf(),
        Some(dir) => std::env::current_dir()?.join(dir),
        None => std::env::current_dir()?,
    };
    Ok(start.canonicalize().unwrap_or(start))
}

/// The nearest directory at or above `dir` that holds `.eg/`, if any.
fn enclosing_repo(dir: &Path) -> Option<PathBuf> {
    let mut cur = Some(dir);
    while let Some(c) = cur {
        if c.join(EG_DIR).is_dir() {
            return Some(c.to_path_buf());
        }
        cur = c.parent();
    }
    None
}

fn run(result: std::io::Result<()>) -> ExitCode {
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("eg: {err}");
            ExitCode::FAILURE
        }
    }
}

/// Walk the directory at `dir` into a doc and write the snapshot to `out`.
fn snapshot(dir: &Path, out: &Path) -> std::io::Result<()> {
    let ws = CapWorkspace::open(dir)?;
    let doc = Doc::new();
    let files = import_tree(&ws, &doc)?;
    fs::write(out, doc.snapshot().map_err(to_io)?)?;
    println!(
        "snapshot: {files} file(s) from {} -> {}",
        dir.display(),
        out.display()
    );
    Ok(())
}

/// Load the snapshot at `file` and write its tree into the directory `dir`.
fn restore(file: &Path, dir: &Path) -> std::io::Result<()> {
    fs::create_dir_all(dir)?;
    let ws = CapWorkspace::open(dir)?;
    let doc = Doc::from_snapshot(&fs::read(file)?).map_err(to_io)?;
    let files = export_tree(&doc, &ws)?;
    println!(
        "restore: {files} file(s) from {} -> {}",
        file.display(),
        dir.display()
    );
    Ok(())
}

/// Read every file under the workspace into the doc's tree. Returns the file count.
fn import_tree(ws: &dyn Workspace, doc: &Doc) -> std::io::Result<usize> {
    let ignorer = Ignorer::load(ws)?;
    let mut files = 0;
    import_dir(ws, doc, &ignorer, Path::new(""), None, &mut files)?;
    doc.commit();
    Ok(files)
}

fn import_dir(
    ws: &dyn Workspace,
    doc: &Doc,
    ignorer: &Ignorer,
    rel: &Path,
    parent: Option<TreeID>,
    files: &mut usize,
) -> std::io::Result<()> {
    let tree = doc.files();
    let mut entries = ws.read_dir(rel)?;
    entries.sort_by(|a, b| name_of(a).cmp(name_of(b)));

    for entry in entries {
        // `.eg/` holds the repo's own state (key, and later the snapshot); it is
        // metadata about the doc, not content of it, so never walk into it.
        if rel.as_os_str().is_empty() && name_of(&entry) == EG_DIR {
            continue;
        }
        let path = rel.join(name_of(&entry));
        // Checked after `.eg/`, so `.eg/ignore` adds to that skip, never undoes it.
        if ignorer.should_ignore(&path, matches!(entry, Entry::Dir(_))) {
            continue;
        }
        match entry {
            Entry::Dir(name) => {
                let node = tree.create_dir(parent, &name);
                import_dir(ws, doc, ignorer, &rel.join(&name), Some(node), files)?;
            }
            Entry::File(name) => {
                let bytes = ws.read_file(&rel.join(&name))?;
                // Only text files for now; skip what isn't valid UTF-8.
                match String::from_utf8(bytes) {
                    Ok(text) => {
                        let node = tree.create_file(parent, &name);
                        if let Some(content) = tree.content(node) {
                            content.insert(0, &text).map_err(to_io)?;
                        }
                        *files += 1;
                    }
                    Err(_) => eprintln!(
                        "{}: skipping non-text file {}",
                        std::env::current_exe()
                            .map(|p| p.display().to_string())
                            .unwrap_or_else(|_| "eg".to_string()),
                        rel.join(&name).display()
                    ),
                }
            }
        }
    }
    Ok(())
}

/// Write the doc's tree out into the workspace. Returns the file count.
fn export_tree(doc: &Doc, ws: &dyn Workspace) -> std::io::Result<usize> {
    let ignorer = Ignorer::load(ws)?;
    let mut files = 0;
    for node in doc.files().roots() {
        // do not materialize any files in .eg
        if doc.files().name(node).as_deref() == Some(EG_DIR) {
            continue;
        }
        export_node(doc, ws, &ignorer, node, Path::new(""), &mut files)?;
    }
    Ok(files)
}

fn export_node(
    doc: &Doc,
    ws: &dyn Workspace,
    ignorer: &Ignorer,
    node: TreeID,
    parent: &Path,
    files: &mut usize,
) -> std::io::Result<()> {
    let tree = doc.files();
    let Some(name) = tree.name(node) else {
        return Ok(());
    };
    let rel = parent.join(&name);
    let kind = tree.kind(node);
    // Same skip set as import; `.eg/` is already handled by the caller.
    if ignorer.should_ignore(&rel, matches!(kind, Some(NodeKind::Dir))) {
        return Ok(());
    }
    match kind {
        Some(NodeKind::Dir) => {
            ws.create_dir(&rel)?;
            for child in tree.children(node) {
                export_node(doc, ws, ignorer, child, &rel, files)?;
            }
        }
        Some(NodeKind::File) => {
            let text = tree.content(node).map(|c| c.to_string()).unwrap_or_default();
            ws.write_file(&rel, text.as_bytes())?;
            *files += 1;
        }
        // Boundaries point at another doc; nothing to write inline yet.
        Some(NodeKind::Boundary) | None => {}
    }
    Ok(())
}

fn name_of(entry: &Entry) -> &str {
    match entry {
        Entry::Dir(name) | Entry::File(name) => name,
    }
}

fn to_io<E: std::fmt::Display>(err: E) -> std::io::Error {
    std::io::Error::other(err.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use workspace::MemWorkspace;

    #[test]
    fn round_trips_through_an_in_memory_workspace() {
        // Build a tree in memory: README.md, src/main.rs, src/sub/deep.txt.
        let src = MemWorkspace::new();
        src.write_file(Path::new("README.md"), b"gib eg\n").unwrap();
        src.write_file(Path::new("src/main.rs"), b"fn main() {}\n").unwrap();
        src.write_file(Path::new("src/nested/deep.txt"), b"dark fantasies\n").unwrap();

        // Snapshot it into a doc, then restore into a fresh workspace.
        let doc = Doc::new();
        let imported = import_tree(&src, &doc).unwrap();
        assert_eq!(imported, 3);

        let bytes = doc.snapshot().unwrap();
        let restored_doc = Doc::from_snapshot(&bytes).unwrap();
        let dst = MemWorkspace::new();
        let exported = export_tree(&restored_doc, &dst).unwrap();
        assert_eq!(exported, 3);

        // The restored files should match the originals.
        for path in ["README.md", "src/main.rs", "src/nested/deep.txt"] {
            assert_eq!(
                dst.read_file(Path::new(path)).unwrap(),
                src.read_file(Path::new(path)).unwrap(),
                "{path}"
            );
        }
    }

    #[test]
    fn export_never_materializes_an_eg_dir() {
        let doc = Doc::new();
        let tree = doc.files();
        let eg = tree.create_dir(None, ".eg");
        let key = tree.create_file(Some(eg), "key");
        if let Some(content) = tree.content(key) {
            content.insert(0, "attacker-key").unwrap();
        }

        // a normal file that shouldn't be ignored
        let readme = tree.create_file(None, "README.md");
        if let Some(content) = tree.content(readme) {
            content.insert(0, "ok").unwrap();
        }

        doc.commit();

        let ws = MemWorkspace::new();
        let files = export_tree(&doc, &ws).unwrap();

        assert_eq!(files, 1, "only README.md should be written, not .eg/key");
        assert_eq!(ws.read_file(Path::new("README.md")).unwrap(), b"ok");
        assert!(
            ws.read_file(Path::new(".eg/key")).is_err(),
            ".eg must never be materialized from doc content"
        );
    }

    #[test]
    fn ignore_skips_imports_and_prunes_dirs() {
        let src = MemWorkspace::new();
        src.write_file(Path::new(".eg/ignore"), b"secrets.env\ntarget/\n").unwrap();
        src.write_file(Path::new("README.md"), b"ok\n").unwrap();
        src.write_file(Path::new("secrets.env"), b"hunter2\n").unwrap();
        src.write_file(Path::new("target/build.o"), b"junk\n").unwrap();

        let doc = Doc::new();
        let imported = import_tree(&src, &doc).unwrap();
        assert_eq!(imported, 1, "only README.md should be imported");

        // And the ignored paths must not have been materialized on export either.
        let dst = MemWorkspace::new();
        dst.write_file(Path::new(".eg/ignore"), b"secrets.env\ntarget/\n").unwrap();
        let exported = export_tree(&doc, &dst).unwrap();
        assert_eq!(exported, 1);
        assert_eq!(dst.read_file(Path::new("README.md")).unwrap(), b"ok\n");
        assert!(dst.read_file(Path::new("secrets.env")).is_err());
    }
}
