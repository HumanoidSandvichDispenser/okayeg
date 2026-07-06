//! The browser peer's own ed identity.
//!
//! okayeg authorizes peers by their ed public key, so each browser needs a
//! stable secret of its own. We keep the 32-byte seed in localStorage and derive
//! the iroh `SecretKey` (and thus the `EndpointId` a host trusts) from it. A host
//! later authorizes this `EndpointId` (see okayeg-net's `Authorizer`).

use iroh::SecretKey;

/// localStorage key under which the raw 32-byte seed is stored (hex-encoded).
const STORAGE_KEY: &str = "okayeg-identity-seed";

/// Load the persisted identity, or mint and persist a fresh one.
pub fn load_or_create() -> SecretKey {
    if let Some(seed) = load_seed() {
        return SecretKey::from_bytes(&seed);
    }
    let mut seed = [0u8; 32];
    getrandom::fill(&mut seed).expect("browser csprng");
    store_seed(&seed);
    SecretKey::from_bytes(&seed)
}

fn storage() -> Option<web_sys::Storage> {
    web_sys::window()?.local_storage().ok().flatten()
}

fn load_seed() -> Option<[u8; 32]> {
    let hex = storage()?.get_item(STORAGE_KEY).ok().flatten()?;
    let bytes = decode_hex(&hex)?;
    bytes.try_into().ok()
}

fn store_seed(seed: &[u8; 32]) {
    if let Some(store) = storage() {
        let _ = store.set_item(STORAGE_KEY, &encode_hex(seed));
    }
}

fn encode_hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

fn decode_hex(s: &str) -> Option<Vec<u8>> {
    if !s.len().is_multiple_of(2) {
        return None;
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).ok())
        .collect()
}
