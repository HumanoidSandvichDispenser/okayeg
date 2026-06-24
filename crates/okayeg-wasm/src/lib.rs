//! WebAssembly adapter for Okayeg.
//!
//! Owns the doc and exposes a narrow surface to JavaScript.

use wasm_bindgen::prelude::*;

#[wasm_bindgen]
pub fn version() -> String {
    env!("CARGO_PKG_VERSION").to_string()
}
