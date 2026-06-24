# Transports

Okayeg's sync protocol moves bytes and nothing more. Syncing a doc comes down
to "here is the version I have, send me what I'm missing," along with signed
proposals and checkpoints (see [Sync](sync.md)). All of that is a stream of
bytes between two peers, and Okayeg leaves the job of carrying those bytes to a
transport.

A transport is an adapter that moves bytes between two peers and authenticates
the channel. Any transport that can do those two things can carry Okayeg, so the
same doc can sync over whichever one fits the situation.

## The Transports

- **iroh** connects peers directly, peer to peer, finding a route through NATs.
  This is the ideal path for real-time sessions, where changes stream live
  between peers.
- **SSH** reaches a peer over an SSH connection, reusing the keys and access you
  already have on that host.
- **Static HTTP** fetches a doc from an ordinary web host. A static or
  read-only server is enough to publish a doc for others to pull.
- **WebSocket over HTTPS** establishes a bidirectional channel through a web
  server, so a browser can sync with a peer.
- **file** writes the bytes to a disk or a USB stick and carries them by hand, so
  two peers can sync without ever being online at the same time.

## Addresses and Authentication

A remote's addresses (see [Remotes](remotes.md)) are transport-specific routes:
an ssh url, an iroh node id, an https url, a filesystem path. The address says
which transport to use and where to reach the peer.

Each transport authenticates the channel its own way (SSH keys, a TLS
certificate, an iroh node id) and Okayeg trusts the channel once the transport
has vouched for it. Authorship is settled separately by signatures (see
[Identity and signing](identity.md)).
