//! Live syncing: watch a directory and merge its changes into a doc.
//!
//! On each debounced batch the watcher does a *scoped* reconcile: for each path
//! that changed, it asks the filesystem what is there now and makes the doc
//! match. It never trusts the event's kind (that part is a cross-platform
//! mess), only its path. Work is proportional to what changed, not to repo
//! size. Writing a file's current text back is a no-op when nothing changed,
//! which absorbs the watcher's own echo.
//!
//! A file on disk carries only final state, so turning an edit into doc ops is
//! a three-way merge: the diff base must be the content eg and disk last agreed
//! on, not the doc's live state, or a peer op that landed in between is diffed
//! away as a deletion. The agreement point is tracked per file as a doc
//! frontier in [`FileBases`], advanced on both crossings (disk->doc ingest here,
//! doc->disk export in the caller); the base content itself stays derivable
//! from history, so nothing is shadowed on disk.

use std::collections::HashMap;
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

use notify::RecursiveMode;
use notify_debouncer_full::{new_debouncer, DebounceEventResult};
use okayeg::{Doc, Frontiers, NodeKind, TreeID};

use crate::workspace::{CapWorkspace, Kind, Workspace};

/// Per-file merge base: for each materialized file, the doc frontier at the
/// moment that file last matched the disk (was exported to it or ingested from
/// it). `checkout` at the frontier reproduces the content both sides agreed on.
pub type FileBases = HashMap<PathBuf, Frontiers>;

/// Seed [`FileBases`] for every doc file whose disk content already matches the
/// doc, pinning them to the current frontier. Files that differ (or are missing
/// on disk) get no base; their first reconcile establishes one.
///
/// Run at startup, after any initial export: it turns "disk and doc agree right
/// now" into recorded agreement points, so a resumed session merges instead of
/// falling back to the live 2-way diff.
pub fn seed_bases(ws: &dyn Workspace, doc: &Doc) -> FileBases {
    let tree = doc.files();
    let now = doc.frontiers();
    let mut bases = FileBases::new();

    let mut stack: Vec<(TreeID, PathBuf)> = tree
        .roots()
        .into_iter()
        .filter_map(|n| Some((n, PathBuf::from(tree.name(n)?))))
        .filter(|(_, p)| p.as_os_str() != crate::EG_DIR)
        .collect();

    while let Some((node, path)) = stack.pop() {
        match tree.kind(node) {
            Some(NodeKind::Dir) => {
                for child in tree.children(node) {
                    if let Some(name) = tree.name(child) {
                        stack.push((child, path.join(name)));
                    }
                }
            }
            Some(NodeKind::File) => {
                let text = tree.content(node).map(|c| c.to_string()).unwrap_or_default();
                if ws.read_file(&path).is_ok_and(|bytes| bytes == text.as_bytes()) {
                    bases.insert(path, now.clone());
                }
            }
            _ => {}
        }
    }
    bases
}

/// Merge a batch of changed paths into the doc, commit, and advance each path's
/// merge base to the resulting frontier. Returns whether the doc moved.
pub fn apply_batch(
    ws: &dyn Workspace,
    doc: &Doc,
    paths: &[PathBuf],
    bases: &mut FileBases,
) -> std::io::Result<bool> {
    let before = doc.version();
    for rel in paths {
        apply_path(ws, doc, rel, bases)?;
    }
    doc.commit();

    let now = doc.frontiers();
    for rel in paths {
        // The base advances only where a file is actually on disk; a deleted
        // or non-file path has no agreement point to record.
        match ws.kind(rel)? {
            Some(Kind::File) => {
                bases.insert(rel.clone(), now.clone());
            }
            _ => {
                bases.remove(rel);
            }
        }
    }
    Ok(doc.version() != before)
}

