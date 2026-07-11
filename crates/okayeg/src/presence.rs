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
use serde::de::{
    DeserializeSeed, EnumAccess, Error as DeError, MapAccess, SeqAccess, VariantAccess, Visitor,
};
use serde::ser::{SerializeMap, Serializer};
use serde::{Deserialize, Deserializer, Serialize};

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
        return key == owner || key.strip_prefix(owner).is_some_and(|r| r.starts_with('/'));
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

    /// Purge entries past their timeout, notifying subscribers of removals.
    /// Nothing runs this on a schedule; until it runs, expiry only shows in
    /// what gets encoded.
    pub fn remove_outdated(&self) {
        self.store.remove_outdated();
    }

    /// Watch the merged store: added, updated and removed keys, whether the
    /// change was local, imported or a timeout.
    pub fn subscribe(&self, callback: EphemeralSubscriber) -> Subscription {
        self.store.subscribe(callback)
    }

    /// Watch local changes as encoded bytes, ready for the wire.
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
        let key = Self::cursor_key(ns);

        self.get(&key).and_then(|value| {
            if let LoroValue::Map(map) = value {
                let file = map.get("file")?.as_string()?.to_string();
                let anchor = map.get("anchor")?.as_binary()?.to_vec();
                let head = map.get("head")?.as_binary()?.to_vec();
                Some((file, anchor, head))
            } else {
                None
            }
        })
    }
}

// Loro's EphemeralStore encodes as postcard over a sequence of
// { key, Option<LoroValue>, timestamp } entries, and its apply() ingests a
// payload whole, so filtering means decoding that layout here. Two deliberate
// differences from a plain LoroValue decode: nesting is depth-limited, and
// Container values are refused since a container reference has no meaning as
// ephemeral state. The encoding is not public API; wire_matches_loro fails
// loudly if a Loro upgrade changes it.

/// One store entry as Loro lays it out on the wire. Field order is the format.
#[derive(Serialize, Deserialize)]
struct Entry {
    key: String,
    value: Option<Value>,
    timestamp: i64,
}

/// LoroValue's wire shape, minus Container. Variant order (and the odd I32
/// name for what holds an i64) must match LoroValue's serde derive exactly,
/// since postcard encodes variants by index.
enum Value {
    Null,
    Bool(bool),
    Double(f64),
    I64(i64),
    String(String),
    List(Vec<Value>),
    // pairs, not a hash map, to re-encode in the exact order decoded
    Map(Vec<(String, Value)>),
    Binary(Vec<u8>),
}

/// How deep a value may nest; bounds a crafted payload's recursion.
const MAX_DEPTH: usize = 64;

const VARIANTS: &[&str] = &[
    "Null",
    "Bool",
    "Double",
    "I32",
    "String",
    "List",
    "Map",
    "Container",
    "Binary",
];

impl Serialize for Value {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        const N: &str = "LoroValue";
        match self {
            Value::Null => s.serialize_unit_variant(N, 0, "Null"),
            Value::Bool(v) => s.serialize_newtype_variant(N, 1, "Bool", v),
            Value::Double(v) => s.serialize_newtype_variant(N, 2, "Double", v),
            Value::I64(v) => s.serialize_newtype_variant(N, 3, "I32", v),
            Value::String(v) => s.serialize_newtype_variant(N, 4, "String", v),
            Value::List(v) => s.serialize_newtype_variant(N, 5, "List", v),
            Value::Map(v) => s.serialize_newtype_variant(N, 6, "Map", &MapWire(v)),
            Value::Binary(v) => s.serialize_newtype_variant(N, 8, "Binary", v),
        }
    }
}

/// Serializes key/value pairs through the map protocol, matching how a hash
/// map lays out on the wire.
struct MapWire<'a>(&'a [(String, Value)]);

impl Serialize for MapWire<'_> {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let mut m = s.serialize_map(Some(self.0.len()))?;
        for (k, v) in self.0 {
            m.serialize_entry(k, v)?;
        }
        m.end()
    }
}

impl<'de> Deserialize<'de> for Value {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        ValueSeed { depth: MAX_DEPTH }.deserialize(d)
    }
}

#[derive(Deserialize)]
enum Tag {
    Null,
    Bool,
    Double,
    I32,
    String,
    List,
    Map,
    Container,
    Binary,
}

#[derive(Clone, Copy)]
struct ValueSeed {
    depth: usize,
}

impl<'de> DeserializeSeed<'de> for ValueSeed {
    type Value = Value;

    fn deserialize<D: Deserializer<'de>>(self, d: D) -> Result<Value, D::Error> {
        d.deserialize_enum("LoroValue", VARIANTS, self)
    }
}

impl<'de> Visitor<'de> for ValueSeed {
    type Value = Value;

    fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.write_str("a presence value")
    }

    fn visit_enum<A: EnumAccess<'de>>(self, data: A) -> Result<Value, A::Error> {
        let next = Self {
            depth: self
                .depth
                .checked_sub(1)
                .ok_or_else(|| A::Error::custom("presence value nests too deep"))?,
        };

        match data.variant()? {
            (Tag::Null, v) => {
                v.unit_variant()?;
                Ok(Value::Null)
            }
            (Tag::Bool, v) => v.newtype_variant().map(Value::Bool),
            (Tag::Double, v) => v.newtype_variant().map(Value::Double),
            (Tag::I32, v) => v.newtype_variant().map(Value::I64),
            (Tag::String, v) => v.newtype_variant().map(Value::String),
            (Tag::List, v) => v.newtype_variant_seed(ListSeed(next)).map(Value::List),
            (Tag::Map, v) => v.newtype_variant_seed(MapSeed(next)).map(Value::Map),
            (Tag::Container, _) => Err(A::Error::custom("container values are not presence data")),
            (Tag::Binary, v) => v.newtype_variant().map(Value::Binary),
        }
    }
}

struct ListSeed(ValueSeed);

impl<'de> DeserializeSeed<'de> for ListSeed {
    type Value = Vec<Value>;

    fn deserialize<D: Deserializer<'de>>(self, d: D) -> Result<Self::Value, D::Error> {
        d.deserialize_seq(self)
    }
}

impl<'de> Visitor<'de> for ListSeed {
    type Value = Vec<Value>;

    fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.write_str("a presence list")
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
        let mut out = Vec::new();
        while let Some(v) = seq.next_element_seed(self.0)? {
            out.push(v);
        }
        Ok(out)
    }
}

struct MapSeed(ValueSeed);

impl<'de> DeserializeSeed<'de> for MapSeed {
    type Value = Vec<(String, Value)>;

    fn deserialize<D: Deserializer<'de>>(self, d: D) -> Result<Self::Value, D::Error> {
        d.deserialize_map(self)
    }
}

impl<'de> Visitor<'de> for MapSeed {
    type Value = Vec<(String, Value)>;

    fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.write_str("a presence map")
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
        let mut out = Vec::new();
        while let Some(k) = map.next_key::<String>()? {
            let v = map.next_value_seed(self.0)?;
            out.push((k, v));
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
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
