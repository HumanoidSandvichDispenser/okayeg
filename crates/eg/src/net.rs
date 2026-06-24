//! The networked sync commands: serve a doc over iroh and pull from a peer.
//!
//! Both load a directory into a doc, run one okayeg sync over iroh, and write
//! the merged result back out. `serve` keeps listening and rewrites the
//! directory after each peer; `pull` dials a peer once and exits.

use std::io;
use std::path::Path;
use std::str::FromStr;

use okayeg::Doc;
use okayeg_net::{Accepted, EndpointId, Node, Perms};

use crate::trust::{self, Trust};
use crate::workspace::{CapWorkspace, Workspace};
use crate::{export_tree, import_tree, to_io};

use crate::EG_DIR;

/// This node's secret key, the raw form of its identity. Owner-only.
const KEY_PATH: &str = ".eg/key";

/// Run an async command on a small current-thread runtime.
fn block_on<F>(fut: F) -> std::io::Result<()>
where
    F: std::future::Future<Output = std::io::Result<()>>,
{
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?
        .block_on(fut)
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

/// Load the directory behind `ws` into a fresh doc.
fn load(ws: &dyn Workspace) -> io::Result<Doc> {
    let doc = Doc::new();
    import_tree(ws, &doc)?;
    Ok(doc)
}

/// Write `doc`'s tree back out through `ws`. Returns the file count.
fn store(doc: &Doc, ws: &dyn Workspace) -> io::Result<usize> {
    export_tree(doc, ws)
}

/// Serve `dir` over iroh, syncing each peer that connects.
///
/// Binds an endpoint, prints the id a peer dials, then loops forever: accept a
/// peer, sync, and rewrite the directory with the merged result.
pub fn serve(dir: &Path) -> std::io::Result<()> {
    let ws = open_repo(dir)?;
    block_on(async move {
        let doc = load(&ws)?;
        let node = Node::bind_with_secret(repo_secret(&ws)?).await.map_err(to_io)?;
        // Wait until we are online so discovery can route a dial to us.
        let _ = node.addr().await;
        println!("eg serve: listening as {}", node.id());
        println!("  trust a peer: eg trust <dir> <their-id> [pull] [push]");
        loop {
            // The gate reads trust the moment a peer connects, not before we
            // block waiting for one, so grants and revocations take effect live.
            let gate = |who| match Trust::load(&ws) {
                Ok(trust) => trust.perms(who),
                Err(e) => {
                    eprintln!("eg serve: cannot read .eg/trust, refusing: {e}");
                    None
                }
            };
            match node.accept_one(&doc, gate).await.map_err(to_io)? {
                Accepted::Synced(who, perms) => {
                    let files = store(&doc, &ws)?;
                    println!(
                        "eg serve: synced {who} ({}), wrote {files} file(s)",
                        trust::flags(perms)
                    );
                }
                Accepted::Refused(who) => {
                    println!("eg serve: refused {who} (not trusted)");
                }
            }
        }
    })
}

/// Pull from the peer named by `peer` (an endpoint id), merging into `dir`.
pub fn pull(dir: &Path, peer: &str) -> std::io::Result<()> {
    let ws = open_repo(dir)?;
    block_on(async move {
        let id = EndpointId::from_str(peer).map_err(to_io)?;
        let doc = load(&ws)?;
        let node = Node::bind_with_secret(repo_secret(&ws)?).await.map_err(to_io)?;
        node.sync_with(id, &doc).await.map_err(to_io)?;
        let files = store(&doc, &ws)?;
        println!(
            "eg pull: synced with {id}, wrote {files} file(s) to {}",
            dir.display()
        );
        Ok(())
    })
}

/// Print this repo's endpoint id, the address a peer dials and trusts.
///
/// Generates the key on first use, so `eg id` also initializes `.eg/`.
pub fn id(dir: &Path) -> io::Result<()> {
    let ws = open_repo(dir)?;
    println!("{}", okayeg_net::id_from_secret(repo_secret(&ws)?));
    Ok(())
}

/// Grant `peer` access to this repo, writing it into `.eg/trust`.
///
/// `flags` is any of `pull` / `push`; empty means grant both. Re-running with
/// `revoked`... is not how revocation works here, that is hand-edited or a later
/// command; this only adds grants.
pub fn trust(dir: &Path, peer: &str, flags: &[String]) -> io::Result<()> {
    let ws = open_repo(dir)?;
    let id = EndpointId::from_str(peer).map_err(to_io)?;
    let perms = perms_from_flags(flags)?;
    trust::add(&ws, id, perms)?;
    println!("eg trust: {id} may {}", trust::flags(perms));
    Ok(())
}

/// Parse `pull` / `push` flags; no flags means full access.
fn perms_from_flags(flags: &[String]) -> io::Result<Perms> {
    if flags.is_empty() {
        return Ok(Perms::all());
    }
    let mut perms = Perms {
        pull: false,
        push: false,
    };
    for f in flags {
        match f.as_str() {
            "pull" => perms.pull = true,
            "push" => perms.push = true,
            other => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("unknown perm {other:?}; use pull and/or push"),
                ));
            }
        }
    }
    Ok(perms)
}

/// Open `dir` as a confined workspace, creating it (and `.eg/`) if needed.
fn open_repo(dir: &Path) -> io::Result<CapWorkspace> {
    std::fs::create_dir_all(dir)?;
    let ws = CapWorkspace::open(dir)?;
    ws.create_dir(Path::new(EG_DIR))?;
    Ok(ws)
}
