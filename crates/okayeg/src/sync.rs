//! The sync protocol, with no I/O of its own.
//!
//! Two peers converge in one round: each sends the version it has, each
//! replies with the updates the other is missing, each imports. That is the
//! whole exchange. This module is the protocol as a driver over messages, it
//! never touches a socket. The caller feeds it received messages and ships the
//! messages it hands back, over whatever channel it has (iroh, a websocket, an
//! in-memory pipe). That is what lets the same code run in a CLI and in a
//! browser tab, where blocking on a socket is not allowed.

use loro::{LoroError, VersionVector};

use crate::Doc;

/// A protocol message. The caller frames these on the wire (one per websocket
/// message, length-prefixed over a stream, and so on); the protocol only cares
/// about message boundaries, not how they are drawn.
pub enum Msg {
    /// "Here is the version I have." Carries an encoded [`VersionVector`].
    Have(Vec<u8>),
    /// "Here is what you were missing." Carries exported updates.
    Updates(Vec<u8>),
}

impl Msg {
    /// Serialize to bytes for the wire: a one byte tag, then the body.
    pub fn encode(&self) -> Vec<u8> {
        let (tag, body) = match self {
            Msg::Have(b) => (0u8, b),
            Msg::Updates(b) => (1u8, b),
        };
        let mut out = Vec::with_capacity(1 + body.len());
        out.push(tag);
        out.extend_from_slice(body);
        out
    }

    /// Parse a message off the wire.
    pub fn decode(bytes: &[u8]) -> Result<Self, SyncError> {
        let (tag, body) = bytes.split_first().ok_or(SyncError::Malformed)?;
        match tag {
            0 => Ok(Msg::Have(body.to_vec())),
            1 => Ok(Msg::Updates(body.to_vec())),
            _ => Err(SyncError::Malformed),
        }
    }
}

/// What a peer is allowed to do in a sync, mapped onto the two directions of
/// the exchange.
///
/// `pull` lets them read our state: we answer their version with the real
/// updates they lack. `push` lets them write to us: we import the updates they
/// send. Denying a direction does not change the message shape, it withholds
/// content, so a read-only peer still gets a (empty) reply and a peer we will
/// not accept writes from still has its updates read off the wire and dropped.
/// A peer allowed neither should never reach the protocol; that admission check
/// belongs to the caller, not here.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Perms {
    /// May read our state.
    pub pull: bool,
    /// May write to our state.
    pub push: bool,
}

impl Perms {
    /// Full access: a symmetric, ungated sync. What a dialer and in-process use
    /// get, and what both sides of a mutual-trust sync grant.
    pub const fn all() -> Self {
        Self {
            pull: true,
            push: true,
        }
    }
}

/// What the driver wants the caller to do next.
pub enum Step {
    /// Ship this message to the peer, then keep feeding what comes back.
    Send(Msg),
    /// Both sides have converged. Stop.
    Done,
}

/// Something went wrong driving the protocol.
#[derive(Debug)]
pub enum SyncError {
    /// A message did not parse.
    Malformed,
    /// A message arrived that does not fit the current step.
    OutOfOrder,
    /// Loro rejected an encoded version or a set of updates.
    Loro(LoroError),
}

impl std::fmt::Display for SyncError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SyncError::Malformed => write!(f, "malformed sync message"),
            SyncError::OutOfOrder => write!(f, "sync message arrived out of order"),
            SyncError::Loro(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for SyncError {}

impl From<LoroError> for SyncError {
    fn from(e: LoroError) -> Self {
        SyncError::Loro(e)
    }
}

enum State {
    AwaitHave,
    AwaitUpdates,
    Done,
}

/// One side of a sync exchange against a single peer.
///
/// Both peers run an identical driver: open with [`start`](Self::start), then
/// feed every received message to [`on`](Self::on) and ship whatever it returns
/// until it reports [`Step::Done`]. The exchange is symmetric, so it does not
/// matter who dialed whom.
pub struct Sync<'a> {
    doc: &'a Doc,
    state: State,
    perms: Perms,
}

impl<'a> Sync<'a> {
    /// Begin a full, ungated sync against the given doc.
    pub fn new(doc: &'a Doc) -> Self {
        Self::gated(doc, Perms::all())
    }

    /// Begin a sync that grants the peer only `perms`.
    ///
    /// Used by the side enforcing access control on whoever connected. The peer
    /// runs its own driver and need not know it is being gated.
    pub fn gated(doc: &'a Doc, perms: Perms) -> Self {
        Self {
            doc,
            state: State::AwaitHave,
            perms,
        }
    }

    /// The opening message to send the peer: the version we currently have.
    pub fn start(&self) -> Msg {
        Msg::Have(self.doc.version().encode())
    }

