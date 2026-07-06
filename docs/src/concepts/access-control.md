# Access Control

Collaboration in okayeg is a gradient: you decide who connects, what they see,
and which of their changes become part of your copy.

## Admitting a Connection

When a peer connects, the repo asks its authorizer what the peer may do. The
transport has already established who is on the other end (see [Identity and
Trust](identity.md)); the authorizer turns that identity into a set of
capabilities: permission to pull, permission to push, both, or a refusal that
drops the connection before any sync runs.

By default the verdict comes from the repo's trust record on disk, with one
entry per credential. A repo can also name a command in its config, and Okayeg
runs that command for each incoming connection, passing the peer's identity and
reading the results from its output. This lets an application that embeds
Okayeg define its own authorization mechanism: the command can call the
application's API, query its database, or consult whatever system already knows
who may access the project.

```toml
# .eg/config.toml
[authz]
command = ["/usr/local/bin/my-authz", "arg 1"]  # should take the peer's identity on stdin
```

The config lives in `.eg/` alongside the repo's other private state, and like
everything under `.eg/` it is never synced or exported. A peer therefore has
no way to push you a config that grants itself access; whoever holds the
repo's directory decides its policy.

**NOTE:** we could also possibly use UDS for interprocess authorization.

## Sessions

The authorizer answers once, when the connection opens, and the
permissions/capabilities granted lives with that connection for as long as it
lasts. A host with an incoming session admitted with pull can send updates to
peer; one admitted with push also accepts the peer's updates in. Every action
during the session is checked against the session's own permissions, so a
running sync never reaches back out to the authorizer, which is identical to a
Unix file descriptor, where `open()` checks permissions once, and the
descriptor carries its access mode from then on, untouched by a later `chmod`.

## Revoking Access

Removing a peer's access means closing the peer's session. The session and its
associated permissions end together. To receive a new grant, the peer must make
a new connection, where the authorizer answers again with whatever policy holds
at that moment. Editing the trust record, or some database behind an authz
command, only covers future connection on its own. To cut off a session already
running, the serving side closes it. Demoting a peer, say from push to
read-only, works the same way: close the session and let the peer reconnect
under the new capabilities.

## Outbound: What You Share

Access control on the way out is at the level of **connections** and
**documents**:

- A connection is allowed before it can contribute.
- Within an allowed connection, you choose which documents a peer can see. A
  shared document is sent; an unshared one stays with you.

## Inbound: What You Take

When a peer sends you changes, they arrive as something you accept or deny.

- **Accept** imports the changes into your copy.
- **Deny** drops them. Denying is just the same as deciding to not import a
  change, and it doesn't affect the sender's copy.

## Enforcement

A remote enforces its own rules on what it accepts. When a push touches a
document the sender isn't permitted to write, the remote rejects the **whole**
push, so a single disallowed change can never sneak in alongside allowed ones.
