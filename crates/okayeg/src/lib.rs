//! Okayeg: local-first, real-time, conflict-free file/document sync.
//!
//! Built on Loro (eg-walker CRDT).

mod doc;
mod tree;

pub use doc::Doc;
pub use tree::{FileTree, NodeKind, TreeID};
