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
mod comments;
#[cfg(target_arch = "wasm32")]
mod identity;
#[cfg(target_arch = "wasm32")]
mod wire;

#[cfg(target_arch = "wasm32")]
mod client {
    use std::cell::RefCell;
    use std::rc::Rc;

    use std::collections::HashMap;

    use iroh::endpoint::presets;
    use iroh::{Endpoint, EndpointId, SecretKey};
    use js_sys::{Array, Function, Object, Reflect};
    use okayeg::{Doc, FileTree, LoroValue, NodeKind, Presence, Subscription, TreeID};
    use okayeg_net::{ALPN, Perms, PresenceLink, Shared, drive_live};
    use tokio::sync::{broadcast, mpsc};
    use wasm_bindgen::prelude::*;
    use wasm_bindgen_futures::spawn_local;

    use crate::identity;
    use crate::wire::WireDelta;

    /// How long a presence entry lives without a refresh.
    const PRESENCE_TIMEOUT_MS: i64 = 30_000;

    /// How often the own entry is re-set and expired peers are swept.
    const PRESENCE_REFRESH_MS: i32 = 10_000;

    /// JS callbacks the client fires. Each is optional until registered.
    #[derive(Default)]
    struct Callbacks {
        on_log: RefCell<Option<Function>>,
        on_files: RefCell<Option<Function>>,
        on_file_content: RefCell<Option<Function>>,
        on_comments: RefCell<Option<Function>>,
        on_presence: RefCell<Option<Function>>,
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

        fn comments(&self, comments: &Array) {
            if let Some(f) = self.on_comments.borrow().as_ref() {
                let _ = f.call1(&JsValue::NULL, comments);
            }
        }

