//! Okayeg: local-first, real-time, conflict-free file/document sync.
//!
//! Built on Loro (eg-walker CRDT).

mod doc;
mod sync;
mod tree;

pub use doc::Doc;
pub use sync::{Msg, Perms, Step, Sync, SyncError};
pub use tree::{FileTree, NodeKind, TreeID};
