//! HACK: Loro does not have a public API for filtering and applying keys from the raw bytes of an
//! encoded EphemeralStore. We implement our own serialization and deserialization of the format,
//! which can be used to filter out keys we don't want to store, and to apply keys we do want to
//! store. This is a hack since it relies on the internal details of Loro's serialization format,
//! which could change in future versions Loro. The
//! [`wire_matches_loro`](crate::presence::wire_matches_loro) test ensures that our serialization
//! and deserialization matches Loro's.

use serde::de::{
    DeserializeSeed, EnumAccess, Error as DeError, MapAccess, SeqAccess, VariantAccess, Visitor,
};
use serde::ser::{SerializeMap, Serializer};
use serde::{Deserialize, Deserializer, Serialize};

/// One store entry as Loro lays it out on the wire. Field order is the format.
#[derive(Serialize, Deserialize)]
pub(super) struct Entry {
    pub(super) key: String,
    pub(super) value: Option<Value>,
    pub(super) timestamp: i64,
}

/// LoroValue's wire shape, minus Container. Variant order (and the odd I32
/// name for what holds an i64) must match LoroValue's serde derive exactly,
/// since postcard encodes variants by index.
pub(super) enum Value {
    Null,
    Bool(bool),
    Double(f64),
    I64(i64),
    String(String),
    List(Vec<Value>),
    // keypairs to re-encode in the exact order decoded
    Map(Vec<(String, Value)>),
    Binary(Vec<u8>),
}

/// How deep a value may nest which bounds a crafted payload's recursion. In practice, we would not
/// expect to see nested objects, but an extension of the protocol may require or use it.
pub(super) const MAX_DEPTH: usize = 64;

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

/// Serializes key/value pairs through the map protocol.
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
