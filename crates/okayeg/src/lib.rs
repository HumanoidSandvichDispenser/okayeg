//! Okayeg: local-first, real-time, conflict-free file/document sync.
//!
//! Built on Loro (eg-walker CRDT).

mod comment;
mod doc;
mod sync;
mod tree;

pub use comment::{Comment, Comments};
pub use loro::Frontiers;
pub use doc::Doc;
pub use sync::{Live, LiveSync, Msg, Perms, Step, Sync, SyncError};
pub use tree::{FileTree, NodeKind, TreeID};
