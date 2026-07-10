//! Okayeg: local-first, real-time, conflict-free file/document sync.
//!
//! Built on Loro (eg-walker CRDT).

mod comment;
mod doc;
mod fs;
mod sync;
mod tree;

pub use comment::{Comment, Comments};
pub use fs::{valid_name, Change, DocFs, Entry, FsError};
pub use loro::{Frontiers, Subscription};
pub use doc::Doc;
pub use sync::{Live, LiveSync, Msg, Perms, Step, Sync, SyncError};
pub use tree::{FileTree, NodeKind, TreeID};
