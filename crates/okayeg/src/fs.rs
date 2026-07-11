//! A path-addressed view over the file tree.
//!
//! [`FileTree`] works in node identities; this module layers "which paths
//! exist and what is at them" on top, once, so every consumer resolves and
//! validates paths the same way. Paths use `/` as the separator regardless of
//! host, and the view is synchronous and local: it reads the doc you hold.
//!
//! It is read-plus-edit, never filesystem-faithful. Content edits stay on the
//! text handle from [`DocFs::text`], because flattening them to a byte write
//! would discard the merge granularity the CRDT exists for. A byte-level write
//! belongs in a backend that genuinely only has bytes, done there as a diff
//! against the doc.

use std::sync::Arc;

use loro::{ContainerID, ContainerType, LoroText, Subscription, TreeParentId};

use crate::Doc;
use crate::tree::{FileTree, NodeKind, TreeID};

/// Whether `name` can appear as one path component.
///
/// The single name-safety rule: nonempty, not `.` or `..`, and free of `/`
/// and NUL. A tree node whose name breaks it has no path; every reader here
/// skips it, and materialization backends refuse to write it.
pub fn valid_name(name: &str) -> bool {
    !name.is_empty() && name != "." && name != ".." && !name.contains(['/', '\0'])
}

/// Why a path operation failed. Variants mirror the errno a filesystem
/// syscall would raise for the same misuse.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FsError {
    /// A component of the path is not a valid name.
    InvalidPath,
    /// The path does not exist.
    NotFound,
    /// A non-directory sits where the path needs a directory.
    NotADirectory,
    /// The operation needs a file but the path is a directory or boundary.
    NotAFile,
    /// The destination already exists.
    AlreadyExists,
    /// The directory still has children.
    NotEmpty,
    /// The move would put a node inside its own subtree.
    InvalidMove,
}

impl std::fmt::Display for FsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            FsError::InvalidPath => "invalid path",
            FsError::NotFound => "no such file or directory",
            FsError::NotADirectory => "not a directory",
            FsError::NotAFile => "not a file",
            FsError::AlreadyExists => "already exists",
            FsError::NotEmpty => "directory not empty",
            FsError::InvalidMove => "cannot move a node into its own subtree",
        })
    }
}

impl std::error::Error for FsError {}

/// One directory entry: a node together with its name and kind.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Entry {
    pub node: TreeID,
    pub name: String,
    pub kind: Option<NodeKind>,
}

/// A change to the file tree, keyed by node.
///
/// Events carry no paths: a consumer that mirrors the tree (a mount's inode
/// table, an editor's buffer map) already knows each node's previous place,
/// and one that does not can ask [`DocFs::path_of`] for the current one.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Change {
    /// A node appeared.
    Created { node: TreeID },
    /// A node moved or was renamed; read its new parent and name from the doc.
    Moved { node: TreeID },
    /// A node was deleted.
    Removed { node: TreeID },
    /// A file node's content text changed.
    Content { node: TreeID },
}

impl Doc {
    /// The path-addressed view over this doc's file tree.
    pub fn fs(&self) -> DocFs<'_> {
        DocFs { doc: self }
    }
}

/// A path-addressed view over a [`Doc`]'s file tree.
///
/// Resolves paths against the tree on each call. If sibling names collide, a
/// path resolves to the first match in child order. Duplicates would stay
/// reachable by node through [`FileTree`].
pub struct DocFs<'a> {
    doc: &'a Doc,
}

