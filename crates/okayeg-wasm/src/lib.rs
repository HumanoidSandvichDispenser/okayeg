//! WebAssembly adapter for Okayeg.
//!
//! A thin browser peer: it owns a [`Doc`](okayeg::Doc), binds an iroh endpoint
//! with the browser's own ed identity, and runs okayeg's live sync
//! ([`drive_live`](okayeg_net::drive_live)) over the iroh stream. Document
//! operations are plain synchronous calls on the doc; only the connection is an
//! async task. Change notification rides the same `changed` nudge bus the native
//! `eg` uses.

use wasm_bindgen::prelude::*;

#[wasm_bindgen]
pub fn version() -> String {
    env!("CARGO_PKG_VERSION").to_string()
}

#[cfg(target_arch = "wasm32")]
mod identity;
#[cfg(target_arch = "wasm32")]
mod wire;

#[cfg(target_arch = "wasm32")]
mod client {
    use std::cell::RefCell;
    use std::rc::Rc;

    use iroh::endpoint::presets;
    use iroh::{Endpoint, EndpointId, SecretKey};
    use js_sys::{Array, Function};
    use okayeg::{Doc, FileTree, NodeKind, TreeID};
    use okayeg_net::{drive_live, Perms, Shared, ALPN};
    use tokio::sync::broadcast;
    use wasm_bindgen::prelude::*;
    use wasm_bindgen_futures::spawn_local;

    use crate::identity;
    use crate::wire::WireDelta;

    /// JS callbacks the client fires. Each is optional until registered.
    #[derive(Default)]
    struct Callbacks {
        on_log: RefCell<Option<Function>>,
        on_files: RefCell<Option<Function>>,
        on_file_content: RefCell<Option<Function>>,
        on_disconnect: RefCell<Option<Function>>,
    }

    impl Callbacks {
        fn log(&self, msg: &str) {
            if let Some(f) = self.on_log.borrow().as_ref() {
                let _ = f.call1(&JsValue::NULL, &JsValue::from_str(msg));
            }
        }

        fn files(&self, paths: &[String]) {
            if let Some(f) = self.on_files.borrow().as_ref() {
                let arr: Array = paths.iter().map(|p| JsValue::from_str(p)).collect();
                let _ = f.call1(&JsValue::NULL, &arr);
            }
        }

        fn file_content(&self, path: &str, content: &str) {
            if let Some(f) = self.on_file_content.borrow().as_ref() {
                let _ = f.call2(
                    &JsValue::NULL,
                    &JsValue::from_str(path),
                    &JsValue::from_str(content),
                );
            }
        }

        fn disconnect(&self, reason: &str) {
            if let Some(f) = self.on_disconnect.borrow().as_ref() {
                let _ = f.call1(&JsValue::NULL, &JsValue::from_str(reason));
            }
        }
    }

    /// Browser-side okayeg peer.
    #[wasm_bindgen]
    pub struct OkayegClient {
        doc: Shared,
        changed: broadcast::Sender<()>,
        secret: SecretKey,
        endpoint: Rc<RefCell<Option<Endpoint>>>,
        callbacks: Rc<Callbacks>,
    }

    #[wasm_bindgen]
    impl OkayegClient {
        /// Create a peer: load the browser identity, open an empty doc, and start
        /// reflecting doc changes to the registered callbacks. The endpoint is
        /// bound lazily on the first [`connect`](Self::connect).
        #[wasm_bindgen(constructor)]
        pub fn new() -> Self {
            let doc: Shared = Rc::new(Doc::new());
            let (changed, _) = broadcast::channel(64);
            let secret = identity::load_or_create();
            let callbacks = Rc::new(Callbacks::default());

            spawn_local(reflect_changes(
                doc.clone(),
                changed.subscribe(),
                callbacks.clone(),
            ));

            Self {
                doc,
                changed,
                secret,
                endpoint: Rc::new(RefCell::new(None)),
                callbacks,
            }
        }

        /// This peer's node id (hex `EndpointId`), the identity a host authorizes.
        #[wasm_bindgen(js_name = nodeId)]
        pub fn node_id(&self) -> String {
            self.secret.public().to_string()
        }

