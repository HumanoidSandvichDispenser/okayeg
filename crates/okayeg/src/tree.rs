//! The file tree held in a doc.
//!
//! A doc holds one [`LoroTree`](loro::LoroTree) of nodes. Each node is a file
//! (it owns a `Text` of content), a directory (it owns children), or a boundary
//! (it points at another doc, the split point). A node's identity is its
//! [`TreeID`], which stays fixed across moves and renames.

pub use loro::TreeID;

use loro::{LoroMap, LoroText, UpdateOptions};

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
    const TREE: &'static str = "files";

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

    /// A boundary node's reference. `None` for files and directories.
    pub fn reference(&self, node: TreeID) -> Option<String> {
        self.string_field(node, "ref")
    }

    fn string_field(&self, node: TreeID, key: &str) -> Option<String> {
        let value = self.meta(node)?.get(key)?;
        Some(value.as_value()?.as_string()?.to_string())
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
    fn boundary_carries_a_reference() {
        let doc = Doc::new();
        let files = doc.files();
        let node = files.create_boundary(None, "vendor", "doc:abc123");

        assert_eq!(files.kind(node), Some(NodeKind::Boundary));
        assert_eq!(files.reference(node).as_deref(), Some("doc:abc123"));
        assert!(files.content(node).is_none());
    }
}
