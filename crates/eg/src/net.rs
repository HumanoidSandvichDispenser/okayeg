//! The networked sync commands: serve a doc over iroh and pull from a peer.
//!
//! Both load a directory into a doc, run one okayeg sync over iroh, and write
//! the merged result back out. `serve` keeps listening and rewrites the
//! directory after each peer; `pull` dials a peer once and exits.

use std::cell::RefCell;
use std::io;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::str::FromStr;
use std::time::Duration;

use notify::RecursiveMode;
use notify_debouncer_full::{DebounceEventResult, new_debouncer};
use okayeg::{Doc, LoroValue, NodeKind, Presence};
use okayeg_net::{
    Accepted, Authorizer, CommandAuthorizer, Decision, EndpointId, Node, Perms, PresenceLink,
    Shared, Transport, drive_live,
};
use tokio::sync::broadcast;

use crate::bridge::{export_tree, import_tree};
use crate::config::Config;
use crate::to_io;
use crate::trust::{self, Trust};
use crate::watch;
use crate::workspace::{CapWorkspace, Workspace};

use crate::EG_DIR;

/// This node's secret key, the raw form of its identity. Owner-only.
const KEY_PATH: &str = ".eg/key";

// kept across runs so node ids stay stable; rebuilding from files each run duplicates them on merge
const DOC_PATH: &str = ".eg/doc";

/// How long a presence entry lives without a refresh.
const PRESENCE_TIMEOUT: Duration = Duration::from_secs(30);

/// A peer's self-asserted name and email, read from its presence entry.
/// Display only, like a git commit author.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct Identity {
    name: Option<String>,

    email: Option<String>,
}

/// Wire this node's presence: set its own entry under `key`, feed local
/// updates into the relay, and keep the entry fresh while sweeping expired
/// peers. Returns the subscription, which can be dropped to stop feeding the
/// relay.
fn wire_presence(
    presence: &Presence,
    relay: &broadcast::Sender<(String, Vec<u8>)>,
    key: String,
    identity: &Identity,
) -> okayeg::Subscription {
    let sub = {
        let relay = relay.clone();
        let key = key.clone();
        presence.subscribe_local_updates(Box::new(move |bytes| {
            let _ = relay.send((key.clone(), bytes.clone()));
            true
        }))
    };

    let entry = presence_value(identity);
    presence.set(&key, entry.clone());

    // re-set well inside the timeout so the entry never expires while we live
    let keep = presence.clone();
    tokio::task::spawn_local(async move {
        let mut tick = tokio::time::interval(PRESENCE_TIMEOUT / 3);
        loop {
            tick.tick().await;
            keep.set(&key, entry.clone());
            keep.remove_outdated();
        }
    });

    sub
}

/// The presence entry for `identity`: its name and email as a map, unset
/// fields left out.
fn presence_value(identity: &Identity) -> LoroValue {
    let mut map = std::collections::HashMap::new();
    if let Some(name) = &identity.name {
        map.insert("name".to_string(), LoroValue::from(name.as_str()));
    }
    if let Some(email) = &identity.email {
        map.insert("email".to_string(), LoroValue::from(email.as_str()));
    }
    LoroValue::from(map)
}

/// The identity announced in this repo's presence entry: `[identity]` from the
/// config, falling back per field to git config for anything unset. An empty
/// string in the config keeps the field blank.
fn local_identity(config: &Config, dir: &Path) -> Identity {
    let field = |set: &Option<String>, key: &str| match set {
        Some(s) => (!s.is_empty()).then(|| s.clone()),
        None => git_config(dir, key),
    };

    Identity {
        name: field(&config.name, "user.name"),
        email: field(&config.email, "user.email"),
    }
}

/// Read one git config value through the git binary, repo-local when `dir` is
/// inside a git repo, global otherwise. `None` when git or the value is absent.
fn git_config(dir: &Path, key: &str) -> Option<String> {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["config", "--get", key])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }

    let value = String::from_utf8(out.stdout).ok()?;
    let value = value.trim();
    (!value.is_empty()).then(|| value.to_owned())
}

