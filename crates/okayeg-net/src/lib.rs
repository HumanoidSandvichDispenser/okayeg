//! The common transport: iroh p2p plus the loop that drives okayeg's sync protocol over a
//! connection.

use std::rc::Rc;

use okayeg::{Doc, LiveSync, Msg, Step, Sync, SyncError};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::broadcast;

mod authz;
#[cfg(feature = "native")]
mod node;

pub use authz::{from_fn, Authorizer, FnAuthorizer};
#[cfg(feature = "native")]
pub use authz::CommandAuthorizer;
#[cfg(feature = "native")]
pub use iroh::{EndpointAddr, EndpointId};
#[cfg(feature = "native")]
pub use node::{generate_secret, id_from_secret, Node};
pub use okayeg::Perms;

/// A way to obtain live duplex connections to peers.
///
/// This abstracts the iroh endpoint behind the two operations the live runtime
/// needs, dialing one peer and accepting the next, so the same runtime can run
/// over an in-memory pipe in tests (see [`MemTransport`]). The protocol itself
/// is [`drive_live`], already generic over the stream halves.
#[allow(async_fn_in_trait)]
pub trait Transport {
    /// How a peer is named and gated.
    type Id: Copy + Eq + std::fmt::Display + 'static;
    /// The write half of a connection.
    type Send: AsyncWrite + Unpin + 'static;
    /// The read half of a connection.
    type Recv: AsyncRead + Unpin + 'static;
    /// Held for the connection's lifetime; dropping it tears the link down.
    type Guard: 'static;

    /// Dial `peer`, returning its duplex stream and a lifetime guard.
    async fn dial(
        &self,
        peer: Self::Id,
    ) -> Result<(Self::Send, Self::Recv, Self::Guard), Error>;

    /// Accept the next peer, gating it by id before handing back its stream.
    ///
    /// `authz` is the authz hook: it resolves the peer's id to its [`Perms`], or
    /// `None` to refuse. A closure works, or use [`CommandAuthorizer`] to defer
    /// the decision to an external script.
    async fn accept<A>(&self, authz: &A) -> Result<Accepted<Self>, Error>
    where
        A: Authorizer<Id = Self::Id>;
}

/// The outcome of [`Transport::accept`].
pub enum Accepted<T: Transport + ?Sized> {
    /// The gate refused this peer.
    Refused(T::Id),
    /// A trusted peer with its stream, ready to hand to [`drive_live`].
    Peer {
        who: T::Id,
        perms: Perms,
        send: T::Send,
        recv: T::Recv,
        /// Hold this for the session; dropping it closes the link.
        guard: T::Guard,
    },
}

/// A doc shared between the live drivers, the watcher, and the exporter.
pub type Shared = Rc<Doc>;

/// The okayeg sync protocol, as named on the iroh wire.
pub const ALPN: &[u8] = b"okayeg/sync/0";

/// Largest frame we will read to not allow a peer to make us allocate more than this much at once.
const MAX_FRAME: usize = 64 << 20;

/// Something went wrong moving sync bytes.
#[derive(Debug)]
pub enum Error {
    /// The transport (iroh, or the underlying stream) failed.
    Transport(String),
    /// The peer spoke the protocol wrong.
    Protocol(SyncError),
    /// A framed read or write failed.
    Io(std::io::Error),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Transport(s) => write!(f, "transport: {s}"),
            Error::Protocol(e) => write!(f, "protocol: {e}"),
            Error::Io(e) => write!(f, "io: {e}"),
        }
    }
}

impl std::error::Error for Error {}

impl From<SyncError> for Error {
    fn from(e: SyncError) -> Self {
        Error::Protocol(e)
    }
}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error::Io(e)
    }
}

/// Drive one sync exchange against a peer over a duplex byte stream, granting
/// the peer `perms`. Messages are length prefixed: a four byte big endian
/// length, then the body. Pass [`Perms::all`] for an ungated, symmetric sync.
pub async fn drive<W, R>(doc: &Doc, mut send: W, mut recv: R, perms: Perms) -> Result<(), Error>
where
    W: AsyncWrite + Unpin,
    R: AsyncRead + Unpin,
{
    let mut sync = Sync::gated(doc, perms);
    write_frame(&mut send, &sync.start().encode()).await?;
    loop {
        let frame = read_frame(&mut recv).await?;
        match sync.on(Msg::decode(&frame)?)? {
            Step::Send(msg) => write_frame(&mut send, &msg.encode()).await?,
            Step::Done => break,
        }
    }
    send.flush().await?;
    Ok(())
}