/// Watch `dir`, keeping the snapshot at `out` in sync until interrupted.
pub fn watch(dir: &Path, out: &Path) -> std::io::Result<()> {
    let ws = CapWorkspace::open(dir)?;
    let doc = Doc::new();
    crate::bridge::import_tree(&ws, &doc)?;
    persist(&doc, out)?;

    // The doc was just seeded from disk, so every file gets a base here.
    let mut bases = seed_bases(&ws, &doc);

    let base = dir.canonicalize()?;
    let snapshot_path = out.canonicalize().ok();

    let (tx, rx) = std::sync::mpsc::channel();
    let mut debouncer = new_debouncer(
        Duration::from_millis(200),
        None,
        move |result: DebounceEventResult| {
            let _ = tx.send(result);
        },
    )
    .map_err(crate::to_io)?;
    debouncer
        .watch(dir, RecursiveMode::Recursive)
        .map_err(crate::to_io)?;

    println!("watching {} (ctrl-c to stop)", dir.display());

    for result in rx {
        let events = match result {
            Ok(events) => events,
            Err(errors) => {
                for e in errors {
                    eprintln!("eg: watch error: {e}");
                }
                continue;
            }
        };

        // Collect the unique relative paths that changed in this batch.
        let mut changed: Vec<PathBuf> = Vec::new();
        for event in &events {
            for path in &event.paths {
                if snapshot_path.as_deref() == Some(path.as_path()) {
                    continue; // our own snapshot write
                }
                if let Ok(rel) = path.strip_prefix(&base) {
                    if !rel.as_os_str().is_empty() && !changed.contains(&rel.to_path_buf()) {
                        changed.push(rel.to_path_buf());
                    }
                }
            }
        }

        if !changed.is_empty() {
            apply_batch(&ws, &doc, &changed, &mut bases)?;
            persist(&doc, out)?;
            println!("synced {} change(s)", changed.len());
        }
    }
    Ok(())
}

fn persist(doc: &Doc, out: &Path) -> std::io::Result<()> {
    std::fs::write(out, doc.snapshot().map_err(crate::to_io)?)
}

/// Make the doc match what is at `rel` on the filesystem right now.
///
/// A changed file diffs against its recorded merge base when one exists, so
/// concurrent peer ops survive; with no base (new file, or nothing recorded
/// yet) it falls back to the live 2-way diff, which is correct exactly when
/// nothing else has touched the text since disk last saw it.
///
/// Separated from the notify plumbing so it can be tested against an in-memory
/// workspace with no watcher and no disk.
pub fn apply_path(
    ws: &dyn Workspace,
    doc: &Doc,
    rel: &Path,
    bases: &FileBases,
) -> std::io::Result<()> {
    let tree = doc.files();
    match ws.kind(rel)? {
        Some(Kind::Dir) => {
            ensure_dir(doc, rel);
        }
        Some(Kind::File) => {
            let bytes = ws.read_file(rel)?;
            match String::from_utf8(bytes) {
                Ok(text) => {
                    let parent = match rel.parent() {
                        Some(p) if !p.as_os_str().is_empty() => ensure_dir(doc, p),
                        _ => None,
                    };
                    let name = file_name(rel);
                    let node = child_named(doc, parent, &name)
                        .unwrap_or_else(|| tree.create_file(parent, &name));

                    if tree.content(node).is_some_and(|c| c.to_string() == text) {
                        // Disk and doc already agree (usually our own export
                        // echoing back); nothing to merge.
                    } else if !bases
                        .get(rel)
                        .is_some_and(|base| tree.set_content_at(node, &text, base))
                    {
                        tree.set_content(node, &text);
                    }
                }
                Err(_) => eprintln!("eg: skipping non-text file {}", rel.display()),
            }
        }
        None => {
            if let Some(node) = node_at(doc, rel) {
                tree.delete(node);
            }
        }
    }
    Ok(())
}

/// The child of `parent` (or a root, if `None`) named `name`.
fn child_named(doc: &Doc, parent: Option<TreeID>, name: &str) -> Option<TreeID> {
    let tree = doc.files();
    let candidates = match parent {
        Some(p) => tree.children(p),
        None => tree.roots(),
    };
    candidates
        .into_iter()
        .find(|node| tree.name(*node).as_deref() == Some(name))
}

/// Resolve a path to its node, if every component exists.
fn node_at(doc: &Doc, rel: &Path) -> Option<TreeID> {
    let mut parent = None;
    for comp in components(rel) {
        let node = child_named(doc, parent, &comp)?;
        parent = Some(node);
    }
    parent
}

/// Ensure a directory node exists for every component of `rel`, returning the
/// deepest one. `None` for the root.
fn ensure_dir(doc: &Doc, rel: &Path) -> Option<TreeID> {
    let tree = doc.files();
    let mut parent = None;
    for comp in components(rel) {
        let node = child_named(doc, parent, &comp)
            .unwrap_or_else(|| tree.create_dir(parent, &comp));
        parent = Some(node);
    }
    parent
}

