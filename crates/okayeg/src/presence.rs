//! Ephemeral state beside the doc: who is here, where their cursor is.
//!
//! [`Presence`] wraps Loro's `EphemeralStore`, a last-write-wins key-value
//! store with per-key timestamps and timeout expiry. Nothing here persists or
//! merges into the doc; an entry that stops being refreshed ages out. Updates
//! travel as opaque bytes in [`Msg::Ephemeral`](crate::Msg::Ephemeral) frames
//! on the same connection as sync: local changes flow out through
//! [`subscribe_local_updates`](Presence::subscribe_local_updates), remote
//! frames land through [`apply_from`](Presence::apply_from), which drops
//! entries under keys the sender does not own.

use std::collections::HashMap;

use loro::awareness::{EphemeralStore, EphemeralSubscriber, LocalEphemeralCallback};
use loro::{LoroValue, Subscription};

mod wire;
use wire::Entry;

/// A presence update did not parse, or Loro refused it.
#[derive(Debug)]
pub enum PresenceError {
    /// The payload did not decode, nested too deep, or carried a value kind
    /// presence does not allow.
    Malformed,
}

impl std::fmt::Display for PresenceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PresenceError::Malformed => write!(f, "malformed presence update"),
        }
    }
}

impl std::error::Error for PresenceError {}

/// The ephemeral state shared with peers during a live session.
///
/// Cloning hands out another handle to the same store.
#[derive(Debug, Clone)]
pub struct Presence {
    store: EphemeralStore,
}

impl Presence {
    /// A store whose entries expire `timeout_ms` after their last update.
    pub fn new(timeout_ms: i64) -> Self {
        Self {
            store: EphemeralStore::new(timeout_ms),
        }
    }

    /// Set `key` to `value`, notifying local-update subscribers with the
    /// encoded change.
    pub fn set(&self, key: &str, value: impl Into<LoroValue>) {
        self.store.set(key, value);
    }

    /// Delete `key`. The removal encodes and spreads like any other update.
    pub fn delete(&self, key: &str) {
        self.store.delete(key);
    }

    pub fn get(&self, key: &str) -> Option<LoroValue> {
        self.store.get(key)
    }

    /// Every live entry.
    pub fn all(&self) -> HashMap<String, LoroValue> {
        self.store.get_all_states().into_iter().collect()
    }

    /// Every live entry, encoded for the wire. Expired entries are omitted.
    pub fn encode_all(&self) -> Vec<u8> {
        self.store.encode_all()
    }

    /// Apply a peer's encoded update, keeping only entries whose key passes `allowed`. Returns the
    /// bytes that make up the scoped update, or `None` if nothing was kept.
    pub fn apply_from(
        &self,
        bytes: &[u8],
        allowed: impl Fn(&str) -> bool,
    ) -> Result<Option<Vec<u8>>, PresenceError> {
        let entries: Vec<Entry> =
            postcard::from_bytes(bytes).map_err(|_| PresenceError::Malformed)?;
        let total = entries.len();

        let kept: Vec<Entry> = entries.into_iter().filter(|e| allowed(&e.key)).collect();
        if kept.is_empty() {
            return Ok(None);
        }

        // nothing dropped: the original bytes are already what we would emit
        let sane = if kept.len() == total {
            bytes.to_vec()
        } else {
            postcard::to_allocvec(&kept).map_err(|_| PresenceError::Malformed)?
        };

        self.store
            .apply(&sane)
            .map_err(|_| PresenceError::Malformed)?;
        Ok(Some(sane))
    }

    /// Returns true if `key` is `owner` or a subkey under `owner/`.
    pub fn owns_key(owner: &str, key: &str) -> bool {
        key == owner || key.strip_prefix(owner).is_some_and(|r| r.starts_with('/'))
    }

