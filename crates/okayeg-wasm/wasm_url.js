// The URL of the wasm binary, for callers that pass it to init explicitly
// instead of relying on the default relative lookup. Lives outside pkg/ so
// wasm-pack rebuilds cannot delete it, and bundlers turn the new URL into an
// emitted asset.
export default new URL('./pkg/okayeg_wasm_bg.wasm', import.meta.url);