/// A peer's display label: the claimed name and email alongside its key. The
/// claimed part is peer text, so control characters are stripped.
fn label(identity: &Identity, who: &EndpointId) -> String {
    let clean = |s: &str| -> String { s.chars().filter(|c| !c.is_control()).collect() };

    let mut parts = Vec::new();
    if let Some(name) = &identity.name {
        parts.push(clean(name));
    }
    if let Some(email) = &identity.email {
        parts.push(format!("<{}>", clean(email)));
    }
    if parts.is_empty() {
        return who.to_string();
    }

    parts.push(format!("({who})"));
    parts.join(" ")
}

/// Report a session event to the `[session] command` hook, when one is
/// configured. Fire and forget: the hook decides nothing, a failure only logs.
fn run_session_hook(cmd: &Rc<Option<Vec<String>>>, event: &str, who: &EndpointId) {
    let Some(cmd) = cmd.as_ref() else { return };

    let stdin_body = format!("{event}\n{who}\n");
    let child = tokio::process::Command::new(&cmd[0])
        .args(&cmd[1..])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .spawn();

    let program = cmd[0].clone();
    tokio::task::spawn_local(async move {
        let run = async {
            let mut child = child?;
            if let Some(mut stdin) = child.stdin.take() {
                use tokio::io::AsyncWriteExt;
                stdin.write_all(stdin_body.as_bytes()).await?;
            }
            child.wait().await
        };
        if let Err(e) = run.await {
            eprintln!("eg serve: session hook {program}: {e}");
        }
    });
}

/// The identity a peer claims in its presence entry, empty when none arrived.
fn presence_identity(presence: &Presence, who: &EndpointId) -> Identity {
    let Some(LoroValue::Map(fields)) = presence.get(&who.to_string()) else {
        return Identity::default();
    };
    let text = |key: &str| match fields.get(key) {
        Some(LoroValue::String(s)) => Some(s.to_string()),
        _ => None,
    };

    Identity {
        name: text("name"),
        email: text("email"),
    }
}

/// Run an async command on a small current-thread runtime, inside a `LocalSet`
/// so the live runtime can `spawn_local` tasks that hold the `!Send` doc.
fn block_on<F>(fut: F) -> std::io::Result<()>
where
    F: std::future::Future<Output = std::io::Result<()>>,
{
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    tokio::task::LocalSet::new().block_on(&rt, fut)
}

/// This repo's stable secret key, generating and persisting it on first use.
///
/// The key lives in `.eg/key` under the served directory, written 0600. Reusing
/// it keeps the node's [`EndpointId`](okayeg_net::EndpointId) the same across
/// restarts, so a peer can dial the same address twice and so trust can pin it.
fn repo_secret(ws: &dyn Workspace) -> io::Result<[u8; 32]> {
    match ws.read_file(Path::new(KEY_PATH)) {
        Ok(bytes) => bytes.as_slice().try_into().map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                ".eg/key is not 32 bytes; remove it to regenerate",
            )
        }),
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            let secret = okayeg_net::generate_secret();
            ws.write_new_secret(Path::new(KEY_PATH), &secret)?;
            Ok(secret)
        }
        Err(e) => Err(e),
    }
}

fn open_or_seed(ws: &dyn Workspace) -> io::Result<Doc> {
    match read_doc(ws)? {
        Some(doc) => Ok(doc),
        None => {
            let doc = Doc::new();
            import_tree(ws, &doc)?;
            Ok(doc)
        }
    }
}

fn open_or_clone(ws: &dyn Workspace) -> io::Result<Doc> {
    match read_doc(ws)? {
        Some(doc) => Ok(doc),
        None if is_empty_repo(ws)? => Ok(Doc::new()),
        None => Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "refusing to clone into a non-empty directory; clone into an empty one",
        )),
    }
}