/// Drive a held-open live sync: catch the peer up, then stay connected,
/// streaming each local commit out and importing the peer's as they arrive.
///
/// `changed` is the repo-wide nudge: this subscribes to know when to push, and
/// fires it after an import that moved the doc so the other peers and the
/// exporter react. Returns when the stream closes or errors.
pub async fn drive_live<W, R>(
    doc: Shared,
    mut send: W,
    recv: R,
    perms: Perms,
    changed: broadcast::Sender<()>,
) -> Result<(), Error>
where
    W: AsyncWrite + Unpin,
    R: AsyncRead + Unpin,
{
    let mut nudged = changed.subscribe();
    let mut live = LiveSync::new(perms);

    write_frame(&mut send, &live.start(&doc).encode()).await?;

    // this exists so nudge doesn't just drop the in-flight read half way through a frame
    async fn next_frame<R: AsyncRead + Unpin>(mut recv: R) -> (R, Result<Vec<u8>, Error>) {
        let frame = read_frame(&mut recv).await;
        (recv, frame)
    }

    let read = next_frame(recv);
    tokio::pin!(read);

    loop {
        tokio::select! {
            biased;
            (recv, frame) = &mut read => {
                let out = live.on(&doc, Msg::decode(&frame?)?)?;
                read.set(next_frame(recv));
                if let Some(msg) = out.send {
                    write_frame(&mut send, &msg.encode()).await?;
                }
                if out.changed {
                    let _ = changed.send(());
                }
            }
            nudge = nudged.recv() => {
                if let Err(broadcast::error::RecvError::Closed) = nudge {
                    break;
                }
                // Ok or Lagged: either way, re-measure and push the gap.
                if let Some(msg) = live.pending(&doc)? {
                    write_frame(&mut send, &msg.encode()).await?;
                }
            }
        }
    }
    Ok(())
}

async fn write_frame<W: AsyncWrite + Unpin>(send: &mut W, body: &[u8]) -> Result<(), Error> {
    let len = u32::try_from(body.len())
        .map_err(|_| Error::Transport("message too large to frame".into()))?;
    send.write_all(&len.to_be_bytes()).await?;
    send.write_all(body).await?;
    Ok(())
}

async fn read_frame<R: AsyncRead + Unpin>(recv: &mut R) -> Result<Vec<u8>, Error> {
    let mut len = [0u8; 4];
    recv.read_exact(&mut len).await?;
    let len = u32::from_be_bytes(len) as usize;
    if len > MAX_FRAME {
        return Err(Error::Transport(format!(
            "frame of {len} bytes exceeds {MAX_FRAME} cap"
        )));
    }
    let mut body = vec![0u8; len];
    recv.read_exact(&mut body).await?;
    Ok(body)
}

/// An in-memory [`Transport`] pair for tests: no sockets, no iroh. A `dial` on
/// one end surfaces at the other end's `accept`.
#[cfg(any(test, feature = "testing"))]
mod mem {
    use super::*;
    use tokio::io::{duplex, split, DuplexStream, ReadHalf, WriteHalf};
    use tokio::sync::{mpsc, Mutex};

    pub struct MemTransport {
        id: u64,
        to_peer: mpsc::UnboundedSender<(u64, DuplexStream)>,
        inbound: Mutex<mpsc::UnboundedReceiver<(u64, DuplexStream)>>,
    }

    impl MemTransport {
        /// A connected pair; each side's `dial` is received by the other's `accept`.
        pub fn pair() -> (MemTransport, MemTransport) {
            let (a_tx, a_rx) = mpsc::unbounded_channel();
            let (b_tx, b_rx) = mpsc::unbounded_channel();
            (
                MemTransport { id: 1, to_peer: b_tx, inbound: Mutex::new(a_rx) },
                MemTransport { id: 2, to_peer: a_tx, inbound: Mutex::new(b_rx) },
            )
        }
    }

    impl Transport for MemTransport {
        type Id = u64;
        type Send = WriteHalf<DuplexStream>;
        type Recv = ReadHalf<DuplexStream>;
        type Guard = ();

        async fn dial(&self, _peer: u64) -> Result<(Self::Send, Self::Recv, ()), Error> {
            let (mine, theirs) = duplex(64 * 1024);
            self.to_peer
                .send((self.id, theirs))
                .map_err(|_| Error::Transport("peer gone".into()))?;
            let (recv, send) = split(mine);
            Ok((send, recv, ()))
        }

        async fn accept<A>(&self, authz: &A) -> Result<Accepted<Self>, Error>
        where
            A: Authorizer<Id = u64>,
        {
            let (who, stream) = self
                .inbound
                .lock()
                .await
                .recv()
                .await
                .ok_or_else(|| Error::Transport("peer gone".into()))?;
            let Some(perms) = authz.authorize(who).await else {
                return Ok(Accepted::Refused(who));
            };
            let (recv, send) = split(stream);
            Ok(Accepted::Peer { who, perms, send, recv, guard: () })
        }
    }
}

#[cfg(any(test, feature = "testing"))]
pub use mem::MemTransport;

#[cfg(test)]
mod tests {
    use super::*;
    use std::rc::Rc;
    use std::time::Duration;

