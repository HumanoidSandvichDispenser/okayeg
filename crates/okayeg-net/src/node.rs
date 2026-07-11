//! The iroh endpoint, bound for okayeg sync.
//!
//! A [`Node`] is one peer on the mesh. Its secret key is its identity: the [`EndpointId`] others
//! dial, and, later, the key access control gates on. The drive loop in the crate root does the
//! actual protocol; this type just stands up the endpoint and hands it a bidirectional stream per
//! peer.

use std::time::Duration;

use iroh::address_lookup::PkarrResolver;
use iroh::endpoint::presets;
use iroh::endpoint::{Connection, RecvStream, SendStream};

use iroh::{Endpoint, EndpointAddr, EndpointId, SecretKey};
use okayeg::{Doc, Perms};

use crate::{
    ALPN, Accepted, Authorizer, Decision, Error, Identity, Transport, drive, encode_refused, hello,
    write_frame,
};

/// How long a dial may take before we give up and report the peer unreachable.
/// iroh keeps hole-punching indefinitely otherwise, so a dead endpoint never
/// returns. Long enough for a real handshake, short enough not to hang.
pub const DIAL_TIMEOUT: Duration = Duration::from_secs(15);

/// How long the accept side spends sending a refusal before moving on. Kept
/// short so a slow dialer can't stall the accept loop.
const REFUSE_TIMEOUT: Duration = Duration::from_secs(5);

/// Generate a fresh 32-byte secret.
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
        // The N0 preset resolves ids only over DNS off the system resolver. That
        // breaks on hosts whose resolv.conf lists a nameserver the process cannot
        // reach (Tailscale writes an IPv6 MagicDNS server that often times out,
        // and VPNs do similar), leaving a dial with no addressing information even
        // though the id is published. Add the HTTPS pkarr resolver the browser
        // build already uses so lookups have a path that never touches resolv.conf.
        let endpoint = Endpoint::builder(presets::N0)
            .address_lookup(PkarrResolver::n0_dns())
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

    /// Connect to `peer`, giving up after `timeout` so an unreachable or
    /// dial-only peer fails as [`Error::Unreachable`] instead of hanging.
    async fn connect(
        &self,
        peer: impl Into<EndpointAddr>,
        timeout: Duration,
    ) -> Result<Connection, Error> {
        let peer = peer.into();
        let id = peer.id;
        match tokio::time::timeout(timeout, self.endpoint.connect(peer, ALPN)).await {
            Ok(Ok(conn)) => Ok(conn),
            Ok(Err(e)) => Err(Error::Transport(e.to_string())),
            Err(_elapsed) => Err(Error::Unreachable(id.to_string())),
        }
    }

    /// Dial `peer` and run one full sync of `doc` against it, announcing
    /// `identity` and returning the peer's claimed one. Gives up on the dial
    /// after `timeout`.
    ///
    /// The dialer grants the peer full perms; access control is the accepting
    /// side's job. One-shot: used by `pull` to clone or catch up and exit. Live
    /// sessions go through [`Transport`] + [`drive_live`](crate::drive_live).
    pub async fn sync_with(
        &self,
        peer: impl Into<EndpointAddr>,
        doc: &Doc,
        identity: &Identity,
        timeout: Duration,
    ) -> Result<Identity, Error> {
        let conn = self.connect(peer, timeout).await?;
        let (mut send, mut recv) = conn
            .open_bi()
            .await
            .map_err(|e| Error::Transport(e.to_string()))?;

        let peer_identity = tokio::time::timeout(timeout, hello(&mut send, &mut recv, identity))
            .await
            .map_err(|_elapsed| Error::Transport("hello timed out".into()))??;

        drive(doc, send, recv, Perms::all()).await?;
        // Do not close the connection ourselves: that would send a QUIC
        // CONNECTION_CLOSE and abort the stream still carrying our last frame
        // before the peer reads it. Hold the link open until the accepting side
        // has consumed everything and closes, which it does once its own sync
        // completes.
        let _ = conn.closed().await;
        Ok(peer_identity)
    }
}

/// iroh as a live [`Transport`]: peers are [`EndpointId`]s, connections are QUIC
/// bi-streams, and the held [`Connection`] is the lifetime guard (dropping it
/// closes the link).
impl Transport for Node {
    type Id = EndpointId;
    type Send = SendStream;
    type Recv = RecvStream;
    type Guard = Connection;

    async fn dial(&self, peer: EndpointId) -> Result<(SendStream, RecvStream, Connection), Error> {
        let conn = self.connect(peer, DIAL_TIMEOUT).await?;
        let (send, recv) = conn
            .open_bi()
            .await
            .map_err(|e| Error::Transport(e.to_string()))?;
        Ok((send, recv, conn))
    }

    async fn accept<A>(&self, authz: &A) -> Result<Accepted<Self>, Error>
    where
        A: Authorizer<Id = EndpointId>,
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

        let message = match authz.authorize(who).await {
            Decision::Grant(perms) => {
                let (send, recv) = conn
                    .accept_bi()
                    .await
                    .map_err(|e| Error::Transport(e.to_string()))?;
                return Ok(Accepted::Peer {
                    who,
                    perms,
                    send,
                    recv,
                    guard: conn,
                });
            }
            Decision::Deny { message } => message,
        };

        // Refused. Send the message on the stream the dialer opened, then close,
        // so it gets a clear refusal instead of a bare drop. Wait for the write
        // to be acked before the connection drops, or the close cuts the frame
        // off mid-flight. Bounded, so a slow dialer can't stall the loop.
        let refuse = async {
            let (mut send, _recv) = conn
                .accept_bi()
                .await
                .map_err(|e| Error::Transport(e.to_string()))?;
            write_frame(&mut send, &encode_refused(message.as_deref())).await?;
            let _ = send.finish();
            let _ = send.stopped().await;
            Ok::<(), Error>(())
        };
        let _ = tokio::time::timeout(REFUSE_TIMEOUT, refuse).await;
        Ok(Accepted::Refused { who, message })
    }
}