fn read_doc(ws: &dyn Workspace) -> io::Result<Option<Doc>> {
    match ws.read_file(Path::new(DOC_PATH)) {
        Ok(bytes) => Ok(Some(Doc::from_snapshot(&bytes).map_err(to_io)?)),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

/// Load the doc last stored in this repo, failing when there is none yet.
///
/// One of three answers to a missing doc: `open_or_clone` requires an empty
/// directory and starts fresh, `open_or_seed` imports the working tree, and
/// this refuses. They could share a doc-opening seam once a fourth appears.
pub(crate) fn load_doc(dir: &Path) -> io::Result<Doc> {
    let ws = open_repo(dir)?;
    read_doc(&ws)?.ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            "no doc here yet; run serve, pull, or join first",
        )
    })
}

fn is_empty_repo(ws: &dyn Workspace) -> io::Result<bool> {
    use crate::workspace::Entry;
    Ok(ws.read_dir(Path::new(""))?.iter().all(|e| match e {
        Entry::Dir(name) | Entry::File(name) => name == EG_DIR,
    }))
}

fn store(doc: &Doc, ws: &dyn Workspace) -> io::Result<Vec<PathBuf>> {
    ws.write_private(Path::new(DOC_PATH), &doc.snapshot().map_err(to_io)?)?;
    export_tree(doc, ws)
}

/// Serve `dir` over iroh: watch local edits into the doc and sync every peer
/// that connects, live, until interrupted.
pub fn serve(dir: &Path) -> std::io::Result<()> {
    let base = dir.canonicalize()?;
    let ws = Rc::new(open_repo(dir)?);
    block_on(async move {
        let doc: Shared = Rc::new(open_or_seed(&*ws)?);
        store(&doc, &*ws)?;

        let node = Node::bind_with_secret(repo_secret(&*ws)?)
            .await
            .map_err(to_io)?;
        let _ = node.addr().await;
        let changed = spawn_watch_and_export(ws.clone(), base.clone(), doc.clone())?;

        println!("eg serve: listening as {}", node.id());

        let config = Config::load(&*ws)?;
        let me = Rc::new(local_identity(&config, &base));
        let session_hook = Rc::new(config.session_command.clone());

        let presence = Presence::new(PRESENCE_TIMEOUT.as_millis() as i64);
        let (relay, _) = broadcast::channel(64);
        let _presence_sub = wire_presence(&presence, &relay, node.id().to_string(), &me);

        // The gate deciding each incoming connection: the authz command from
        // .eg/config.toml when one is configured, the trust file otherwise. The
        // config is read once here at startup; a policy change reaches a running
        // session only by closing it (the verdict lives with the connection).
        let gate = match config.authz_command {
            Some(cmd) => {
                println!("  authz: {}", cmd.join(" "));
                let mut authz = CommandAuthorizer::new(&cmd[0]);
                for arg in &cmd[1..] {
                    authz = authz.arg(arg);
                }
                Gate::Command(authz)
            }
            None => {
                println!("  trust a peer: eg trust <their-id> [pull] [push]");
                Gate::Trust(ws.clone())
            }
        };

        let sessions: Sessions = Rc::new(RefCell::new(Vec::new()));
        spawn_repl(
            sessions.clone(),
            presence.clone(),
            base,
            session_hook.clone(),
        );

        loop {
            match node.accept(&gate).await.map_err(to_io)? {
                Accepted::Peer {
                    who,
                    perms,
                    send,
                    recv,
                    guard,
                } => {
                    let doc = doc.clone();
                    let changed = changed.clone();
                    let hook = session_hook.clone();
                    let link = PresenceLink {
                        presence: presence.clone(),
                        relay: relay.clone(),
                        owner: Some(who.to_string()),
                    };

                    // the claimed name arrives through presence a moment after
                    // the session opens, so the join line carries the bare id;
                    // the sessions command shows the name once it lands
                    println!("eg serve: {who} joined ({})", trust::flags(perms));
                    run_session_hook(&hook, "hello", &who);

                    let handle = tokio::task::spawn_local(async move {
                        let _guard = guard; // hold the link open for the session
                        if let Err(e) =
                            drive_live(doc, send, recv, perms, changed, Some(link)).await
                        {
                            eprintln!("eg serve: {who} dropped: {e}");
                        }
                        run_session_hook(&hook, "bye", &who);
                    });

                    let mut sessions = sessions.borrow_mut();
                    sessions.retain(|s| !s.handle.is_finished());
                    sessions.push(Session { who, perms, handle });
                }
                Accepted::Refused { who, message } => match message {
                    Some(msg) => println!("eg serve: refused {who}: {msg}"),
                    None => println!("eg serve: refused {who}"),
                },
            }
        }
    })
}