        /// Dial a host by its `EndpointId` and start syncing live.
        pub async fn connect(&self, endpoint_id: String) -> Result<(), JsValue> {
            let peer: EndpointId = endpoint_id
                .parse()
                .map_err(|_| JsValue::from_str("invalid endpoint id"))?;

            let endpoint = self.ensure_endpoint().await?;
            let conn = endpoint
                .connect(peer, ALPN)
                .await
                .map_err(|e| JsValue::from_str(&format!("connect: {e}")))?;
            let (send, recv) = conn
                .open_bi()
                .await
                .map_err(|e| JsValue::from_str(&format!("open_bi: {e}")))?;

            self.callbacks.log(&format!("connected to {peer}"));

            let doc = self.doc.clone();
            let changed = self.changed.clone();
            let callbacks = self.callbacks.clone();
            spawn_local(async move {
                let _conn = conn; // hold the link open for the session
                let reason = match drive_live(doc, send, recv, Perms::all(), changed).await {
                    Ok(()) => "peer closed".to_string(),
                    Err(e) => format!("sync ended: {e}"),
                };
                callbacks.disconnect(&reason);
            });
            Ok(())
        }

        /// The project's file paths.
        pub fn files(&self) -> Array {
            collect_files(&self.doc)
                .into_iter()
                .map(|(path, _)| JsValue::from_str(&path))
                .collect()
        }

        /// A file's current content, or `null` if it does not exist.
        pub fn read(&self, path: String) -> JsValue {
            match find_file(&self.doc, &path) {
                Some(node) => self
                    .doc
                    .files()
                    .content(node)
                    .map(|t| JsValue::from_str(&t.to_string()))
                    .unwrap_or(JsValue::NULL),
                None => JsValue::NULL,
            }
        }

        /// Apply an editor delta to a file, then push it to peers.
        #[wasm_bindgen(js_name = applyEdit)]
        pub fn apply_edit(&self, path: String, delta: JsValue) {
            let wire: WireDelta = match serde_wasm_bindgen::from_value(delta) {
                Ok(w) => w,
                Err(e) => {
                    self.callbacks.log(&format!("bad delta: {e}"));
                    return;
                }
            };
            let Some(node) = find_file(&self.doc, &path) else {
                self.callbacks.log(&format!("edit to unknown file {path:?}"));
                return;
            };
            let Some(content) = self.doc.files().content(node) else {
                return;
            };
            if let Err(e) = wire.apply_to(&content) {
                self.callbacks.log(&format!("apply delta: {e}"));
                return;
            }
            self.commit_and_nudge();
        }

        /// Create an empty file (with any missing parent directories), then push.
        #[wasm_bindgen(js_name = createFile)]
        pub fn create_file(&self, path: String) {
            if find_file(&self.doc, &path).is_some() {
                return; // never clobber an existing file
            }
            create_file_at(&self.doc, &path);
            self.commit_and_nudge();
        }

        /// Delete a file, then push the removal.
        #[wasm_bindgen(js_name = deleteFile)]
        pub fn delete_file(&self, path: String) {
            if let Some(node) = find_file(&self.doc, &path) {
                self.doc.files().delete(node);
                self.commit_and_nudge();
            }
        }

        /// Register `onLog(message: string)`.
        #[wasm_bindgen(js_name = onLog)]
        pub fn on_log(&self, callback: Function) {
            *self.callbacks.on_log.borrow_mut() = Some(callback);
        }

        /// Register `onFiles(paths: string[])`.
        #[wasm_bindgen(js_name = onFiles)]
        pub fn on_files(&self, callback: Function) {
            *self.callbacks.on_files.borrow_mut() = Some(callback);
        }

        /// Register `onFileContent(path: string, content: string)`.
        #[wasm_bindgen(js_name = onFileContent)]
        pub fn on_file_content(&self, callback: Function) {
            *self.callbacks.on_file_content.borrow_mut() = Some(callback);
        }

        /// Register `onDisconnect(reason: string)`.
        #[wasm_bindgen(js_name = onDisconnect)]
        pub fn on_disconnect(&self, callback: Function) {
            *self.callbacks.on_disconnect.borrow_mut() = Some(callback);
        }
    }