fn components(rel: &Path) -> impl Iterator<Item = String> + '_ {
    rel.components().filter_map(|c| match c {
        Component::Normal(s) => Some(s.to_string_lossy().into_owned()),
        _ => None,
    })
}

fn file_name(rel: &Path) -> String {
    rel.file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default()
}

// Keep NodeKind referenced for future per-kind handling of existing nodes.
#[allow(dead_code)]
fn _kind_anchor(_: NodeKind) {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workspace::MemWorkspace;

    #[test]
    fn scoped_apply_creates_modifies_and_removes() {
        let ws = MemWorkspace::new();
        let doc = Doc::new();
        let mut bases = FileBases::new();
        let rel = || vec![PathBuf::from("src/main.rs")];

        // Create: a nested file appears, and gains a merge base.
        ws.write_file(Path::new("src/main.rs"), b"fn main() {}").unwrap();
        apply_batch(&ws, &doc, &rel(), &mut bases).unwrap();

        let node = node_at(&doc, Path::new("src/main.rs")).unwrap();
        assert_eq!(doc.files().content(node).unwrap().to_string(), "fn main() {}");
        assert!(bases.contains_key(Path::new("src/main.rs")));

        // Modify: same path, new content, same node (identity preserved).
        ws.write_file(Path::new("src/main.rs"), b"fn main() { todo!() }").unwrap();
        apply_batch(&ws, &doc, &rel(), &mut bases).unwrap();

        assert_eq!(node_at(&doc, Path::new("src/main.rs")), Some(node));
        assert_eq!(
            doc.files().content(node).unwrap().to_string(),
            "fn main() { todo!() }"
        );

        // Remove: the file is gone from the workspace, so the node and its
        // base go too.
        ws.remove(Path::new("src/main.rs"));
        apply_batch(&ws, &doc, &rel(), &mut bases).unwrap();

        assert_eq!(node_at(&doc, Path::new("src/main.rs")), None);
        assert!(!bases.contains_key(Path::new("src/main.rs")));
    }

    #[test]
    fn concurrent_peer_edit_survives_a_local_file_edit() {
        // A peer op lands in the doc, then a local disk edit made before that
        // op was flushed to disk is ingested. Both edits must survive.
        let ws = MemWorkspace::new();
        let doc = Doc::new();
        let mut bases = FileBases::new();
        let paths = vec![PathBuf::from("notes.txt")];

        ws.write_file(Path::new("notes.txt"), b"hello").unwrap();
        apply_batch(&ws, &doc, &paths, &mut bases).unwrap();

        // A peer appends "!" concurrently; the doc imports it, disk does not
        // see it yet.
        let node = node_at(&doc, Path::new("notes.txt")).unwrap();
        let peer = Doc::from_snapshot(&doc.snapshot().unwrap()).unwrap();
        peer.files().content(node).unwrap().insert(5, "!").unwrap();
        peer.commit();
        doc.import(&peer.updates_since(&doc.version()).unwrap()).unwrap();

        // A local edit based on the pre-peer content lands on disk.
        ws.write_file(Path::new("notes.txt"), b"hello world").unwrap();
        apply_batch(&ws, &doc, &paths, &mut bases).unwrap();

        let merged = doc.files().content(node).unwrap().to_string();
        assert!(merged.contains("world"), "local edit lost: {merged:?}");
        assert!(merged.contains('!'), "peer edit lost: {merged:?}");
    }

    #[test]
    fn seed_bases_pins_only_files_that_match_disk() {
        let ws = MemWorkspace::new();
        let doc = Doc::new();
        let tree = doc.files();

        let same = tree.create_file(None, "same.txt");
        tree.content(same).unwrap().insert(0, "agreed").unwrap();
        let diff = tree.create_file(None, "diff.txt");
        tree.content(diff).unwrap().insert(0, "doc side").unwrap();
        doc.commit();

        ws.write_file(Path::new("same.txt"), b"agreed").unwrap();
        ws.write_file(Path::new("diff.txt"), b"disk side").unwrap();

        let bases = seed_bases(&ws, &doc);
        assert!(bases.contains_key(Path::new("same.txt")));
        assert!(!bases.contains_key(Path::new("diff.txt")));
    }
}