/// One live peer session: the capability minted at accept, held so the serving
/// side can tear it down. Aborting `handle` drops the session's link guard, which
/// closes the connection; the capability and the connection end together.
struct Session {
    who: EndpointId,

    perms: Perms,

    handle: tokio::task::JoinHandle<()>,
}

/// The live sessions, shared between the accept loop (which registers) and the
/// repl (which lists and revokes). Finished entries are pruned on access.
type Sessions = Rc<RefCell<Vec<Session>>>;

/// What the serve repl accepts on each stdin line: the repl-only session
/// commands plus every [`SharedCmd`](crate::SharedCmd), parsed by the same clap
/// derive as the shell, so names, args, and help text exist once. `multicall`
/// makes the first word the command (no leading binary name on the line).
#[derive(clap::Parser)]
#[command(
    multicall = true,
    disable_help_flag = true,
    about = "Control this running host: one command per line."
)]
enum ReplCmd {
    /// List live sessions and their perms.
    Sessions,
    /// Close every live session for a peer.
    ///
    /// Only tears down what is running; whether the peer gets back in is the
    /// gate's call on its next connection, so a lasting revocation also edits
    /// the trust file or the policy behind the authz command.
    Revoke { id: EndpointId },
    #[command(flatten)]
    Shared(crate::SharedCmd),
}

/// The serve repl: a line protocol on stdin, for a human at the terminal and for
/// a supervising parent process alike. `help` lists the commands.
///
/// EOF on stdin ends the repl and serving continues, so `eg serve < /dev/null`
/// still works headless.
fn spawn_repl(sessions: Sessions, presence: Presence, dir: PathBuf, hook: Rc<Option<Vec<String>>>) {
    tokio::task::spawn_local(async move {
        use tokio::io::AsyncBufReadExt;
        let mut lines = tokio::io::BufReader::new(tokio::io::stdin()).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            if line.trim().is_empty() {
                continue;
            }
            match <ReplCmd as clap::Parser>::try_parse_from(line.split_whitespace()) {
                Ok(cmd) => run_repl_cmd(cmd, &sessions, &presence, &dir, &hook),
                Err(e) => print!("{e}"), // clap's own usage/help text
            }
        }
    });
}

fn run_repl_cmd(
    cmd: ReplCmd,
    sessions: &Sessions,
    presence: &Presence,
    dir: &Path,
    hook: &Rc<Option<Vec<String>>>,
) {
    match cmd {
        ReplCmd::Sessions => {
            let mut sessions = sessions.borrow_mut();
            sessions.retain(|s| !s.handle.is_finished());
            println!("eg serve: {} session(s)", sessions.len());
            for s in sessions.iter() {
                println!(
                    "  {} {}",
                    label(&presence_identity(presence, &s.who), &s.who),
                    trust::flags(s.perms)
                );
            }
        }
        ReplCmd::Revoke { id } => {
            let mut sessions = sessions.borrow_mut();
            let mut dropped = 0;
            sessions.retain(|s| {
                if s.who == id && !s.handle.is_finished() {
                    s.handle.abort();
                    // an aborted session task never reaches its own bye
                    run_session_hook(hook, "bye", &s.who);
                    dropped += 1;
                }
                s.who != id
            });
            println!("eg serve: revoked {id} ({dropped} session(s) closed)");
        }
        ReplCmd::Shared(cmd) => {
            if let Err(e) = cmd.run(dir) {
                println!("eg serve: {e}");
            }
        }
    }
}

