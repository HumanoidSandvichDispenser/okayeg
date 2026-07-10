//! The file tree held in a doc.
//!
//! A doc holds one [`LoroTree`](loro::LoroTree) of nodes. Each node is a file
//! (it owns a `Text` of content), a directory (it owns children), or a boundary
//! (it points at another doc, the split point). A node's identity is its
//! [`TreeID`], which stays fixed across moves and renames.

pub use loro::TreeID;

use loro::{ExportMode, Frontiers, LoroMap, LoroText, UpdateOptions};

use crate::Doc;

/// What a tree node is.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum NodeKind {
    /// A file: owns a `content` text.
    File,
    /// A directory: owns children.
    Dir,
    /// A boundary: owns a `ref` to another doc, where its subtree lives.
    Boundary,
}

impl NodeKind {
    fn as_str(self) -> &'static str {
        match self {
            NodeKind::File => "file",
            NodeKind::Dir => "dir",
            NodeKind::Boundary => "boundary",
        }
    }

    fn from_str(s: &str) -> Option<Self> {
        match s {
            "file" => Some(NodeKind::File),
            "dir" => Some(NodeKind::Dir),
            "boundary" => Some(NodeKind::Boundary),
            _ => None,
        }
    }
}

impl Doc {
    /// The file tree held in this doc.
    pub fn files(&self) -> FileTree<'_> {
        FileTree { doc: self }
    }
}

/// A view over the file tree held in a [`Doc`].
///
/// Cheap to construct; it borrows the doc and reads or writes the tree
/// container on each call. Edits apply to the local copy and are folded into
/// history at the doc's next [`commit`](Doc::commit).
pub struct FileTree<'a> {
    doc: &'a Doc,
}