        fn presence(&self, peers: &JsValue) {
            if let Some(f) = self.on_presence.borrow().as_ref() {
                let _ = f.call1(&JsValue::NULL, peers);
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
        presence: Presence,
        relay: broadcast::Sender<(String, Vec<u8>)>,
        /// The own entry as last set, re-set by the refresh task so it never
        /// expires while the tab lives.
        my_presence: Rc<RefCell<Option<LoroValue>>>,
        _presence_subs: (Subscription, Subscription),
    }

    #[wasm_bindgen]
    impl OkayegClient {
        /// Create a peer: load the browser identity, open an empty doc, and start
        /// reflecting doc changes to the registered callbacks. The endpoint is
        /// bound lazily on the first [`connect`](Self::connect).
        ///
        /// The seed is loaded from (or minted into) localStorage by the wasm
        /// binding itself. Prefer [`with_seed`](Self::with_seed) when the host
        /// app owns identity persistence, so it controls where the seed lives
        /// and knows when a fresh one was created (to upload its public half).
        #[wasm_bindgen(constructor)]
        pub fn new() -> Self {
            Self::from_secret(identity::load_or_create())
        }

        /// Create a peer from a caller-supplied 32-byte seed. This lets the host
        /// app own identity persistence (IndexedDB, etc.) and identity rotation,
        /// rather than the binding reaching into localStorage. Errors if the seed
        /// is not exactly 32 bytes.
        #[wasm_bindgen(js_name = withSeed)]
        pub fn with_seed(seed: &[u8]) -> Result<OkayegClient, JsValue> {
            let seed: [u8; 32] = seed
                .try_into()
                .map_err(|_| JsValue::from_str("seed must be exactly 32 bytes"))?;
            Ok(Self::from_secret(SecretKey::from_bytes(&seed)))
        }

        /// Mint a fresh 32-byte identity seed with the browser CSPRNG. The host
        /// app persists this and passes it back to [`with_seed`](Self::with_seed);
        /// the public half (see [`node_id`](Self::node_id)) is what a host
        /// authorizes.
        #[wasm_bindgen(js_name = generateSeed)]
        pub fn generate_seed() -> Vec<u8> {
            let mut seed = [0u8; 32];
            getrandom::fill(&mut seed).expect("browser csprng");
            seed.to_vec()
        }

        /// Shared construction: open an empty doc and start reflecting its
        /// changes to the registered callbacks. Endpoint binds lazily on the
        /// first [`connect`](Self::connect).
        fn from_secret(secret: SecretKey) -> Self {
            let doc: Shared = Rc::new(Doc::new());
            let (changed, _) = broadcast::channel(64);
            let callbacks = Rc::new(Callbacks::default());

            spawn_local(reflect_changes(
                doc.clone(),
                changed.subscribe(),
                callbacks.clone(),
            ));

            let presence = Presence::new(PRESENCE_TIMEOUT_MS);
            let (relay, _) = broadcast::channel(64);
            let key = secret.public().to_string();

            // local set/delete flows to the host through the relay
            let local_sub = {
                let relay = relay.clone();
                let key = key.clone();
                presence.subscribe_local_updates(Box::new(move |bytes| {
                    let _ = relay.send((key.clone(), bytes.clone()));
                    true
                }))
            };

            // store subscribers must be Send, so events hop to the JS callback
            // through a channel drained by a local task
            let (events_tx, events_rx) = mpsc::unbounded_channel();
            let events_sub = presence.subscribe(Box::new(move |_| {
                let _ = events_tx.send(());
                true
            }));
            spawn_local(reflect_presence(
                presence.clone(),
                events_rx,
                callbacks.clone(),
            ));

            let my_presence = Rc::new(RefCell::new(None));
            spawn_local(refresh_presence(presence.clone(), key, my_presence.clone()));

            Self {
                doc,
                changed,
                secret,
                endpoint: Rc::new(RefCell::new(None)),
                callbacks,
                presence,
                relay,
                my_presence,
                _presence_subs: (local_sub, events_sub),
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
            // the dialed peer is the host, so it may carry every presence key
            let link = PresenceLink {
                presence: self.presence.clone(),
                relay: self.relay.clone(),
                owner: None,
            };
            spawn_local(async move {
                let _conn = conn; // hold the link open for the session
                let reason =
                    match drive_live(doc, send, recv, Perms::all(), changed, Some(link)).await {
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
                .map(|(path, _, _)| JsValue::from_str(&path))
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
                self.callbacks
                    .log(&format!("edit to unknown file {path:?}"));
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

        /// All comments in the project. Each is `{id, file, parent, createdAt,
        /// range, orphaned, fields}`; see the [`comments`](crate::comments)
        /// module for the shape.
        pub fn comments(&self) -> Array {
            let nodes: Vec<(String, TreeID)> = collect_files(&self.doc)
                .into_iter()
                .map(|(p, n, _)| (p, n))
                .collect();
            crate::comments::to_js(&self.doc, &nodes)
        }

        /// Anchor a comment to `[start, end)` (Unicode code points) in a file,
        /// with consumer fields such as `body` and `author`, then push.
        /// Returns the comment id, or `null` when the file or range is bad.
        #[wasm_bindgen(js_name = addComment)]
        pub fn add_comment(
            &self,
            path: String,
            start: usize,
            end: usize,
            fields: JsValue,
        ) -> Option<String> {
            let node = find_file(&self.doc, &path)?;
            let id = self.doc.comments().add(
                node,
                start..end,
                js_sys::Date::now() as i64,
                &crate::comments::fields_from_js(&fields),
            )?;
            self.commit_and_nudge();
            Some(id)
        }

        /// Reply to a comment, with consumer fields, then push. Returns the
        /// reply's id, or `null` when the parent does not exist.
        #[wasm_bindgen(js_name = replyComment)]
        pub fn reply_comment(&self, parent: String, fields: JsValue) -> Option<String> {
            let id = self.doc.comments().reply(
                &parent,
                js_sys::Date::now() as i64,
                &crate::comments::fields_from_js(&fields),
            )?;
            self.commit_and_nudge();
            Some(id)
        }

        /// Set one consumer field on a comment (a scalar: string, boolean,
        /// number, or null), then push. Returns `false` when the comment does
        /// not exist, the key is core-interpreted, or the value is not a scalar.
        #[wasm_bindgen(js_name = setComment)]
        pub fn set_comment(&self, id: String, key: String, value: JsValue) -> bool {
            let Some(value) = crate::comments::value_from_js(&value) else {
                return false;
            };
            if !self.doc.comments().set(&id, &key, value) {
                return false;
            }
            self.commit_and_nudge();
            true
        }

        /// Remove a comment, then push. Replies to it stay.
        #[wasm_bindgen(js_name = removeComment)]
        pub fn remove_comment(&self, id: String) {
            self.doc.comments().remove(&id);
            self.commit_and_nudge();
        }

        /// Set this peer's presence entry: an object of scalar fields (such as
        /// `{name, email}`), shared live with every peer on the project. The
        /// entry refreshes itself until [`clear_presence`](Self::clear_presence)
        /// or the tab closes, after which peers expire it.
        #[wasm_bindgen(js_name = setPresence)]
        pub fn set_presence(&self, fields: JsValue) {
            let map: HashMap<String, LoroValue> = crate::comments::fields_from_js(&fields)
                .into_iter()
                .collect();
            let value = LoroValue::from(map);
            *self.my_presence.borrow_mut() = Some(value.clone());
            self.presence.set(&self.node_id(), value);
        }

        /// Drop this peer's presence entry.
        #[wasm_bindgen(js_name = clearPresence)]
        pub fn clear_presence(&self) {
            *self.my_presence.borrow_mut() = None;
            self.presence.delete(&self.node_id());
        }

        /// Every live presence entry, keyed by peer node id, own entry included.
        pub fn presence(&self) -> JsValue {
            presence_to_js(&self.presence)
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

        /// Register `onComments(comments: Comment[])`, fired with the full
        /// comment list on every doc change.
        #[wasm_bindgen(js_name = onComments)]
        pub fn on_comments(&self, callback: Function) {
            *self.callbacks.on_comments.borrow_mut() = Some(callback);
        }

        /// Register `onPresence(peers: object)`, fired with every live entry
        /// (as [`presence`](Self::presence) returns) on each change.
        #[wasm_bindgen(js_name = onPresence)]
        pub fn on_presence(&self, callback: Function) {
            *self.callbacks.on_presence.borrow_mut() = Some(callback);
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

    /// On every store event, fire the presence callback with a fresh snapshot.
    async fn reflect_presence(
        presence: Presence,
        mut events: mpsc::UnboundedReceiver<()>,
        callbacks: Rc<Callbacks>,
    ) {
        while events.recv().await.is_some() {
            callbacks.presence(&presence_to_js(&presence));
        }
    }

    /// Re-set the own entry and sweep expired peers, forever. The re-set is
    /// the keepalive: it bumps the entry's timestamp and streams to the host.
    async fn refresh_presence(
        presence: Presence,
        key: String,
        mine: Rc<RefCell<Option<LoroValue>>>,
    ) {
        loop {
            sleep_ms(PRESENCE_REFRESH_MS).await;
            let value = mine.borrow().clone();
            if let Some(value) = value {
                presence.set(&key, value);
            }
            presence.remove_outdated();
        }
    }

    /// The store as a JS object: entry keys to objects of scalar fields.
    fn presence_to_js(presence: &Presence) -> JsValue {
        let out = Object::new();
        for (key, value) in presence.all() {
            let entry: JsValue = match &value {
                LoroValue::Map(fields) => {
                    let obj = Object::new();
                    for (k, v) in fields.iter() {
                        let _ = Reflect::set(
                            &obj,
                            &JsValue::from_str(k),
                            &crate::comments::value_to_js(v),
                        );
                    }
                    obj.into()
                }
                v => crate::comments::value_to_js(v),
            };
            let _ = Reflect::set(&out, &JsValue::from_str(&key), &entry);
        }
        out.into()
    }

    async fn sleep_ms(ms: i32) {
        let promise = js_sys::Promise::new(&mut |resolve, _| {
            let _ = web_sys::window()
                .expect("browser window")
                .set_timeout_with_callback_and_timeout_and_arguments_0(&resolve, ms);
        });
        let _ = wasm_bindgen_futures::JsFuture::from(promise).await;
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

                    let paths: Vec<String> = files.iter().map(|(p, _, _)| p.clone()).collect();
                    callbacks.files(&paths);

                    let nodes: Vec<(String, TreeID)> =
                        files.iter().map(|(p, n, _)| (p.clone(), *n)).collect();
                    for (path, _, content) in files {
                        callbacks.file_content(&path, &content);
                    }

                    callbacks.comments(&crate::comments::to_js(&doc, &nodes));
                }
                // The client (and its sender) was dropped; nothing more to do.
                Err(broadcast::error::RecvError::Closed) => return,
            }
        }
    }

    /// Every file in the doc as `(path, node, content)`, directories flattened
    /// into `a/b/c` paths.
    fn collect_files(doc: &Doc) -> Vec<(String, TreeID, String)> {
        let tree = doc.files();
        let mut out = Vec::new();
        fn rec(
            tree: &FileTree<'_>,
            node: TreeID,
            prefix: &str,
            out: &mut Vec<(String, TreeID, String)>,
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
                    out.push((path, node, content));
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
