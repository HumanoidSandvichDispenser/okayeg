use loro::{
    ExportMode, Frontiers, ImportStatus, LoroDoc, LoroError, LoroText, PeerID, VersionVector,
};

/// A single Okayeg doc, wrapping one Loro document.
pub struct Doc {
    inner: LoroDoc,
    /// Session-scoped sequence used to mint comment ids.
    pub(crate) comment_seq: std::sync::atomic::AtomicU64,
}

impl Doc {
    /// Create a new, empty doc.
    pub fn new() -> Self {
        Self {
            inner: LoroDoc::new(),
            comment_seq: std::sync::atomic::AtomicU64::new(0),
        }
    }

    /// Open a doc from a snapshot produced by [`Doc::snapshot`].
    pub fn from_snapshot(bytes: &[u8]) -> Result<Self, LoroError> {
        let inner = LoroDoc::new();
        inner.import(bytes)?;
        Ok(Self {
            inner,
            comment_seq: std::sync::atomic::AtomicU64::new(0),
        })
    }

    /// The text container with the given name, created on first access.
    ///
    /// Content edits go through the returned handle; they apply to this copy
    /// immediately and are folded into history at the next [`commit`](Self::commit).
    pub fn text(&self, name: &str) -> LoroText {
        self.inner.get_text(name)
    }

    /// Fold pending edits into the doc's history.
    ///
    /// Edits made through a container handle are buffered until committed;
    /// exporting or taking a version reflects only committed history.
    pub fn commit(&self) {
        self.inner.commit();
    }

    /// The doc's current version.
    pub fn version(&self) -> VersionVector {
        self.inner.state_vv()
    }

    /// The doc's current frontiers, the tips of its history.
    ///
    /// A checkpoint pins a doc to a frontier, so this is what gets recorded.
    pub fn frontiers(&self) -> Frontiers {
        self.inner.state_frontiers()
    }

    /// This copy's peer id, the origin marker assigned when the doc is created.
    pub fn peer_id(&self) -> PeerID {
        self.inner.peer_id()
    }

    /// A full snapshot of history and state, for storage or a first share.
    pub fn snapshot(&self) -> Result<Vec<u8>, LoroError> {
        self.inner
            .export(ExportMode::snapshot())
            .map_err(Into::into)
    }

    /// The updates this doc has that the given version does not.
    ///
    /// This is the answer to a peer's "here is the version I have, send me
    /// what I'm missing." An empty `since` yields the doc's whole history.
    pub fn updates_since(&self, since: &VersionVector) -> Result<Vec<u8>, LoroError> {
        self.inner
            .export(ExportMode::updates(since))
            .map_err(Into::into)
    }

    /// Apply updates received from a peer.
    ///
    /// The returned [`ImportStatus`] reports what applied and what is held
    /// pending. Updates whose ancestors are missing land in `pending` rather
    /// than applying partially, so the caller can tell "references history I
    /// don't have" from a clean apply.
    pub fn import(&self, bytes: &[u8]) -> Result<ImportStatus, LoroError> {
        self.inner.import(bytes)
    }

    /// The underlying Loro document.
    pub fn inner(&self) -> &LoroDoc {
        &self.inner
    }
}

impl Default for Doc {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_round_trips() {
        let doc = Doc::new();
        doc.text("body").insert(0, "hello").unwrap();
        doc.commit();

        let reopened = Doc::from_snapshot(&doc.snapshot().unwrap()).unwrap();
        assert_eq!(reopened.text("body").to_string(), "hello");
    }

    #[test]
    fn import_catches_a_peer_up() {
        let doc = Doc::new();
        doc.text("body").insert(0, "hello").unwrap();
        doc.commit();

        // A peer with nothing yet catches up from the full update stream.
        let peer = Doc::new();
        let caught_up = peer.version();
        peer.import(&doc.updates_since(&caught_up).unwrap()).unwrap();
        assert_eq!(peer.text("body").to_string(), "hello");

        // A later edit ships as just the delta since the peer's version.
        doc.text("body").insert(5, " world").unwrap();
        doc.commit();
        let delta = doc.updates_since(&peer.version()).unwrap();
        peer.import(&delta).unwrap();
        assert_eq!(peer.text("body").to_string(), "hello world");
    }
}
