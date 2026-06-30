//! Text deltas across the JS boundary.
//!
//! The shape matches what the editor already produces (a list of retain / insert
//! / delete ops, offsets counted in Unicode code points), so the frontend's
//! existing `applyWireDelta` keeps working. We translate a delta into splices on
//! a Loro `LoroText`.

use loro::LoroText;
use serde::{Deserialize, Serialize};
use tsify_next::Tsify;

#[derive(Serialize, Deserialize, Tsify)]
#[tsify(into_wasm_abi, from_wasm_abi)]
pub enum WireOp {
    Retain(usize),
    Insert(String),
    Delete(usize),
}

#[derive(Serialize, Deserialize, Tsify)]
#[tsify(into_wasm_abi, from_wasm_abi)]
pub struct WireDelta(pub Vec<WireOp>);

impl WireDelta {
    /// Apply this delta to `text`, mirroring the editor's own application.
    /// Offsets advance in Unicode code points, matching Loro's index space.
    pub fn apply_to(self, text: &LoroText) -> Result<(), loro::LoroError> {
        let mut pos = 0usize;
        for op in self.0 {
            match op {
                WireOp::Retain(n) => pos += n,
                WireOp::Insert(s) => {
                    text.insert(pos, &s)?;
                    pos += s.chars().count();
                }
                WireOp::Delete(n) => text.delete(pos, n)?,
            }
        }
        Ok(())
    }
}