    /// Feed a message received from the peer, getting back the next step.
    pub fn on(&mut self, msg: Msg) -> Result<Step, SyncError> {
        match (&self.state, msg) {
            // The peer told us their version; reply with what they lack, but
            // only if they may pull. Otherwise withhold by computing updates
            // since our own version, which is nothing.
            (State::AwaitHave, Msg::Have(vv)) => {
                let from = if self.perms.pull {
                    VersionVector::decode(&vv)?
                } else {
                    self.doc.version()
                };
                let updates = self.doc.updates_since(&from)?;
                self.state = State::AwaitUpdates;
                Ok(Step::Send(Msg::Updates(updates)))
            }
            // The peer sent what we lacked; apply it only if they may push,
            // otherwise drop it. Either way we have converged as far as we will.
            (State::AwaitUpdates, Msg::Updates(bytes)) => {
                if self.perms.push {
                    self.doc.import(&bytes)?;
                }
                self.state = State::Done;
                Ok(Step::Done)
            }
            _ => Err(SyncError::OutOfOrder),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;

    /// Run two drivers against each other in process, passing messages through
    /// the wire format (encode/decode) the way a real channel would. No sockets.
    fn sync_pair(a: &Doc, b: &Doc) {
        sync_pair_gated(a, Perms::all(), b, Perms::all());
    }

    /// Like [`sync_pair`] but each side grants the other only the given perms,
    /// so a gated exchange can be exercised with no network.
    fn sync_pair_gated(a: &Doc, a_grants: Perms, b: &Doc, b_grants: Perms) {
        let mut sa = Sync::gated(a, a_grants);
        let mut sb = Sync::gated(b, b_grants);
        let mut to_a: VecDeque<Vec<u8>> = VecDeque::new();
        let mut to_b: VecDeque<Vec<u8>> = VecDeque::new();
        to_b.push_back(sa.start().encode());
        to_a.push_back(sb.start().encode());

        let (mut a_done, mut b_done) = (false, false);
        while !(a_done && b_done) {
            if let Some(bytes) = to_a.pop_front() {
                match sa.on(Msg::decode(&bytes).unwrap()).unwrap() {
                    Step::Send(m) => to_b.push_back(m.encode()),
                    Step::Done => a_done = true,
                }
            } else if let Some(bytes) = to_b.pop_front() {
                match sb.on(Msg::decode(&bytes).unwrap()).unwrap() {
                    Step::Send(m) => to_a.push_back(m.encode()),
                    Step::Done => b_done = true,
                }
            } else {
                panic!("ran dry before both sides converged");
            }
        }
    }

    #[test]
    fn one_sided_change_propagates() {
        let a = Doc::new();
        a.text("body").insert(0, "hello").unwrap();
        a.commit();
        let b = Doc::new();

        sync_pair(&a, &b);

        assert_eq!(b.text("body").to_string(), "hello");
    }

    #[test]
    fn divergent_edits_merge_both_ways() {
        // Both start from a shared base so their edits are concurrent, not one
        // built on the other.
        let a = Doc::new();
        a.text("body").insert(0, "x").unwrap();
        a.commit();
        let b = Doc::from_snapshot(&a.snapshot().unwrap()).unwrap();

        a.text("body").insert(1, "A").unwrap();
        a.commit();
        b.text("body").insert(1, "B").unwrap();
        b.commit();

        sync_pair(&a, &b);

        // Both converge to the same text, and it is not lost on either side.
        let merged = a.text("body").to_string();
        assert_eq!(merged, b.text("body").to_string());
        assert!(merged.contains('A') && merged.contains('B'), "got {merged:?}");
    }

    #[test]
    fn a_peer_denied_push_cannot_write_to_us() {
        // `keeper` will grant `writer` pull but not push. writer's change must
        // not land in keeper, while keeper's own state still flows to writer.
        let keeper = Doc::new();
        keeper.text("body").insert(0, "kept").unwrap();
        keeper.commit();
        let writer = Doc::from_snapshot(&keeper.snapshot().unwrap()).unwrap();
        writer.text("body").insert(4, "X").unwrap();
        writer.commit();

        // keeper grants writer { pull, !push }; writer (the dialer) grants all.
        sync_pair_gated(&keeper, Perms { pull: true, push: false }, &writer, Perms::all());

        assert_eq!(keeper.text("body").to_string(), "kept", "push leaked in");
        assert_eq!(writer.text("body").to_string(), "keptX", "pull was withheld");
    }

    #[test]
    fn a_peer_denied_pull_cannot_read_us() {
        // keeper grants reader { !pull, push }: reader may submit but learns
        // nothing of keeper's state.
        let keeper = Doc::new();
        keeper.text("body").insert(0, "secret").unwrap();
        keeper.commit();
        let reader = Doc::new();
        reader.text("body").insert(0, "mine").unwrap();
        reader.commit();

        sync_pair_gated(&keeper, Perms { pull: false, push: true }, &reader, Perms::all());

        // reader pushed to keeper, so keeper saw reader's ops...
        assert!(keeper.text("body").to_string().contains("mine"), "push was accepted");
        // ...but keeper's "secret" never reached reader.
        assert!(!reader.text("body").to_string().contains("secret"), "pull leaked out");
    }

    #[test]
    fn decode_rejects_an_unknown_tag() {
        assert!(matches!(Msg::decode(&[]), Err(SyncError::Malformed)));
        assert!(matches!(Msg::decode(&[9, 1, 2]), Err(SyncError::Malformed)));
    }
}
