# Okayeg

Local-first, real-time, conflict-free file sync, built on
[Loro](https://loro.dev) (an implementation of eg-walker).

Each peer keeps its own full copy of a directory as an event graph of edits.
You work offline, decide when to sync, and merge concurrent changes. On top of
that sits per-connection, per-document access control and per-peer identity, so
you also decide who connects, what they see, and whose changes get into your
copy.

## Building

Needs Rust 1.85 or newer (edition 2024). The `eg mount` command needs FUSE
(`fuse3` on Linux). Right now FUSE only works as a read-only mount. In the
future, it will support writing to the mount as well.

```sh
cargo build --release
```

## Using `eg`

`eg` turns a directory of text files into a synced okayeg repo. It keeps its
private state (your key, the doc snapshot, the trust set) in a `.eg/`
directory.

### Local snapshots

```sh
eg snapshot ./project project.eg   # directory -> doc snapshot (.eg/doc)
eg restore project.eg ./project    # doc snapshot (.eg/doc) -> directory
eg watch ./project project.eg      # keep the doc in sync with the directory
```

### Syncing two machines

Currently, this uses iroh (QUIC) to establish P2P connections. In the future,
Okayeg will support other transports.

To share a repo, one peer must serve it and the other must join it. The process
is:

```sh
# On forsen's machine:
eg serve
# -> "ab7302c2..." forsen can now post this code for anyone to join

# Anyone else can join forsen's endpoint by running:
eg id
```

By default, a peer can only pull from another peer if the other peer has
granted them trust (the authorization mechanism can also be overrided with
hooks).

If Forsen wants to let Wesker pull from his endpoint, he can run:

```sh
eg trust add <wesker-endpoint-id> pull
```

Then Wesker can pull from Forsen's endpoint:

```sh
eg join ab7302c2...
```

Wesker can also pull once, without joining:

```sh
eg pull <alice-endpoint-id>
```

### Inspecting a repo

```sh
eg status               # this repo's id, doc contents, and trust set
eg ls [path]            # list files in the doc
eg cat <path>...        # print file contents from the doc
eg trust list           # show current grants
```

Most commands find the repo by walking up to the nearest `.eg/`. To run in a
specific directory containing a `.eg` directory, use `-C <dir>`.

### Access control

By default the granted permissions per session for each incoming connection
comes from `.eg/trust`. A repo can instead specify a command in
`.eg/config.toml` that decides each connection. This is useful for embedding
okayeg in a larger system with its own identity and access control.


```toml
# .eg/config.toml
[authz]
command = ["/usr/local/bin/my-authz"]   # peer id on stdin, pull/push on stdout
```

A grant is checked once, when the connection opens, and lasts the life of that
session. Revoking a peer means closing its session, not editing a flag it would
recheck; the peer must reconnect to get a new grant.

## This is like Teamtype, why not use or fork Teamtype?

My initial main motiviation for working on this project was for a
synchronization engine for another project, which provides a Google Docs-like
experience with Typst that allows for both web-based and local-first editing. I
initially forked Teamtype to get a WASM build, and then realized that the core
model of Teamtype didn't fit my needs:

- Teamtype's model is more focused on ephemeral P2P editing
- It does not include access control (mostly explained to the reason above)
- Uses Automerge CRDT. Loro only uses CRDTs when merging concurrent edits, and
  otherwise uses a more efficient event graph model.

Still, because Teamtype is already relatively well-established, I will aim to
interop with editor plugins targeting Teamtype as it is mostly a simple JSON
RPC protocol.

## just use git 4Head LOOL

I will not be teaching my product owner how to use git or resolve conflicts.

## License

AGPL-3.0-only.