impl FileTree<'_> {
    /// The name of the tree container holding the file tree.
    pub(crate) const TREE: &'static str = "files";

    fn tree(&self) -> loro::LoroTree {
        self.doc.inner().get_tree(Self::TREE)
    }

    fn meta(&self, node: TreeID) -> Option<LoroMap> {
        self.tree().get_meta(node).ok()
    }

    /// Create a file under `parent` (or at the top level if `None`).
    pub fn create_file(&self, parent: Option<TreeID>, name: &str) -> TreeID {
        let node = self.create_node(parent, name, NodeKind::File);
        let meta = self.meta(node).expect("new node has meta");
        meta.insert_container("content", LoroText::new())
            .expect("insert content text");
        node
    }

    /// Create a directory under `parent` (or at the top level if `None`).
    pub fn create_dir(&self, parent: Option<TreeID>, name: &str) -> TreeID {
        self.create_node(parent, name, NodeKind::Dir)
    }

    /// Create a boundary under `parent` pointing at another doc.
    pub fn create_boundary(
        &self,
        parent: Option<TreeID>,
        name: &str,
        reference: &str,
    ) -> TreeID {
        // currently a placeholder
        let node = self.create_node(parent, name, NodeKind::Boundary);
        let meta = self.meta(node).expect("new node has meta");
        meta.insert("ref", reference).expect("insert ref");
        node
    }

    fn create_node(&self, parent: Option<TreeID>, name: &str, kind: NodeKind) -> TreeID {
        let tree = self.tree();
        let node = tree.create(parent).expect("tree create");
        let meta = tree.get_meta(node).expect("new node has meta");
        meta.insert("name", name).expect("insert name");
        meta.insert("kind", kind.as_str()).expect("insert kind");
        node
    }

    /// Move a node under a new parent (or to the top level if `None`).
    pub fn mov(&self, node: TreeID, new_parent: Option<TreeID>) {
        self.tree().mov(node, new_parent).expect("tree move");
    }

    /// Rename a node.
    pub fn rename(&self, node: TreeID, name: &str) {
        if let Some(meta) = self.meta(node) {
            meta.insert("name", name).expect("rename");
        }
    }

    /// Delete a node and its subtree.
    pub fn delete(&self, node: TreeID) {
        self.tree().delete(node).expect("tree delete");
    }

    /// The top-level nodes.
    pub fn roots(&self) -> Vec<TreeID> {
        self.tree().roots()
    }

    /// The children of a node, in order.
    pub fn children(&self, parent: TreeID) -> Vec<TreeID> {
        self.tree().children(Some(parent)).unwrap_or_default()
    }

    /// What a node is.
    pub fn kind(&self, node: TreeID) -> Option<NodeKind> {
        NodeKind::from_str(&self.string_field(node, "kind")?)
    }

    /// A node's name.
    pub fn name(&self, node: TreeID) -> Option<String> {
        self.string_field(node, "name")
    }

    /// A file node's content text. `None` for directories and boundaries.
    pub fn content(&self, node: TreeID) -> Option<LoroText> {
        self.meta(node)?
            .get("content")?
            .into_container()
            .ok()?
            .into_text()
            .ok()
    }

    /// Set a file node's content to `text`, applying the minimal diff.
    ///
    /// Loro diffs the new text against the current content and records only the
    /// real change, so writing back identical text is a no-op. Returns `false`
    /// if the node is not a file. This is how the filesystem bridge folds an
    /// edited file in without echoing its own writes back out.
    pub fn set_content(&self, node: TreeID, text: &str) -> bool {
        match self.content(node) {
            Some(content) => {
                let _ = content.update(text, UpdateOptions::default());
                true
            }
            None => false,
        }
    }

    /// Set a file node's content to `text`, diffing against the doc as of
    /// `base` instead of its current state.
    ///
    /// This is the ingest primitive for external byte-level edits under
    /// concurrency. A file on disk only carries final state, so turning it into
    /// ops needs a three-way merge: the diff must be computed against the
    /// content this text had when the file was last reconciled (`base`), and
    /// any ops that landed since must survive. [`set_content`](Self::set_content)
    /// diffs against the live state, which silently deletes concurrent peer
    /// edits the file never saw.
    ///
    /// The edit is made on a fork of the doc pinned at `base` and merged back
    /// through an import, so Loro reconciles it with concurrent ops by CRDT
    /// causality rather than by byte positions. Returns `false` when the edit
    /// cannot be expressed at `base` (the node was not a file there, or `base`
    /// is not in this doc's history); the caller decides how to fall back.
    pub fn set_content_at(&self, node: TreeID, text: &str, base: &Frontiers) -> bool {
        let doc = self.doc.inner();
        let Ok(fork) = doc.fork_at(base) else {
            return false;
        };

        let Some(content) = fork
            .get_tree(Self::TREE)
            .get_meta(node)
            .ok()
            .and_then(|meta| meta.get("content"))
            .and_then(|v| v.into_container().ok())
            .and_then(|c| c.into_text().ok())
        else {
            return false;
        };

        if content.update(text, UpdateOptions::default()).is_err() {
            return false;
        }
        fork.commit();

        let Ok(updates) = fork.export(ExportMode::updates(&doc.state_vv())) else {
            return false;
        };
        doc.import(&updates).is_ok()
    }

    /// A boundary node's reference. `None` for files and directories.
    pub fn reference(&self, node: TreeID) -> Option<String> {
        self.string_field(node, "ref")
    }

    fn string_field(&self, node: TreeID, key: &str) -> Option<String> {
        let value = self.meta(node)?.get(key)?;
        Some(value.as_value()?.as_string()?.to_string())
    }

    /// Walk the subtrees under `roots` depth-first, each node paired with its
    /// kind. Descends into directories only; files, boundaries, and nodes with
    /// no metadata are leaves. Pass a filtered `roots` to leave out subtrees
    /// such as `.eg/`.
    pub fn walk(
        &self,
        roots: impl IntoIterator<Item = TreeID>,
    ) -> Vec<(TreeID, Option<NodeKind>)> {
        let mut stack: Vec<TreeID> = roots.into_iter().collect();
        let mut nodes = Vec::new();
        while let Some(node) = stack.pop() {
            let kind = self.kind(node);
            if kind == Some(NodeKind::Dir) {
                stack.extend(self.children(node));
            }
            nodes.push((node, kind));
        }
        nodes
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_file_with_content() {
        let doc = Doc::new();
        let files = doc.files();
        let node = files.create_file(None, "notes.txt");

        assert_eq!(files.kind(node), Some(NodeKind::File));
        assert_eq!(files.name(node).as_deref(), Some("notes.txt"));

        files.content(node).unwrap().insert(0, "hello").unwrap();
        assert_eq!(files.content(node).unwrap().to_string(), "hello");
    }

    #[test]
    fn nest_and_move_keeps_identity() {
        let doc = Doc::new();
        let files = doc.files();
        let dir = files.create_dir(None, "src");
        let other = files.create_dir(None, "docs");
        let file = files.create_file(Some(dir), "lib.rs");

        assert_eq!(files.children(dir), vec![file]);

        files.mov(file, Some(other));
        assert_eq!(files.children(dir), vec![]);
        assert_eq!(files.children(other), vec![file]);
    }

    #[test]
    fn rename_preserves_node() {
        let doc = Doc::new();
        let files = doc.files();
        let node = files.create_file(None, "draft.txt");
        files.rename(node, "final.txt");

        assert_eq!(files.name(node).as_deref(), Some("final.txt"));
        assert_eq!(files.kind(node), Some(NodeKind::File));
    }

    #[test]
    fn set_content_at_preserves_concurrent_peer_edits() {
        let doc = Doc::new();
        let files = doc.files();
        let node = files.create_file(None, "notes.txt");
        files.content(node).unwrap().insert(0, "hello").unwrap();
        doc.commit();
        let base = doc.frontiers();

        // A peer edit lands after the base was taken.
        let peer = Doc::from_snapshot(&doc.snapshot().unwrap()).unwrap();
        peer.files().content(node).unwrap().insert(5, "!").unwrap();
        peer.commit();
        doc.import(&peer.updates_since(&doc.version()).unwrap()).unwrap();
        assert_eq!(files.content(node).unwrap().to_string(), "hello!");

        // An external edit made against the base ("hello") must not delete it.
        assert!(files.set_content_at(node, "hello world", &base));
        let merged = files.content(node).unwrap().to_string();
        assert!(merged.contains("world"), "local edit lost: {merged:?}");
        assert!(merged.contains('!'), "peer edit lost: {merged:?}");
    }

    #[test]
    fn set_content_at_refuses_an_unknown_base() {
        let doc = Doc::new();
        let files = doc.files();
        let node = files.create_file(None, "a.txt");
        doc.commit();

        let other = Doc::new();
        other.files().create_file(None, "b.txt");
        other.commit();

        assert!(!files.set_content_at(node, "x", &other.frontiers()));
    }

    #[test]
    fn boundary_carries_a_reference() {
        let doc = Doc::new();
        let files = doc.files();
        let node = files.create_boundary(None, "vendor", "doc:abc123");

        assert_eq!(files.kind(node), Some(NodeKind::Boundary));
        assert_eq!(files.reference(node).as_deref(), Some("doc:abc123"));
        assert!(files.content(node).is_none());
    }
}
