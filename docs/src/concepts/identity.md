# Identity and Signing

Authenticating a connection is the transport's job (an SSH key, a TLS
certificate, an iroh node id) and Okayeg trusts a channel once the transport
has vouched for it. Signing your work is okayeg's own concern, and the rest of
this page is about that.

A signing key marks a change as yours. The signature travels with the change
and is verified against keys the receiver already trusts, so a change keeps its
authorship through any relay or disk it passes along the way.

## What Gets Signed

A signature covers the units that cross a boundary: a proposal you submit and a
checkpoint you record. Importing a change merges its ops into the event graph
and drops the envelope around them, so the signature is kept alongside the doc.
Real-time ops flow unsigned, and the signature goes on the proposal that opens
a session and the checkpoint that closes it, similar to how git signs commits
and tags the same way.

## Formats

A signing key can be GPG or SSH, following git's `gpg.format`. SSH ed25519 is
the default and GPG is supported, and you can bring an ed25519 SSH key you
already use, so you could set up a configuration where a single key signs both
your git commits and your Okayeg checkpoints. Each signature records its format
and key.

## Trusting Keys

Trust is local. Each peer keeps its own record of trusted keys, the way SSH
keeps `known_hosts` and `allowed_signers`, and access control decides what each
trusted key may do.

## Proving Identity on Connection

On connecting, a peer proves possession of its signing key, so every peer,
whether it reads or writes, presents one identity that read decisions and
signed content both check against.

The proof is a signature over a fresh, channel-bound challenge, similar to
other signing key-based authentication methods. The steps are:

1. The transport establishes the secure channel and yields a channel binding
   value, such as its session key or a derived secret.
2. The verifying side sends a fresh random nonce to the connecting peer, which
   the peer must include in its proof
3. The connecting peer signs, with its signing key, a transcript covering a
   context string, the channel binding, the verifier's nonce, its own nonce,
   and the key it claims:

   ```text
   sign("okayeg-auth-v1" || channel_binding || verifier_nonce || peer_nonce || claimed_key)
   ```

4. The verifier checks the signature against the claimed key, and against what
   that key is trusted and allowed to do.