    /// Apply a peer's encoded updates, keeping only entries `owner` may write:
    /// the key `owner` itself and any subkey under `owner/`. Returns the bytes
    /// that make up the scoped update, or `None` if nothing was kept. If
    /// `owner` is `None`, all keys are kept.
    pub fn apply_owned(
        &self,
        bytes: &[u8],
        owner: Option<&str>,
    ) -> Result<Option<Vec<u8>>, PresenceError> {
        self.apply_from(bytes, |k| match owner {
            Some(owner) => Self::owns_key(owner, k),
            None => true,
        })
    }

    /// Remove outdated entries. See [`EphemeralStore::remove_outdated`].
    pub fn remove_outdated(&self) {
        self.store.remove_outdated();
    }

    /// Watch the merged store: added, updated and removed keys, whether the
    /// change was local, imported or a timeout.
    pub fn subscribe(&self, callback: EphemeralSubscriber) -> Subscription {
        self.store.subscribe(callback)
    }

    /// Watch local changes as encoded bytes.
    pub fn subscribe_local_updates(&self, callback: LocalEphemeralCallback) -> Subscription {
        self.store.subscribe_local_updates(callback)
    }

    pub const CURSOR: &'static str = "cursor";

    fn cursor_key(ns: &str) -> String {
        format!("{}/{}", ns, Self::CURSOR)
    }

    pub fn set_cursor(&self, ns: &str, file: &str, anchor: &[u8], head: &[u8]) {
        let value = LoroValue::Map(
            [
                ("file".to_string(), LoroValue::from(file)),
                ("anchor".to_string(), LoroValue::from(anchor.to_vec())),
                ("head".to_string(), LoroValue::from(head.to_vec())),
            ]
            .into_iter()
            .collect(),
        );

        self.set(&Self::cursor_key(ns), value);
    }

    pub fn get_cursor(&self, ns: &str) -> Option<(String, Vec<u8>, Vec<u8>)> {
        decode_cursor(self.get(&Self::cursor_key(ns))?)
    }

    /// Clear this peer's cursor entry.
    pub fn clear_cursor(&self, ns: &str) {
        self.delete(&Self::cursor_key(ns));
    }

    /// Every live cursor, as `(ns, file, anchor, head)`. The anchor and head
    /// are still encoded stable cursors; the caller resolves them against its
    /// own doc.
    pub fn cursors(&self) -> Vec<(String, String, Vec<u8>, Vec<u8>)> {
        let suffix = format!("/{}", Self::CURSOR);
        self.all()
            .into_iter()
            .filter_map(|(key, value)| {
                let ns = key.strip_suffix(&suffix)?.to_string();
                let (file, anchor, head) = decode_cursor(value)?;
                Some((ns, file, anchor, head))
            })
            .collect()
    }
}

/// Unpack a cursor entry's map value into `(file, anchor, head)`.
fn decode_cursor(value: LoroValue) -> Option<(String, Vec<u8>, Vec<u8>)> {
    let LoroValue::Map(map) = value else {
        return None;
    };
    let file = map.get("file")?.as_string()?.to_string();
    let anchor = map.get("anchor")?.as_binary()?.to_vec();
    let head = map.get("head")?.as_binary()?.to_vec();
    Some((file, anchor, head))
}

#[cfg(test)]
mod tests {
    use super::wire::{MAX_DEPTH, Value};
    use super::*;
    use std::sync::{Arc, Mutex};

    const TIMEOUT: i64 = 30_000;

    fn value(name: &str) -> LoroValue {
        let mut m = std::collections::HashMap::new();
        m.insert("name".to_string(), LoroValue::from(name));
        m.insert(
            "tags".to_string(),
            LoroValue::from(vec![LoroValue::from(1i64)]),
        );
        LoroValue::from(m)
    }

    /// The mirror decode/re-encode must reproduce Loro's own bytes exactly, so
    /// a Loro encoding change breaks here instead of corrupting sessions.
    #[test]
    fn wire_matches_loro() {
        let a = Presence::new(TIMEOUT);
        a.set("alice", value("alice"));
        a.set("bob", value("bob"));
        let bytes = a.encode_all();

        let entries: Vec<Entry> = postcard::from_bytes(&bytes).unwrap();
        assert_eq!(entries.len(), 2);

        let re = postcard::to_allocvec(&entries).unwrap();
        assert_eq!(re, bytes, "re-encoded bytes drifted from loro's encoding");
    }

