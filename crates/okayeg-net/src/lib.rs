//! The common transport: iroh p2p plus the loop that drives okayeg's sync protocol over a
//! connection.

use okayeg::{Doc, Msg, Step, Sync, SyncError};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

mod node;

pub use iroh::{EndpointAddr, EndpointId};
pub use node::{generate_secret, id_from_secret, Accepted, Node};
pub use okayeg::Perms;

/// The okayeg sync protocol, as named on the iroh wire.
pub const ALPN: &[u8] = b"okayeg/sync/0";

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
    let mut body = vec![0u8; len];
    recv.read_exact(&mut body).await?;
    Ok(body)
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