/// The connection gate `serve` hands to [`Node::accept`]: either the trust file,
/// reloaded per connection so a hand edit takes effect without a restart, or the
/// authz command named in `.eg/config.toml`.
///
/// An enum rather than a boxed trait object because [`Authorizer`] has an async
/// method and is not dyn-safe.
enum Gate {
    Trust(Rc<CapWorkspace>),
    Command(CommandAuthorizer<EndpointId>),
}

impl Authorizer for Gate {
    type Id = EndpointId;

    async fn authorize(&self, who: EndpointId) -> Decision {
        match self {
            Gate::Trust(ws) => match Trust::load(&**ws) {
                Ok(trust) => match trust.perms(who) {
                    Some(perms) => Decision::Grant(perms),
                    // No grant for this peer. Tell it how to get one.
                    None => Decision::Deny {
                        message: Some("ask the owner to run `eg trust <your-id>`".into()),
                    },
                },
                Err(e) => {
                    eprintln!("eg serve: cannot read .eg/trust, refusing: {e}");
                    Decision::Deny { message: None }
                }
            },
            Gate::Command(authz) => authz.authorize(who).await,
        }
    }
}

/// Clone `dir` from `peer` if empty, then hold a live session: local edits and
/// the peer's stream both ways until the link drops.
pub fn join(dir: &Path, peer: &str) -> std::io::Result<()> {
    let base = dir.canonicalize()?;
    let ws = Rc::new(open_repo(dir)?);
    block_on(async move {
        let id = EndpointId::from_str(peer).map_err(to_io)?;
        let doc: Shared = Rc::new(open_or_clone(&*ws)?);
        let node = Node::bind_with_secret(repo_secret(&*ws)?)
            .await
            .map_err(to_io)?;
        let me = local_identity(&Config::load(&*ws)?, &base);
        let changed = spawn_watch_and_export(ws.clone(), base, doc.clone())?;

        let presence = Presence::new(PRESENCE_TIMEOUT.as_millis() as i64);
        let (relay, _) = broadcast::channel(64);
        let _presence_sub = wire_presence(&presence, &relay, node.id().to_string(), &me);

        println!("eg join: syncing live with {id} (ctrl-c to stop)");

        let (send, recv, _guard) = node.dial(id).await.map_err(to_io)?;
        let link = PresenceLink {
            presence,
            relay,
            owner: None,
        };
        if let Err(e) = drive_live(doc, send, recv, Perms::all(), changed, Some(link)).await {
            return Err(report_sync_error("eg join", &id, e));
        }

        println!("eg join: link closed");
        Ok(())
    })
}

/// Pull from the peer named by `peer` (an endpoint id), merging into `dir`. One
/// shot: clone or catch up, write the files, exit.
pub fn pull(dir: &Path, peer: &str, timeout_secs: u64) -> std::io::Result<()> {
    let ws = open_repo(dir)?;
    block_on(async move {
        let id = EndpointId::from_str(peer).map_err(to_io)?;
        let doc = open_or_clone(&ws)?;
        let node = Node::bind_with_secret(repo_secret(&ws)?)
            .await
            .map_err(to_io)?;

        println!("eg pull: dialing {id} (up to {timeout_secs}s)...");
        if let Err(e) = node
            .sync_with(id, &doc, Duration::from_secs(timeout_secs))
            .await
        {
            return Err(report_sync_error("eg pull", &id, e));
        }

        let files = store(&doc, &ws)?.len();
        println!(
            "eg pull: synced with {id}, wrote {files} file(s) to {}",
            dir.display()
        );
        Ok(())
    })
}