    impl OkayegClient {
        fn commit_and_nudge(&self) {
            self.doc.commit();
            let _ = self.changed.send(());
        }

        async fn ensure_endpoint(&self) -> Result<Endpoint, JsValue> {
            if let Some(ep) = self.endpoint.borrow().as_ref() {
                return Ok(ep.clone());
            }
            let ep = Endpoint::builder(presets::N0)
                .secret_key(self.secret.clone())
                .alpns(vec![ALPN.to_vec()])
                .bind()
                .await
                .map_err(|e| JsValue::from_str(&format!("bind: {e}")))?;
            *self.endpoint.borrow_mut() = Some(ep.clone());
            Ok(ep)
        }
    }

    impl Default for OkayegClient {
        fn default() -> Self {
            Self::new()
        }
    }

    /// On every `changed` tick, re-read the doc and fire the file list plus each
    /// file's content. Snapshot-per-change keeps pass 1 simple; incremental
    /// `onEdit` deltas can come later.
    async fn reflect_changes(
        doc: Shared,
        mut changed: broadcast::Receiver<()>,
        callbacks: Rc<Callbacks>,
    ) {
        loop {
            match changed.recv().await {
                Ok(()) | Err(broadcast::error::RecvError::Lagged(_)) => {
                    let files = collect_files(&doc);
                    let paths: Vec<String> = files.iter().map(|(p, _)| p.clone()).collect();
                    callbacks.files(&paths);
                    for (path, content) in files {
                        callbacks.file_content(&path, &content);
                    }
                }
                // The client (and its sender) was dropped; nothing more to do.
                Err(broadcast::error::RecvError::Closed) => return,
            }
        }
    }

    /// Every file in the doc as `(path, content)`, directories flattened into
    /// `a/b/c` paths.
    fn collect_files(doc: &Doc) -> Vec<(String, String)> {
        let tree = doc.files();
        let mut out = Vec::new();
        fn rec(
            tree: &FileTree<'_>,
            node: TreeID,
            prefix: &str,
            out: &mut Vec<(String, String)>,
        ) {
            let Some(name) = tree.name(node) else { return };
            let path = if prefix.is_empty() {
                name
            } else {
                format!("{prefix}/{name}")
            };
            match tree.kind(node) {
                Some(NodeKind::File) => {
                    let content = tree
                        .content(node)
                        .map(|t| t.to_string())
                        .unwrap_or_default();
                    out.push((path, content));
                }
                Some(NodeKind::Dir) => {
                    for child in tree.children(node) {
                        rec(tree, child, &path, out);
                    }
                }
                _ => {}
            }
        }
        for root in tree.roots() {
            rec(&tree, root, "", &mut out);
        }
        out
    }

    /// Find a node by `a/b/c` path, descending by name.
    fn find_file(doc: &Doc, path: &str) -> Option<TreeID> {
        let tree = doc.files();
        let mut node: Option<TreeID> = None;
        let mut level = tree.roots();
        for comp in path.split('/').filter(|c| !c.is_empty()) {
            let found = level
                .iter()
                .copied()
                .find(|n| tree.name(*n).as_deref() == Some(comp))?;
            node = Some(found);
            level = tree.children(found);
        }
        node
    }

    /// Create a file at `a/b/c`, making missing parent directories along the way.
    fn create_file_at(doc: &Doc, path: &str) {
        let tree = doc.files();
        let comps: Vec<&str> = path.split('/').filter(|c| !c.is_empty()).collect();
        let Some((file_name, dirs)) = comps.split_last() else {
            return;
        };
        let mut parent: Option<TreeID> = None;
        for dir in dirs {
            let children = match parent {
                Some(p) => tree.children(p),
                None => tree.roots(),
            };
            parent = Some(
                children
                    .into_iter()
                    .find(|n| {
                        tree.name(*n).as_deref() == Some(*dir)
                            && tree.kind(*n) == Some(NodeKind::Dir)
                    })
                    .unwrap_or_else(|| tree.create_dir(parent, dir)),
            );
        }
        tree.create_file(parent, file_name);
    }
}

#[cfg(target_arch = "wasm32")]
pub use client::OkayegClient;
