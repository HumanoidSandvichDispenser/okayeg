//! Okayeg: local-first, real-time, conflict-free file/document sync.
//!
//! Built on Loro (eg-walker CRDT).

mod comment;
mod doc;
mod fs;
mod presence;
mod sync;
mod tree;

pub use comment::{Comment, Comments};
pub use doc::Doc;
pub use fs::{Change, DocFs, Entry, FsError, valid_name};
pub use loro::awareness::{EphemeralEventTrigger, EphemeralStoreEvent};
pub use loro::{Frontiers, LoroValue, Subscription};
pub use presence::{Presence, PresenceError};
pub use sync::{Live, LiveSync, Msg, Perms, Step, Sync, SyncError};
pub use tree::{FileTree, NodeKind, TreeID};