/// Turn a failed sync into an io error. On a refusal, print the peer's message
/// under a `remote:` prefix; other errors pass straight through.
fn report_sync_error(prog: &str, peer: &EndpointId, e: okayeg_net::Error) -> io::Error {
    if let okayeg_net::Error::Refused { message } = &e {
        println!("{prog}: rejected by {peer}");
        if let Some(msg) = message {
            // The peer chose this text, so strip control bytes before printing
            // it: an escape sequence here would be a terminal injection.
            for line in sanitize(msg).lines() {
                println!("remote: {line}");
            }
        }
        // Empty message: we already printed the refusal above, so `run` should
        // stay quiet and just deliver the exit code.
        return io::Error::new(io::ErrorKind::PermissionDenied, "");
    }
    to_io(e)
}

/// Drop control characters from peer text, keeping newlines so a multi-line
/// message still lays out.
fn sanitize(s: &str) -> String {
    s.chars()
        .filter(|&c| c == '\n' || !c.is_control())
        .collect()
}

/// Wire the FS watcher and disk exporter onto the shared doc, returning the
/// repo-wide change nudge. The watcher merges local edits in (firing the nudge
/// when they move the doc); the exporter writes the doc back out on every nudge,
/// whatever its source.
///
/// The two tasks share the per-file merge bases: the exporter advances a file's
/// base whenever it writes it to disk, and the ingest side diffs an edited file
/// against its base so peer ops that landed since the file last matched disk
/// survive the merge.
fn spawn_watch_and_export(
    ws: Rc<CapWorkspace>,
    base: PathBuf,
    doc: Shared,
) -> io::Result<broadcast::Sender<()>> {
    let (changed, _) = broadcast::channel::<()>(64);
    let bases = Rc::new(RefCell::new(watch::seed_bases(&*ws, &doc)));

    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let mut debouncer = new_debouncer(
        Duration::from_millis(150),
        None,
        move |res: DebounceEventResult| {
            let _ = tx.send(res);
        },
    )
    .map_err(to_io)?;
    debouncer
        .watch(&base, RecursiveMode::Recursive)
        .map_err(to_io)?;

    // ingest: merge each changed path in, nudge only when the doc actually moves.
    let in_ws = ws.clone();
    let in_doc = doc.clone();
    let in_changed = changed.clone();
    let in_bases = bases.clone();
    tokio::task::spawn_local(async move {
        // hold the watcher alive for as long as we drain it
        let _debouncer = debouncer;

        while let Some(res) = rx.recv().await {
            let events = match res {
                Ok(events) => events,
                Err(errors) => {
                    for e in errors {
                        eprintln!("eg: watch error: {e}");
                    }
                    continue;
                }
            };

            let paths = changed_paths(&events, &base);
            if paths.is_empty() {
                continue;
            }

            match watch::apply_batch(&*in_ws, &in_doc, &paths, &mut in_bases.borrow_mut()) {
                Ok(true) => {
                    let _ = in_changed.send(());
                }
                Ok(false) => {}
                Err(e) => eprintln!("eg: {e}"),
            }
        }
    });

    // egress: on any nudge (local or peer), write the doc back to disk. Every
    // written file now matches the doc, so its merge base moves to the current
    // frontier.
    let out_ws = ws;
    let out_doc = doc;
    let mut nudged = changed.subscribe();
    tokio::task::spawn_local(async move {
        loop {
            match nudged.recv().await {
                Err(broadcast::error::RecvError::Closed) => break,
                _ => match store(&out_doc, &*out_ws) {
                    Ok(written) => {
                        let now = out_doc.frontiers();
                        let mut bases = bases.borrow_mut();
                        for path in written {
                            bases.insert(path, now.clone());
                        }
                    }
                    Err(e) => eprintln!("eg: export: {e}"),
                },
            }
        }
    });

    Ok(changed)
}

/// Collect the unique repo-relative paths touched by a batch, skipping `.eg/`.
fn changed_paths(events: &[notify_debouncer_full::DebouncedEvent], base: &Path) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    for event in events {
        for path in &event.paths {
            let Ok(rel) = path.strip_prefix(base) else {
                continue;
            };
            if rel.as_os_str().is_empty() || rel.starts_with(EG_DIR) {
                continue;
            }
            let rel = rel.to_path_buf();
            if !paths.contains(&rel) {
                paths.push(rel);
            }
        }
    }
    paths
}