impl DocFs<'_> {
    fn tree(&self) -> FileTree<'_> {
        self.doc.files()
    }

    /// Split a path into validated components. Empty components collapse, so
    /// `a//b` and `/a/b/` read as `a/b`; the empty path is the root.
    fn components(path: &str) -> Result<Vec<&str>, FsError> {
        let comps: Vec<&str> = path.split('/').filter(|c| !c.is_empty()).collect();

        if comps.iter().all(|c| valid_name(c)) {
            Ok(comps)
        } else {
            Err(FsError::InvalidPath)
        }
    }

    /// The child of `parent` (or a root, for `None`) with a valid `name`.
    fn child_named(&self, parent: Option<TreeID>, name: &str) -> Option<TreeID> {
        let tree = self.tree();
        let candidates = match parent {
            Some(p) => tree.children(p),
            None => tree.roots(),
        };

        candidates
            .into_iter()
            .find(|node| tree.name(*node).as_deref() == Some(name))
    }

    /// Resolve a path to its node. The root is not a node, so the empty path
    /// is `NotFound`.
    pub fn resolve(&self, path: &str) -> Result<TreeID, FsError> {
        let tree = self.tree();
        let mut current = None;

        for comp in Self::components(path)? {
            if current.is_some_and(|node| tree.kind(node) != Some(NodeKind::Dir)) {
                return Err(FsError::NotADirectory);
            }
            current = Some(self.child_named(current, comp).ok_or(FsError::NotFound)?);
        }
        current.ok_or(FsError::NotFound)
    }

    /// The entry at a path.
    pub fn stat(&self, path: &str) -> Result<Entry, FsError> {
        let node = self.resolve(path)?;
        Ok(self.entry(node))
    }

    fn entry(&self, node: TreeID) -> Entry {
        let tree = self.tree();
        Entry {
            node,
            name: tree.name(node).unwrap_or_default(),
            kind: tree.kind(node),
        }
    }

    /// A file's content as a string.
    pub fn read(&self, path: &str) -> Result<String, FsError> {
        Ok(self.text(path)?.to_string())
    }

    /// A file's content text handle, for reading or editing in place.
    pub fn text(&self, path: &str) -> Result<LoroText, FsError> {
        let node = self.resolve(path)?;
        self.tree().content(node).ok_or(FsError::NotAFile)
    }

    /// The entries of a directory, in child order. The empty path lists the
    /// top level. Entries with invalid names are omitted; they have no path.
    pub fn readdir(&self, path: &str) -> Result<Vec<Entry>, FsError> {
        let tree = self.tree();

        let children = if Self::components(path)?.is_empty() {
            tree.roots()
        } else {
            let node = self.resolve(path)?;
            if tree.kind(node) != Some(NodeKind::Dir) {
                return Err(FsError::NotADirectory);
            }
            tree.children(node)
        };

        Ok(children
            .into_iter()
            .map(|node| self.entry(node))
            .filter(|e| valid_name(&e.name))
            .collect())
    }

    /// Every reachable path in the tree, depth-first, paired with its entry.
    /// Subtrees under an invalid name are unreachable and do not appear.
    pub fn walk(&self) -> Vec<(String, Entry)> {
        let tree = self.tree();
        let mut stack: Vec<(TreeID, String)> = Vec::new();
        let mut out = Vec::new();

        for node in tree.roots() {
            if let Some(name) = tree.name(node).filter(|n| valid_name(n)) {
                stack.push((node, name));
            }
        }
        while let Some((node, path)) = stack.pop() {
            if tree.kind(node) == Some(NodeKind::Dir) {
                for child in tree.children(node) {
                    if let Some(name) = tree.name(child).filter(|n| valid_name(n)) {
                        stack.push((child, format!("{path}/{name}")));
                    }
                }
            }
            out.push((path.clone(), self.entry(node)));
        }
        out
    }

    /// The current path of a node, or `None` if the node is deleted or any
    /// name on the way up is invalid.
    pub fn path_of(&self, node: TreeID) -> Option<String> {
        let tree = self.tree();
        let inner = self.doc.inner().get_tree(FileTree::TREE);
        let mut parts = Vec::new();
        let mut current = node;

        loop {
            parts.push(tree.name(current).filter(|n| valid_name(n))?);
            match inner.parent(current)? {
                TreeParentId::Node(parent) => current = parent,
                TreeParentId::Root => break,
                TreeParentId::Deleted | TreeParentId::Unexist => return None,
            }
        }
        parts.reverse();
        Some(parts.join("/"))
    }

    /// Resolve everything but the last component to an existing directory.
    /// Returns the parent node (`None` for the root) and the final name.
    fn split_parent<'p>(&self, path: &'p str) -> Result<(Option<TreeID>, &'p str), FsError> {
        let comps = Self::components(path)?;
        let (name, parents) = comps.split_last().ok_or(FsError::InvalidPath)?;

        let tree = self.tree();
        let mut parent = None;
        for comp in parents {
            let node = self.child_named(parent, comp).ok_or(FsError::NotFound)?;
            if tree.kind(node) != Some(NodeKind::Dir) {
                return Err(FsError::NotADirectory);
            }
            parent = Some(node);
        }
        Ok((parent, name))
    }

    /// Create a file at a path whose parent directory already exists.
    pub fn create_file(&self, path: &str) -> Result<TreeID, FsError> {
        let (parent, name) = self.split_parent(path)?;

        if self.child_named(parent, name).is_some() {
            return Err(FsError::AlreadyExists);
        }
        Ok(self.tree().create_file(parent, name))
    }

    /// Create a directory at a path whose parent directory already exists.
    pub fn create_dir(&self, path: &str) -> Result<TreeID, FsError> {
        let (parent, name) = self.split_parent(path)?;

        if self.child_named(parent, name).is_some() {
            return Err(FsError::AlreadyExists);
        }
        Ok(self.tree().create_dir(parent, name))
    }

    /// Delete a non-directory at a path.
    pub fn remove_file(&self, path: &str) -> Result<(), FsError> {
        let node = self.resolve(path)?;
        let tree = self.tree();

        if tree.kind(node) == Some(NodeKind::Dir) {
            return Err(FsError::NotAFile);
        }
        tree.delete(node);
        Ok(())
    }

    /// Delete an empty directory at a path.
    pub fn remove_dir(&self, path: &str) -> Result<(), FsError> {
        let node = self.resolve(path)?;
        let tree = self.tree();

        if tree.kind(node) != Some(NodeKind::Dir) {
            return Err(FsError::NotADirectory);
        }
        if !tree.children(node).is_empty() {
            return Err(FsError::NotEmpty);
        }
        tree.delete(node);
        Ok(())
    }

    /// Move or rename a node. An existing destination is replaced when the
    /// kinds are compatible: a non-directory over a non-directory, or a
    /// directory over an empty directory.
    pub fn rename(&self, from: &str, to: &str) -> Result<(), FsError> {
        let node = self.resolve(from)?;
        let (parent, name) = self.split_parent(to)?;
        let tree = self.tree();

        let displaced = self.child_named(parent, name).filter(|d| *d != node);
        if let Some(dest) = displaced {
            let node_is_dir = tree.kind(node) == Some(NodeKind::Dir);
            match tree.kind(dest) {
                Some(NodeKind::Dir) if !node_is_dir => return Err(FsError::NotADirectory),
                Some(NodeKind::Dir) if !tree.children(dest).is_empty() => {
                    return Err(FsError::NotEmpty);
                }
                Some(NodeKind::Dir) => {}
                _ if node_is_dir => return Err(FsError::NotAFile),
                _ => {}
            }
        }

        // Move first: Loro refuses a move into the node's own subtree, and
        // failing here leaves the destination untouched.
        self.doc
            .inner()
            .get_tree(FileTree::TREE)
            .mov(node, parent)
            .map_err(|_| FsError::InvalidMove)?;

        if let Some(dest) = displaced {
            tree.delete(dest);
        }
        tree.rename(node, name);
        Ok(())
    }

    /// Watch the file tree for changes.
    ///
    /// The callback runs after each commit, import, or checkout that touched
    /// the tree, with one node-keyed [`Change`] per affected node. A freshly
    /// created node reports only `Created`, even though creation also writes
    /// its metadata and content.
    pub fn subscribe(&self, f: impl Fn(&[Change]) + Send + Sync + 'static) -> Subscription {
        let tree_id = ContainerID::new_root(FileTree::TREE, ContainerType::Tree);
        let filter = tree_id.clone();

        self.doc.inner().subscribe_root(Arc::new(move |event| {
            let mut changes: Vec<Change> = Vec::new();

            for diff in &event.events {
                let in_tree = *diff.target == filter
                    || diff.path.first().is_some_and(|(id, _)| *id == filter);
                if !in_tree {
                    continue;
                }

                match &diff.diff {
                    loro::event::Diff::Tree(tree_diff) => {
                        for item in &tree_diff.diff {
                            let node = item.target;
                            changes.push(match item.action {
                                loro::TreeExternalDiff::Create { .. } => Change::Created { node },
                                loro::TreeExternalDiff::Move { .. } => Change::Moved { node },
                                loro::TreeExternalDiff::Delete { .. } => Change::Removed { node },
                            });
                        }
                    }
                    loro::event::Diff::Map(map) => {
                        if map.updated.contains_key("name")
                            && let Some(node) = node_in_path(diff.path)
                        {
                            changes.push(Change::Moved { node });
                        }
                    }
                    loro::event::Diff::Text(_) => {
                        if let Some(node) = node_in_path(diff.path) {
                            changes.push(Change::Content { node });
                        }
                    }
                    _ => {}
                }
            }

            // Creation also fires metadata and content diffs for the new
            // node; merge those into the one Created.
            let created: Vec<TreeID> = changes
                .iter()
                .filter_map(|c| match c {
                    Change::Created { node } => Some(*node),
                    _ => None,
                })
                .collect();
            changes.retain(|c| match c {
                Change::Moved { node } | Change::Content { node } => !created.contains(node),
                _ => true,
            });
            changes.dedup();

            if !changes.is_empty() {
                f(&changes);
            }
        }))
    }
}

