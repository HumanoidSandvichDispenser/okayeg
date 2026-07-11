//! Comments across the JS boundary.
//!
//! A comment crosses as a plain object: `{id, file, parent, createdAt, range,
//! orphaned, fields}`, with `file` a path, `range` a `[start, end]` pair in
//! Unicode code points (or `null` when unresolved), and `fields` the
//! consumer-owned keys. Field values are JS scalars: strings, booleans,
//! numbers, and null.

use js_sys::{Array, Object, Reflect};
use loro::LoroValue;
use okayeg::{Comment, Doc};
use wasm_bindgen::JsCast as _;
use wasm_bindgen::JsValue;

/// All comments in the doc as a JS array, file node ids resolved to paths
/// via `paths` (`(path, node)` pairs from the file tree walk).
pub fn to_js(doc: &Doc, paths: &[(String, okayeg::TreeID)]) -> Array {
    doc.comments()
        .list(None)
        .iter()
        .map(|c| comment_to_js(c, paths))
        .collect()
}

fn comment_to_js(comment: &Comment, paths: &[(String, okayeg::TreeID)]) -> JsValue {
    let obj = Object::new();
    let set = |key: &str, value: JsValue| {
        let _ = Reflect::set(&obj, &JsValue::from_str(key), &value);
    };
    set("id", JsValue::from_str(&comment.id));
    let path = comment.file.and_then(|node| {
        paths
            .iter()
            .find(|(_, n)| *n == node)
            .map(|(p, _)| JsValue::from_str(p))
    });
    set("file", path.unwrap_or(JsValue::NULL));
    set(
        "parent",
        comment
            .parent
            .as_deref()
            .map(JsValue::from_str)
            .unwrap_or(JsValue::NULL),
    );
    set("createdAt", JsValue::from_f64(comment.created_at as f64));
    set(
        "range",
        match &comment.range {
            Some(r) => {
                let pair = Array::new();
                pair.push(&JsValue::from_f64(r.start as f64));
                pair.push(&JsValue::from_f64(r.end as f64));
                pair.into()
            }
            None => JsValue::NULL,
        },
    );
    set("orphaned", JsValue::from_bool(comment.orphaned));
    let fields = Object::new();
    for (key, value) in &comment.fields {
        let _ = Reflect::set(&fields, &JsValue::from_str(key), &value_to_js(value));
    }
    set("fields", fields.into());
    obj.into()
}

pub fn value_to_js(value: &LoroValue) -> JsValue {
    match value {
        LoroValue::Bool(b) => JsValue::from_bool(*b),
        LoroValue::Double(d) => JsValue::from_f64(*d),
        LoroValue::I64(i) => JsValue::from_f64(*i as f64),
        LoroValue::String(s) => JsValue::from_str(s),
        _ => JsValue::NULL,
    }
}

/// A JS scalar as a [`LoroValue`]. `None` for anything else.
pub fn value_from_js(value: &JsValue) -> Option<LoroValue> {
    if value.is_null() || value.is_undefined() {
        Some(LoroValue::Null)
    } else if let Some(b) = value.as_bool() {
        Some(LoroValue::Bool(b))
    } else if let Some(n) = value.as_f64() {
        Some(LoroValue::Double(n))
    } else {
        value.as_string().map(LoroValue::from)
    }
}

/// A JS object's entries as comment fields. Non-scalar values are skipped.
pub fn fields_from_js(fields: &JsValue) -> Vec<(String, LoroValue)> {
    if !fields.is_object() {
        return Vec::new();
    }
    Object::entries(fields.unchecked_ref())
        .iter()
        .filter_map(|entry| {
            let entry: Array = entry.into();
            let key = entry.get(0).as_string()?;
            let value = value_from_js(&entry.get(1))?;
            Some((key, value))
        })
        .collect()
}