    /// Poll `pred` until true, yielding to let spawned tasks run; fail on timeout.
    async fn converge<F: Fn() -> bool>(pred: F) {
        for _ in 0..400 {
            if pred() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        panic!("did not converge in time");
    }

    #[tokio::test]
    async fn mem_transport_runtimes_converge_both_ways() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let (a, b) = MemTransport::pair();
                let doc_a: Shared = Rc::new(Doc::new());
                let doc_b: Shared = Rc::new(Doc::from_snapshot(&doc_a.snapshot().unwrap()).unwrap());
                let (ca, _) = broadcast::channel::<()>(64);
                let (cb, _) = broadcast::channel::<()>(64);

                // acceptor: wait for the dialer, then drive its side live
                let bd = doc_b.clone();
                let bc = cb.clone();
                tokio::task::spawn_local(async move {
                    if let Ok(Accepted::Peer { send, recv, perms, .. }) =
                        b.accept(&from_fn(|_: u64| async { Some(Perms::all()) })).await
                    {
                        let _ = drive_live(bd, send, recv, perms, bc).await;
                    }
                });
                // dialer
                let ad = doc_a.clone();
                let ac = ca.clone();
                tokio::task::spawn_local(async move {
                    let (send, recv, _g) = a.dial(2).await.unwrap();
                    let _ = drive_live(ad, send, recv, Perms::all(), ac).await;
                });

                // a's edit reaches b
                doc_a.text("body").insert(0, "hello").unwrap();
                doc_a.commit();
                let _ = ca.send(());
                converge(|| doc_b.text("body").to_string() == "hello").await;

                // b's concurrent edit reaches a; both converge
                doc_b.text("body").insert(5, " world").unwrap();
                doc_b.commit();
                let _ = cb.send(());
                converge(|| doc_a.text("body").to_string() == "hello world").await;
            })
            .await;
    }

    #[tokio::test]
    async fn read_frame_rejects_oversized_length() {
        let header = (MAX_FRAME as u32 + 1).to_be_bytes();
        let mut src = &header[..];
        let err = read_frame(&mut src).await.unwrap_err();
        assert!(matches!(err, Error::Transport(_)), "got {err:?}");
    }

    #[tokio::test]
    async fn drive_converges_over_a_pipe() {
        let a = Doc::new();
        a.text("body").insert(0, "base").unwrap();
        a.commit();
        // b starts from the same base so the two edits are concurrent.
        let b = Doc::from_snapshot(&a.snapshot().unwrap()).unwrap();
        a.text("body").insert(4, "A").unwrap();
        a.commit();
        b.text("body").insert(4, "B").unwrap();
        b.commit();

        let (one, two) = tokio::io::duplex(64 * 1024);
        let (one_r, one_w) = tokio::io::split(one);
        let (two_r, two_w) = tokio::io::split(two);

        let (ra, rb) = tokio::join!(
            drive(&a, one_w, one_r, Perms::all()),
            drive(&b, two_w, two_r, Perms::all())
        );
        ra.unwrap();
        rb.unwrap();

        let merged = a.text("body").to_string();
        assert_eq!(merged, b.text("body").to_string());
        assert!(merged.contains('A') && merged.contains('B'), "got {merged:?}");
    }

    // Regression for the C1 desync: a `changed` nudge that fires while a frame is
    // only half-read must not discard the partial read. We feed `drive_live` a
    // frame's length header, fire a nudge before the body arrives, then deliver
    // the body. If the in-flight read were dropped (the pre-fix behaviour), the
    // header bytes would be lost and the body would be misframed, so the doc would
    // never receive the edit.
    #[tokio::test]
    async fn nudge_mid_frame_keeps_the_stream_aligned() {
        use tokio::io::AsyncWriteExt;

        tokio::task::LocalSet::new()
            .run_until(async {
                // A peer edit, encoded as exactly one Updates frame.
                let a: Shared = Rc::new(Doc::new());
                let b = Doc::from_snapshot(&a.snapshot().unwrap()).unwrap();
                b.text("body").insert(0, "hello").unwrap();
                b.commit();
                let updates = b.updates_since(&a.version()).unwrap();
                let frame = Msg::Updates(updates).encode();
                let header = (frame.len() as u32).to_be_bytes();

                let (mut feed, a_recv) = tokio::io::duplex(64 * 1024);
                let (changed, _) = broadcast::channel::<()>(64);

                let ad = a.clone();
                let drive_changed = changed.clone();
                tokio::task::spawn_local(async move {
                    let _ =
                        drive_live(ad, tokio::io::sink(), a_recv, Perms::all(), drive_changed).await;
                });

                // Deliver only the header, then let drive_live park mid-frame.
                feed.write_all(&header).await.unwrap();
                tokio::time::sleep(Duration::from_millis(10)).await;

                // The nudge lands while the body is still outstanding.
                let _ = changed.send(());
                tokio::time::sleep(Duration::from_millis(10)).await;

                // Body arrives; an aligned reader completes and applies the frame.
                feed.write_all(&frame).await.unwrap();

                converge(|| a.text("body").to_string() == "hello").await;
            })
            .await;
    }
}
