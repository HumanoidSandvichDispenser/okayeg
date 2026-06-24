# Identity and Trust

A peer is known by a key. Authenticating a connection establishes who is on the
other end, and that is what access control needs, so it is all Okayeg requires
today. Signing your work, so that authorship travels with a change beyond the
connection it arrived on, is a separate concern. It is covered at the end of
this page and is not needed yet.

## Keys Authenticate Connections

The transport authenticates the channel, and Okayeg trusts the channel once the
transport has vouched for it. With iroh this is direct: you dial a peer by its
public key, and reaching that key is itself proof that you reached its holder, so
addressing and authentication are the same act. The connection already carries a
verified identity, and access control reads it straight off the connection.

Where holding a key is awkward, such as a browser that would rather not manage
one, a peer can instead present a per-user token: a secret the server issues
after an ordinary login, sent over the already-encrypted channel. A key proven by
possession and a token presented over the channel serve the same purpose, which
is to name the user. They are checked at the same gate.

## Trusting Keys

Trust is local. Each peer keeps its own record of trusted keys, the way SSH keeps
`known_hosts` and `allowed_signers`, and the record lives with the repo it
governs. Access control decides what each trusted key may do.

A key earns that trust in one of two ways. A personal key is trusted directly:
you add the key itself to your record, and it stands for the person who holds it.
A delegated key is trusted through an issuer: you trust the issuer's key once, and
it certifies the per-user keys it manages on behalf of accounts it has
authenticated. A delegated key says "the issuer vouches that an authenticated
user, xyz, did this" rather than "xyz holds this key", the way a service that
authenticates web users with its own keys vouches for the account behind each
one. The two meet at the same gate, since each is one distinct key the gate
checks; only the provenance of the trust differs.

An issuer that wants per-user access control has to mint a distinct key per user.
Because a connection is authenticated by its key, a single shared issuer key would
present one identity to every peer and could not be told apart, so identity would
have to move into asserted metadata and lose the guarantee the key was meant to
give. Per-user keys keep the connection proof meaningful: it identifies the user,
not just the issuer. Revocation follows from this, since the issuer drops or
rotates one user's key.

## The Gate

Access control is one record with an entry per credential, checked when a peer
connects and before any sync runs:

```text
credential -> { user, may pull, may push, revoked }
```

The credential is the connecting key, or a token standing in for it. The
permissions map onto the two directions of a sync. Permission to pull means the
peer answers your request with the updates you lack, that is, it streams its state
to you. Permission to push means the peer accepts the updates you send. A
read-only grant therefore streams out but refuses incoming changes, so the gate
shapes what a connection may do rather than only whether it may open.

## One Gate, Two Owners

The same gate runs whether a server is present or not; only who keeps the record
changes.

With a server, the server is an always-on peer that owns the canonical repo and a
managed record of trust. Web peers trust only the server and sync through it.
Native peers may do the same.

Without a server, each peer keeps its own record and trust is mutual and local:
you control who may push to your copy, and your collaborator controls who may push
to theirs. No central authority is required for this to be correct, because
merging concurrent changes is the CRDT's job and access is a local decision. Many
peers with no server form a mesh, or settle on one always-on peer that the others
join, which is a self-hosted server in all but name.

## Joining a Repo

Joining is a matter of securely handing a newcomer three things: where the peer
is, which repo, and a credential the peer already trusts. There are two ways to
deliver that bundle.

A warm join uses a channel you already have. A native user registers their key
once, the way you add an SSH key to a host, and from then on every clone and sync
just presents it. A web user logs in and opens the document, and the browser
quietly fetches its token.

A cold join, for someone with no prior channel, uses a one-time code. The code
is short and single-use and secured by PAKE, and redeeming it carries the
bundle across. Because the exchange runs both ways, the newcomer can also send
their own key back, so the two sides come away trusting each other.

## Signing (deferred)

Signing answers a question authentication does not: can authorship be verified
without trusting whoever relayed or stored a change. It matters when a change
reaches you through an intermediary, as when one peer's edit arrives by way of a
server, and you authenticate the server rather than the author. A signature
travels with the change and is checked against keys you already trust, so the
change keeps its authorship through any relay or disk along the way.

For a direct sync, or whenever you already trust the server or peer you are
talking to, the connection's identity already is the authorship, so signing buys
nothing and Okayeg does not require it yet. When it is added it covers the units
that cross a boundary, a proposal you submit and a checkpoint you record, and not
each real-time op, similar to how git signs commits and tags but not keystrokes. A
signing key can be GPG or SSH following git's `gpg.format`, so a single ed25519
key could sign both your git commits and your Okayeg checkpoints.

The connection proof below is only needed when the signing identity is a separate
key from the one the transport already authenticated. When the connection key is
the identity, the transport has already proven possession and no extra proof is
required.

The proof is a signature over a fresh, channel-bound challenge, similar to other
signing-key authentication methods. The steps are:

1. The transport establishes the secure channel and yields a channel binding
   value, such as its session key or a derived secret.
2. The verifying side sends a fresh random nonce to the connecting peer, which the
   peer must include in its proof.
3. The connecting peer signs, with its signing key, a transcript covering a
   context string, the channel binding, the verifier's nonce, its own nonce, and
   the key it claims:

   ```text
   sign("okayeg-auth-v1" || channel_binding || verifier_nonce || peer_nonce || claimed_key)
   ```

4. The verifier checks the signature against the claimed key, and against what
   that key is trusted and allowed to do.