    #[test]
    fn apply_from_carries_state_across() {
        let a = Presence::new(TIMEOUT);
        a.set("alice", value("alice"));

        let b = Presence::new(TIMEOUT);
        let relayed = b.apply_from(&a.encode_all(), |_| true).unwrap();
        assert!(relayed.is_some());
        assert_eq!(b.get("alice"), Some(value("alice")));

        // relayed bytes are themselves applicable downstream
        let c = Presence::new(TIMEOUT);
        c.apply_from(&relayed.unwrap(), |_| true).unwrap();
        assert_eq!(c.get("alice"), Some(value("alice")));
    }

    #[test]
    fn disallowed_keys_are_dropped_and_not_relayed() {
        let mallory = Presence::new(TIMEOUT);
        mallory.set("mallory", value("mallory"));
        mallory.set("alice", value("evil twin"));

        let host = Presence::new(TIMEOUT);
        let relayed = host
            .apply_from(&mallory.encode_all(), |k| k == "mallory")
            .unwrap()
            .expect("own key survives");

        assert_eq!(host.get("mallory"), Some(value("mallory")));
        assert_eq!(host.get("alice"), None, "foreign write landed");

        let entries: Vec<Entry> = postcard::from_bytes(&relayed).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].key, "mallory");
    }

    #[test]
    fn nothing_allowed_applies_nothing() {
        let mallory = Presence::new(TIMEOUT);
        mallory.set("alice", value("evil twin"));

        let host = Presence::new(TIMEOUT);
        let relayed = host.apply_from(&mallory.encode_all(), |_| false).unwrap();
        assert!(relayed.is_none());
        assert!(host.all().is_empty());
    }

    #[test]
    fn deletes_pass_the_filter() {
        let a = Presence::new(TIMEOUT);
        let sent: Arc<Mutex<Vec<Vec<u8>>>> = Arc::default();
        let sink = sent.clone();
        let _sub = a.subscribe_local_updates(Box::new(move |bytes| {
            sink.lock().unwrap().push(bytes.clone());
            true
        }));

        let b = Presence::new(TIMEOUT);
        a.set("alice", value("alice"));
        b.apply_from(&sent.lock().unwrap()[0], |k| k == "alice")
            .unwrap();
        assert_eq!(b.get("alice"), Some(value("alice")));

        // loro's LWW keeps the existing entry on a timestamp tie, so let the
        // millisecond clock advance past the set before deleting
        std::thread::sleep(std::time::Duration::from_millis(2));
        a.delete("alice");
        let relayed = b
            .apply_from(&sent.lock().unwrap()[1], |k| k == "alice")
            .unwrap();
        assert!(relayed.is_some(), "a delete still relays");
        assert_eq!(b.get("alice"), None, "delete did not land");
    }

    #[test]
    fn hostile_nesting_is_refused() {
        // a list nested past MAX_DEPTH, built without recursion
        let mut v = Value::Null;
        for _ in 0..(MAX_DEPTH + 1) {
            v = Value::List(vec![v]);
        }

        let entry = Entry {
            key: "x".into(),
            value: Some(v),
            timestamp: i64::MAX / 2,
        };
        let bytes = postcard::to_allocvec(&vec![entry]).unwrap();

        let p = Presence::new(TIMEOUT);
        let res = p.apply_from(&bytes, |_| true);
        assert!(matches!(res, Err(PresenceError::Malformed)));
    }

    #[test]
    fn garbage_is_refused() {
        let p = Presence::new(TIMEOUT);
        let res = p.apply_from(&[0xde, 0xad, 0xbe, 0xef], |_| true);
        assert!(matches!(res, Err(PresenceError::Malformed)));
    }
}
