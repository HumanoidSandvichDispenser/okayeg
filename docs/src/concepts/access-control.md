# Access Control

Collaboration in okayeg is a gradient: you decide who connects, what they see,
and which of their changes become part of your copy.

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
