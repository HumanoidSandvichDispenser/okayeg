//! The iroh endpoint, bound for okayeg sync.
//!
//! A [`Node`] is one peer on the mesh. Its secret key is its identity: the [`EndpointId`] others
//! dial, and, later, the key access control gates on. The drive loop in the crate root does the
//! actual protocol; this type just stands up the endpoint and hands it a bidirectional stream per
//! peer.

use iroh::endpoint::presets;
use iroh::{Endpoint, EndpointAddr, EndpointId, SecretKey};
use okayeg::{Doc, Perms};

use crate::{drive, Error, ALPN};

/// Generate a fresh 32-byte secret, the raw form of a node's identity.
///
/// The caller persists these bytes (we keep crypto here and storage out of this crate) and feeds
/// them back to [`Node::bind_with_secret`] to keep a stable identity across restarts.
pub fn generate_secret() -> [u8; 32] {
    SecretKey::generate().to_bytes()
}

/// The public identity for a persisted secret, without binding an endpoint.
///
/// Lets a peer print the [`EndpointId`] others dial (and trust) cheaply, with no sockets and no
/// network.
pub fn id_from_secret(secret: [u8; 32]) -> EndpointId {
    SecretKey::from_bytes(&secret).public()
}

/// The outcome of accepting one peer at the gate.
///
/// Carries the peer's id either way so the caller can log who connected.
#[derive(Debug)]
pub enum Accepted {
    /// The gate did not trust this peer; no sync ran.
    Refused(EndpointId),
    /// The peer was synced with these granted perms.
    Synced(EndpointId, Perms),
}

/// A bound iroh endpoint that syncs a doc with peers over [`ALPN`].
///
/// The endpoint's secret key is its identity: the [`EndpointId`] others dial, and, later, the key
/// access control gates on. Use [`Node::bind_with_secret`] with a persisted secret to keep that
/// identity across restarts, or [`Node::bind`] for a throwaway one.
pub struct Node {
    endpoint: Endpoint,
}

impl Node {
    /// Bind an endpoint with a fresh, throwaway identity.
    ///
    /// The id changes every call, so this is for tests and one-off dials, not a served repo a peer
    /// needs to find again.
    pub async fn bind() -> Result<Self, Error> {
        Self::bind_with_secret(generate_secret()).await
    }

    /// Bind an endpoint with a persisted secret, keeping a stable identity.
    pub async fn bind_with_secret(secret: [u8; 32]) -> Result<Self, Error> {
        let endpoint = Endpoint::builder(presets::N0)
            .secret_key(SecretKey::from_bytes(&secret))
            .alpns(vec![ALPN.to_vec()])
            .bind()
            .await
            .map_err(|e| Error::Transport(e.to_string()))?;
        Ok(Self { endpoint })
    }

    /// This node's identity, the id a peer dials to reach it.
    pub fn id(&self) -> EndpointId {
        self.endpoint.id()
    }

    /// This node's full address, including reachable paths. Waits until the
    /// node is online so the address is dialable.
    pub async fn addr(&self) -> EndpointAddr {
        self.endpoint.online().await;
        self.endpoint.addr()
    }

    /// Dial `peer` and run one full sync of `doc` against it.
    ///
    /// The dialer grants the peer full perms; access control is the accepting
    /// side's job (see [`accept_one`](Self::accept_one)).
    pub async fn sync_with(&self, peer: impl Into<EndpointAddr>, doc: &Doc) -> Result<(), Error> {
        let conn = self
            .endpoint
            .connect(peer, ALPN)
            .await
            .map_err(|e| Error::Transport(e.to_string()))?;
        let (send, recv) = conn
            .open_bi()
            .await
            .map_err(|e| Error::Transport(e.to_string()))?;
        drive(doc, send, recv, Perms::all()).await?;
        // Do not close the connection ourselves: that would send a QUIC
        // CONNECTION_CLOSE and abort the stream still carrying our last frame
        // before the peer reads it. Hold the link open until the accepting side
        // has consumed everything and closes, which it does once its own sync
        // completes.
        let _ = conn.closed().await;
        Ok(())
    }

    /// Accept one incoming peer, gating it before any sync runs.
    ///
    /// `gate` is handed the peer's authenticated [`EndpointId`] and decides what
    /// it may do: `Some(perms)` to sync with those perms, or `None` to refuse.
    /// A refused peer's connection is closed and the doc is never touched. The
    /// gate is where a trust set (`.eg/trust`) is consulted; this crate stays
    /// out of where that trust lives.
    pub async fn accept_one<G>(&self, doc: &Doc, gate: G) -> Result<Accepted, Error>
    where
        G: FnOnce(EndpointId) -> Option<Perms>,
    {
        let incoming = self
            .endpoint
            .accept()
            .await
            .ok_or_else(|| Error::Transport("endpoint closed".into()))?;
        let conn = incoming
            .await
            .map_err(|e| Error::Transport(e.to_string()))?;
        let who = conn.remote_id();

        let Some(perms) = gate(who) else {
            // Untrusted: refuse before opening a stream or touching the doc.
            conn.close(1u32.into(), b"not trusted");
            return Ok(Accepted::Refused(who));
        };

        let (send, recv) = conn
            .accept_bi()
            .await
            .map_err(|e| Error::Transport(e.to_string()))?;
        drive(doc, send, recv, perms).await?;
        conn.close(0u32.into(), b"done");
        Ok(Accepted::Synced(who, perms))
    }
}