/// The tree node a nested container belongs to: the last node index on its
/// path from the root.
fn node_in_path(path: &[(ContainerID, loro::Index)]) -> Option<TreeID> {
    path.iter().rev().find_map(|(_, index)| match index {
        loro::Index::Node(node) => Some(*node),
        _ => None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Doc {
        let doc = Doc::new();
        let files = doc.files();

        let src = files.create_dir(None, "src");
        let main = files.create_file(Some(src), "main.rs");
        files
            .content(main)
            .unwrap()
            .insert(0, "fn main() {}")
            .unwrap();
        let readme = files.create_file(None, "README.md");
        files.content(readme).unwrap().insert(0, "gib eg").unwrap();

        doc.commit();
        doc
    }

    #[test]
    fn resolves_reads_and_stats_paths() {
        let doc = sample();
        let fs = doc.fs();

        assert_eq!(fs.read("src/main.rs").unwrap(), "fn main() {}");
        assert_eq!(fs.read("/src//main.rs/").unwrap(), "fn main() {}");
        assert_eq!(fs.stat("src").unwrap().kind, Some(NodeKind::Dir));
        assert_eq!(fs.read("missing.txt"), Err(FsError::NotFound));
        assert_eq!(fs.read("src"), Err(FsError::NotAFile));
        assert_eq!(fs.read("README.md/nope"), Err(FsError::NotADirectory));
        assert_eq!(fs.resolve("src/../etc"), Err(FsError::InvalidPath));
        assert_eq!(fs.resolve(""), Err(FsError::NotFound));
    }

    #[test]
    fn readdir_lists_and_walk_covers_every_path() {
        let doc = sample();
        let fs = doc.fs();

        let root: Vec<String> = fs
            .readdir("")
            .unwrap()
            .into_iter()
            .map(|e| e.name)
            .collect();
        assert!(root.contains(&"src".to_string()));
        assert!(root.contains(&"README.md".to_string()));
        assert_eq!(fs.readdir("README.md"), Err(FsError::NotADirectory));

        let mut paths: Vec<String> = fs.walk().into_iter().map(|(p, _)| p).collect();
        paths.sort();
        assert_eq!(paths, ["README.md", "src", "src/main.rs"]);
    }

    #[test]
    fn invalid_names_have_no_path() {
        let doc = sample();
        let files = doc.files();
        let bad = files.create_file(None, "../pwned");
        files.create_file(None, "a/b");
        doc.commit();
        let fs = doc.fs();

        let names: Vec<String> = fs
            .readdir("")
            .unwrap()
            .into_iter()
            .map(|e| e.name)
            .collect();
        assert_eq!(names.len(), 2, "invalid names listed: {names:?}");
        assert_eq!(fs.walk().len(), 3);
        assert_eq!(fs.path_of(bad), None);
    }

    #[test]
    fn creates_with_syscall_shaped_failures() {
        let doc = sample();
        let fs = doc.fs();

        let node = fs.create_file("src/lib.rs").unwrap();
        assert_eq!(fs.resolve("src/lib.rs").unwrap(), node);
        assert_eq!(fs.create_file("src/lib.rs"), Err(FsError::AlreadyExists));
        assert_eq!(fs.create_dir("src"), Err(FsError::AlreadyExists));
        assert_eq!(fs.create_file("no/such/parent.txt"), Err(FsError::NotFound));
        assert_eq!(
            fs.create_file("README.md/child"),
            Err(FsError::NotADirectory)
        );
        assert_eq!(fs.create_file(""), Err(FsError::InvalidPath));
    }

    #[test]
    fn removes_like_unlink_and_rmdir() {
        let doc = sample();
        let fs = doc.fs();

        assert_eq!(fs.remove_file("src"), Err(FsError::NotAFile));
        assert_eq!(fs.remove_dir("README.md"), Err(FsError::NotADirectory));
        assert_eq!(fs.remove_dir("src"), Err(FsError::NotEmpty));

        fs.remove_file("src/main.rs").unwrap();
        fs.remove_dir("src").unwrap();
        assert_eq!(fs.resolve("src"), Err(FsError::NotFound));
    }

    #[test]
    fn rename_moves_replaces_and_refuses_cycles() {
        let doc = sample();
        let fs = doc.fs();
        let main = fs.resolve("src/main.rs").unwrap();

        // A plain rename keeps the node.
        fs.rename("src/main.rs", "src/lib.rs").unwrap();
        assert_eq!(fs.resolve("src/lib.rs").unwrap(), main);

        // Moving over an existing file replaces it.
        fs.rename("src/lib.rs", "README.md").unwrap();
        assert_eq!(fs.resolve("README.md").unwrap(), main);
        assert_eq!(fs.read("README.md").unwrap(), "fn main() {}");

        // Kind mismatches and cycles are refused.
        assert_eq!(fs.rename("README.md", "src"), Err(FsError::NotADirectory));
        let docs = fs.create_dir("docs").unwrap();
        fs.create_file("docs/a.txt").unwrap();
        assert_eq!(fs.rename("src", "docs"), Err(FsError::NotEmpty));
        assert_eq!(fs.rename("docs", "README.md"), Err(FsError::NotAFile));
        assert_eq!(fs.rename("docs", "docs/inside"), Err(FsError::InvalidMove));
        assert_eq!(fs.resolve("docs").unwrap(), docs);

        // Directory over empty directory replaces.
        fs.remove_file("docs/a.txt").unwrap();
        fs.rename("src", "docs").unwrap();
        assert_eq!(fs.readdir("docs").unwrap(), []);
    }

    #[test]
    fn subscribe_reports_node_keyed_changes() {
        use std::sync::Mutex;

        let doc = sample();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let sink = seen.clone();
        let _sub = doc.fs().subscribe(move |changes| {
            sink.lock().unwrap().extend_from_slice(changes);
        });

        // A creation merges its metadata and content writes into one Created.
        let node = doc.fs().create_file("notes.txt").unwrap();
        doc.fs().text("notes.txt").unwrap().insert(0, "hi").unwrap();
        doc.commit();
        assert_eq!(
            seen.lock().unwrap().drain(..).collect::<Vec<_>>(),
            [Change::Created { node }]
        );

        // Content, rename, and removal each report against the same node.
        doc.fs().text("notes.txt").unwrap().insert(2, "!").unwrap();
        doc.commit();
        assert_eq!(
            seen.lock().unwrap().drain(..).collect::<Vec<_>>(),
            [Change::Content { node }]
        );

        doc.fs().rename("notes.txt", "src/notes.txt").unwrap();
        doc.commit();
        assert!(
            seen.lock()
                .unwrap()
                .drain(..)
                .collect::<Vec<_>>()
                .contains(&Change::Moved { node })
        );

        doc.fs().remove_file("src/notes.txt").unwrap();
        doc.commit();
        assert_eq!(
            seen.lock().unwrap().drain(..).collect::<Vec<_>>(),
            [Change::Removed { node }]
        );
    }

    #[test]
    fn subscribe_fires_on_import_too() {
        use std::sync::Mutex;

        let doc = sample();
        let peer = Doc::from_snapshot(&doc.snapshot().unwrap()).unwrap();
        let node = peer.fs().resolve("README.md").unwrap();
        peer.fs()
            .text("README.md")
            .unwrap()
            .insert(0, "pls ")
            .unwrap();
        peer.commit();

        let seen = Arc::new(Mutex::new(Vec::new()));
        let sink = seen.clone();
        let _sub = doc.fs().subscribe(move |changes| {
            sink.lock().unwrap().extend_from_slice(changes);
        });

        doc.import(&peer.updates_since(&doc.version()).unwrap())
            .unwrap();
        assert_eq!(*seen.lock().unwrap(), [Change::Content { node }]);
    }
}