/// Print this repo's endpoint id, the address a peer dials and trusts.
///
/// Generates the key on first use, so `eg id` also initializes `.eg/`.
pub fn id(dir: &Path) -> io::Result<()> {
    let ws = open_repo(dir)?;
    println!("{}", okayeg_net::id_from_secret(repo_secret(&ws)?));
    Ok(())
}

/// Print a read-only summary of this repo: its id, doc contents, and trust set.
///
/// While it does not seed anything, it generates the key on first use like [`id`], so a fresh
/// directory still gets its `.eg/`.
pub fn status(dir: &Path) -> io::Result<()> {
    let ws = open_repo(dir)?;
    // already absolute and canonical: with_repo ran `dir` through abs().
    println!("eg status: {}", dir.display());
    println!("  id:    {}", okayeg_net::id_from_secret(repo_secret(&ws)?));

    match read_doc(&ws)? {
        Some(doc) => {
            let (files, dirs) = count_tree(&doc);
            println!("  doc:   {files} file(s), {dirs} dir(s)");
        }
        None => println!("  doc:   not yet seeded (run eg serve or eg pull)"),
    }

    let grants = Trust::load(&ws)?.grants();
    if grants.is_empty() {
        println!("  trust: no peers (grant access with eg trust <id>)");
    } else {
        println!("  trust: {} peer(s)", grants.len());
        for g in grants {
            let flags = trust::flags(g.perms);
            let flags = if flags.is_empty() { "none" } else { &flags };
            let note = if g.revoked { " (revoked)" } else { "" };
            println!("    {}  {flags}{note}", g.id);
        }
    }
    Ok(())
}

/// Count the files and directories in a doc's tree, skipping the `.eg/` root,
/// so the reported count matches what export actually materializes. The walk
/// already leaves out anything without a valid path, like export does.
fn count_tree(doc: &Doc) -> (usize, usize) {
    let (mut files, mut dirs) = (0, 0);
    let eg_prefix = format!("{EG_DIR}/");

    for (path, entry) in doc.fs().walk() {
        if path == EG_DIR || path.starts_with(&eg_prefix) {
            continue;
        }
        match entry.kind {
            Some(NodeKind::Dir) => dirs += 1,
            Some(NodeKind::File) => files += 1,
            _ => {}
        }
    }
    (files, dirs)
}

/// Grant `peer` access to this repo, writing it into `.eg/trust`.
///
/// `flags` is any of `pull` / `push`; empty means grant both. Re-running with
/// `revoked`... is not how revocation works here, that is hand-edited or a later
/// command; this only adds grants.
pub fn trust(dir: &Path, peer: &str, perms: Perms) -> io::Result<()> {
    let ws = open_repo(dir)?;
    let id = EndpointId::from_str(peer).map_err(to_io)?;
    trust::add(&ws, id, perms)?;
    println!("eg trust: {id} may {}", trust::flags(perms));
    Ok(())
}

/// Open `dir` as a confined workspace, creating it (and `.eg/`) if needed.
pub(crate) fn open_repo(dir: &Path) -> io::Result<CapWorkspace> {
    std::fs::create_dir_all(dir)?;
    let ws = CapWorkspace::open(dir)?;
    ws.create_dir(Path::new(EG_DIR))?;
    Ok(ws)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn count_tree_skips_unsafe_named_nodes_like_export() {
        let doc = Doc::new();
        let tree = doc.files();
        // A safe dir with a safe child, plus unsafe siblings that export skips.
        let dir = tree.create_dir(None, "src");
        tree.create_file(Some(dir), "main.rs");
        tree.create_file(None, "ok.txt");
        tree.create_file(None, "../pwned");
        tree.create_dir(None, "..");
        doc.commit();

        // 2 files (ok.txt, src/main.rs), 1 dir (src); the unsafe nodes are gone.
        assert_eq!(count_tree(&doc), (2, 1));
    }
}
