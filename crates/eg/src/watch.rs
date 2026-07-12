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

use std::collections::{HashMap, HashSet};
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

use notify::RecursiveMode;
use notify_debouncer_full::{DebounceEventResult, new_debouncer};
use okayeg::{Doc, Frontiers, NodeKind, TreeID};

use crate::bridge::{ExportPlan, materializable};
use crate::ignore::Ignorer;
use crate::workspace::{CapWorkspace, Kind, Workspace};

/// A file's merge base (a frontier and the node last written there).
#[derive(Clone)]
pub struct ExportedBase {
    /// The doc frontier where disk and doc last agreed on this path.
    pub frontier: Frontiers,
    /// The file node the exporter wrote to the path at that frontier.
    pub node: TreeID,
}

/// Per-path merge bases for the materialized files.
pub type FileBases = HashMap<PathBuf, ExportedBase>;

/// Materialized directory paths.
pub type ExportDirs = HashSet<PathBuf>;

/// Seed [`FileBases`] and [`ExportDirs`] from current disk/doc agreement.
pub fn seed(ws: &dyn Workspace, doc: &Doc) -> std::io::Result<(FileBases, ExportDirs)> {
    let ignorer = Ignorer::load(ws)?;
    let tree = doc.files();
    let now = doc.frontiers();
    let mut bases = FileBases::new();
    let mut dirs = ExportDirs::new();

    let mut stack: Vec<(TreeID, String, PathBuf)> = tree
        .roots()
        .into_iter()
        .filter_map(|n| Some((n, tree.name(n)?, PathBuf::new())))
        .filter(|(_, name, _)| name != crate::EG_DIR)
        .collect();

    while let Some((node, name, parent)) = stack.pop() {
        let path = parent.join(&name);
        let kind = tree.kind(node);
        if !materializable(&name, &path, matches!(kind, Some(NodeKind::Dir)), &ignorer) {
            continue;
        }
        match kind {
            Some(NodeKind::Dir) => {
                if ws.kind(&path).is_ok_and(|k| k == Some(Kind::Dir)) {
                    dirs.insert(path.clone());
                }
                for child in tree.children(node) {
                    if let Some(name) = tree.name(child) {
                        stack.push((child, name, path.clone()));
                    }
                }
            }
            Some(NodeKind::File) => {
                let text = tree
                    .content(node)
                    .map(|c| c.to_string())
                    .unwrap_or_default();
                if ws
                    .read_file(&path)
                    .is_ok_and(|bytes| bytes == text.as_bytes())
                {
                    bases.insert(
                        path,
                        ExportedBase {
                            frontier: now.clone(),
                            node,
                        },
                    );
                }
            }
            _ => {}
        }
    }
    Ok((bases, dirs))
}

/// Whether disk still equals `node`'s content at `frontier`.
fn is_echo_of(
    tree: &okayeg::FileTree<'_>,
    node: TreeID,
    frontier: &Frontiers,
    disk: &[u8],
) -> bool {
    tree.content_at(node, frontier)
        .is_some_and(|text| disk == text.as_bytes())
}

/// Advance the tracked maps with what an export just wrote, then prune what it
/// did not. Returns the number of files written.
pub fn advance_and_prune(
    doc: &Doc,
    ws: &dyn Workspace,
    plan: ExportPlan,
    bases: &mut FileBases,
    dirs: &mut ExportDirs,
) -> usize {
    let now = doc.frontiers();
    let mut written_files: HashSet<PathBuf> = HashSet::with_capacity(plan.files.len());
    for (path, node) in plan.files {
        written_files.insert(path.clone());
        bases.insert(
            path,
            ExportedBase {
                frontier: now.clone(),
                node,
            },
        );
    }
    let mut written_dirs: HashSet<PathBuf> = HashSet::with_capacity(plan.dirs.len());
    for (path, _) in plan.dirs {
        written_dirs.insert(path.clone());
        dirs.insert(path);
    }
    prune(doc, ws, &written_files, &written_dirs, bases, dirs);
    written_files.len()
}

/// Remove files and dirs a peer delete or rename left stale on disk.
fn prune(
    doc: &Doc,
    ws: &dyn Workspace,
    written_files: &HashSet<PathBuf>,
    written_dirs: &HashSet<PathBuf>,
    bases: &mut FileBases,
    dirs: &mut ExportDirs,
) {
    let tree = doc.files();

    // Files that were exported this pass are current; candidates are everything
    // else in the tracked bases. Ordering does not matter for files (they have
    // no children); collect once so bases can be mutated in the loop.
    let candidates: Vec<PathBuf> = bases
        .keys()
        .filter(|p| !written_files.contains(*p))
        .cloned()
        .collect();
    for rel in candidates {
        let Some(base) = bases.get(&rel) else {
            continue;
        };
        let Some(disk) = ws.read_file(&rel).ok() else {
            // Already gone from disk: drop the base, nothing to prune.
            bases.remove(&rel);
            continue;
        };
        // The echo check reads the base content through a fork at the recorded
        // frontier, so a deleted or moved node still resolves. Disk differing
        // from the base means a local edit raced the op: keep file and base so
        // a later ingest re-creates it deliberately (edit-wins-over-delete).
        if !is_echo_of(&tree, base.node, &base.frontier, &disk) {
            continue;
        }
        // Echo of a deleted/moved node. Re-read right before the unlink to
        // narrow a round-the-debouncer edit race, then remove. A change between
        // the two reads is treated as the local-edit case above; a removal
        // failure keeps the entry so a later nudge retries.
        if ws
            .read_file(&rel)
            .is_ok_and(|d| is_echo_of(&tree, base.node, &base.frontier, &d))
            && ws.remove_file(&rel).is_ok()
        {
            bases.remove(&rel);
        }
    }

    // Dirs not written this pass are candidates, regardless of liveness: the
    // exporter writes every dir that still maps to its path, so not-written
    // means deleted or renamed away, and emptiness decides remove vs keep.
    // Deepest-first so a subtree delete empties children before their parent.
    let mut dir_candidates: Vec<PathBuf> = dirs
        .iter()
        .filter(|p| !written_dirs.contains(*p))
        .cloned()
        .collect();
    dir_candidates.sort_by_key(|p| std::cmp::Reverse(p.components().count()));
    for rel in dir_candidates {
        match ws.kind(&rel) {
            Ok(None) => {
                // Path is gone from disk already: drop the tracked entry so the
                // set stays bounded.
                dirs.remove(&rel);
            }
            Ok(Some(Kind::Dir)) => {
                if ws
                    .read_dir(&rel)
                    .is_ok_and(|entries| entries.is_empty())
                    && ws.remove_dir(&rel).is_ok()
                {
                    dirs.remove(&rel);
                }
                // Non-empty, or removal failed: keep the entry, retry next nudge.
            }
            Ok(Some(Kind::File)) => {
                // The path exists but is now a file (local file raced the
                // rmdir): keep the tracked dir entry so ingest reconciles it.
            }
            Err(_) => {
                // Stat failed (permissions?): keep the entry, retry.
            }
        }
    }
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
        // The base advances only where a file is actually on disk and was
        // resolved to a doc node; a deleted or non-file path has no agreement
        // point to record. apply_path has just created or matched the node,
        // so node_at finds it here; a non-text file it skipped has no node and
        // gets no base.
        match ws.kind(rel)? {
            Some(Kind::File) => {
                if let Some(node) = node_at(doc, rel) {
                    bases.insert(
                        rel.clone(),
                        ExportedBase {
                            frontier: now.clone(),
                            node,
                        },
                    );
                }
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

    // The doc was just seeded from disk, so every matching file gets a base
    // here. This is ingress-only (disk -> doc -> snapshot), so the dir map
    // from `seed` is unused.
    let (mut bases, _dir_bases) = seed(&ws, &doc)?;

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
                    let existing = child_named(doc, parent, &name);
                    let node = match existing {
                        Some(node) => node,
                        None => {
                            // No doc node at this path. Creating one
                            // unconditionally resurrected a peer delete: the
                            // stale file the exporter left behind would land
                            // here and mint a fresh node. Same echo rule as
                            // prune: an unmodified echo of a deleted/moved
                            // node is skipped; a genuine new local file, or a
                            // local edit raced onto a deleted node, is a
                            // deliberate re-create (edit-wins-over-delete).
                            let echo = bases.get(rel).is_some_and(|base| {
                                is_echo_of(&tree, base.node, &base.frontier, text.as_bytes())
                            });
                            if echo {
                                return Ok(());
                            }
                            tree.create_file(parent, &name)
                        }
                    };

                    if tree.content(node).is_some_and(|c| c.to_string() == text) {
                        // Disk and doc already agree (usually our own export
                        // echoing back); nothing to merge.
                    } else if !bases
                        .get(rel)
                        .is_some_and(|base| tree.set_content_at(node, &text, &base.frontier))
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
        let node =
            child_named(doc, parent, &comp).unwrap_or_else(|| tree.create_dir(parent, &comp));
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
        ws.write_file(Path::new("src/main.rs"), b"fn main() {}")
            .unwrap();
        apply_batch(&ws, &doc, &rel(), &mut bases).unwrap();

        let node = node_at(&doc, Path::new("src/main.rs")).unwrap();
        assert_eq!(
            doc.files().content(node).unwrap().to_string(),
            "fn main() {}"
        );
        assert!(bases.contains_key(Path::new("src/main.rs")));

        // Modify: same path, new content, same node (identity preserved).
        ws.write_file(Path::new("src/main.rs"), b"fn main() { todo!() }")
            .unwrap();
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
        doc.import(&peer.updates_since(&doc.version()).unwrap())
            .unwrap();

        // A local edit based on the pre-peer content lands on disk.
        ws.write_file(Path::new("notes.txt"), b"hello world")
            .unwrap();
        apply_batch(&ws, &doc, &paths, &mut bases).unwrap();

        let merged = doc.files().content(node).unwrap().to_string();
        assert!(merged.contains("world"), "local edit lost: {merged:?}");
        assert!(merged.contains('!'), "peer edit lost: {merged:?}");
    }

    #[test]
    fn seed_pins_only_files_that_match_disk_and_skips_unsafe_names() {
        // Matching disk files get a base; differing ones do not. A node with an
        // unsafe name export would never write, so it must not seed a base and
        // become a prune candidate pointing outside the root.
        let ws = MemWorkspace::new();
        let doc = Doc::new();
        let tree = doc.files();

        let same = tree.create_file(None, "same.txt");
        tree.content(same).unwrap().insert(0, "agreed").unwrap();
        let diff = tree.create_file(None, "diff.txt");
        tree.content(diff).unwrap().insert(0, "doc side").unwrap();
        let bad = tree.create_file(None, "..");
        tree.content(bad).unwrap().insert(0, "nope").unwrap();
        doc.commit();

        ws.write_file(Path::new("same.txt"), b"agreed").unwrap();
        ws.write_file(Path::new("diff.txt"), b"disk side").unwrap();

        let (bases, dirs) = seed(&ws, &doc).unwrap();
        assert!(bases.contains_key(Path::new("same.txt")));
        assert!(!bases.contains_key(Path::new("diff.txt")));
        assert!(!bases.contains_key(Path::new("..")), "unsafe name seeded");
        assert!(dirs.is_empty());
    }

    /// Import a peer delete of `node` into `doc` via a snapshot fork.
    fn peer_deletes(doc: &Doc, node: TreeID) {
        let peer = Doc::from_snapshot(&doc.snapshot().unwrap()).unwrap();
        peer.files().delete(node);
        peer.commit();
        doc.import(&peer.updates_since(&doc.version()).unwrap())
            .unwrap();
        doc.commit();
    }

    /// Drive a peer rename of `node` (keeping it alive) into `doc`.
    fn peer_renames(doc: &Doc, node: TreeID, new_name: &str) {
        let peer = Doc::from_snapshot(&doc.snapshot().unwrap()).unwrap();
        peer.files().rename(node, new_name);
        peer.commit();
        doc.import(&peer.updates_since(&doc.version()).unwrap())
            .unwrap();
        doc.commit();
    }

    /// Run the live loop's export half: export the doc, then
    /// [`advance_and_prune`].
    fn export_and_prune(
        doc: &Doc,
        ws: &dyn Workspace,
        bases: &mut FileBases,
        dirs: &mut ExportDirs,
    ) {
        let plan = crate::bridge::export_tree(doc, ws).unwrap();
        advance_and_prune(doc, ws, plan, bases, dirs);
    }

    #[test]
    fn prune_removes_a_stale_file_after_a_peer_delete() {
        // The exporter wrote x.txt to disk and recorded a base. A peer deletes
        // the node; export no longer writes x.txt, so it is a prune candidate.
        // Its disk content still equals the base content, so prune removes it,
        // and the create-fallback (apply_path) would skip it as an echo anyway.
        let ws = MemWorkspace::new();
        let doc = Doc::new();
        let tree = doc.files();
        let node = tree.create_file(None, "x.txt");
        tree.content(node).unwrap().insert(0, "hello").unwrap();
        doc.commit();
        let base = doc.frontiers();

        ws.write_file(Path::new("x.txt"), b"hello").unwrap();
        let mut bases = FileBases::new();
        bases.insert(
            PathBuf::from("x.txt"),
            ExportedBase { frontier: base, node },
        );
        let mut dirs = ExportDirs::new();

        peer_deletes(&doc, node);
        assert!(!doc.files().alive(node), "node should be dead after delete");

        export_and_prune(&doc, &ws, &mut bases, &mut dirs);

        assert!(ws.read_file(Path::new("x.txt")).is_err(), "stale file kept");
        assert!(!bases.contains_key(Path::new("x.txt")), "base kept");
        // No node exists for the path, so a later ingest's create-fallback has
        // nothing to re-create from: the resurrection is dead.
        assert_eq!(node_at(&doc, Path::new("x.txt")), None);
    }

    #[test]
    fn prune_keeps_a_file_a_local_edit_raced_a_delete_into() {
        // The peer deletes the node, but a local edit changed the disk content
        // first. The file is no longer an unmodified materialization of the
        // deleted node, so prune keeps it (edit-wins-over-delete) and keeps
        // the base so a later ingest re-creates it deliberately.
        let ws = MemWorkspace::new();
        let doc = Doc::new();
        let tree = doc.files();
        let node = tree.create_file(None, "x.txt");
        tree.content(node).unwrap().insert(0, "hello").unwrap();
        doc.commit();
        let base = doc.frontiers();

        ws.write_file(Path::new("x.txt"), b"hello").unwrap();
        let mut bases = FileBases::new();
        bases.insert(
            PathBuf::from("x.txt"),
            ExportedBase { frontier: base, node },
        );

        peer_deletes(&doc, node);
        // local edit races the delete import
        ws.write_file(Path::new("x.txt"), b"hello edited").unwrap();

        export_and_prune(&doc, &ws, &mut bases, &mut ExportDirs::new());

        assert_eq!(
            ws.read_file(Path::new("x.txt")).unwrap(),
            b"hello edited",
            "edit-raced file should be kept"
        );
        assert!(bases.contains_key(Path::new("x.txt")), "base should be kept");
    }

    #[test]
    fn prune_keeps_a_live_files_base_untouched() {
        // A file still in the doc is written this pass, so it is not a prune
        // candidate: its base advances to the new frontier and survives.
        let ws = MemWorkspace::new();
        let doc = Doc::new();
        let tree = doc.files();
        let node = tree.create_file(None, "x.txt");
        tree.content(node).unwrap().insert(0, "hello").unwrap();
        doc.commit();

        ws.write_file(Path::new("x.txt"), b"hello").unwrap();
        let mut bases = FileBases::new();
        bases.insert(
            PathBuf::from("x.txt"),
            ExportedBase { frontier: doc.frontiers(), node },
        );

        export_and_prune(&doc, &ws, &mut bases, &mut ExportDirs::new());

        assert_eq!(ws.read_file(Path::new("x.txt")).unwrap(), b"hello");
        assert!(bases.contains_key(Path::new("x.txt")));
    }

    #[test]
    fn prune_removes_an_empty_dir_after_a_subtree_delete() {
        // A peer deletes a directory node; its child files are gone too. Prune
        // removes the stale child file, leaving the dir empty, then removes
        // the dir. Both tracked entries are dropped.
        let ws = MemWorkspace::new();
        let doc = Doc::new();
        let tree = doc.files();
        let dir = tree.create_dir(None, "src");
        let file = tree.create_file(Some(dir), "main.rs");
        tree.content(file).unwrap().insert(0, "fn main()").unwrap();
        doc.commit();
        let base = doc.frontiers();

        ws.create_dir(Path::new("src")).unwrap();
        ws.write_file(Path::new("src/main.rs"), b"fn main()").unwrap();
        let mut bases = FileBases::new();
        bases.insert(
            PathBuf::from("src/main.rs"),
            ExportedBase { frontier: base.clone(), node: file },
        );
        let mut dirs = ExportDirs::new();
        dirs.insert(PathBuf::from("src"));

        peer_deletes(&doc, dir);
        assert!(!doc.files().alive(dir));
        assert!(!doc.files().alive(file));

        export_and_prune(&doc, &ws, &mut bases, &mut dirs);

        assert!(ws.read_file(Path::new("src/main.rs")).is_err(), "stale child kept");
        assert!(ws.kind(Path::new("src")).unwrap() != Some(Kind::Dir), "empty dir kept");
        assert!(!bases.contains_key(Path::new("src/main.rs")));
        assert!(!dirs.contains(Path::new("src")));
    }

    #[test]
    fn prune_keeps_a_dead_dir_that_a_local_file_raced_into() {
        // The dir node is dead, but a local file sits in it that the doc never
        // knew about. Prune must not remove a non-empty dir.
        let ws = MemWorkspace::new();
        let doc = Doc::new();
        let tree = doc.files();
        let dir = tree.create_dir(None, "src");
        doc.commit();

        ws.create_dir(Path::new("src")).unwrap();
        // a local file the doc has no node for, racing the peer rmdir
        ws.write_file(Path::new("src/local.txt"), b"local").unwrap();
        let mut dirs = ExportDirs::new();
        dirs.insert(PathBuf::from("src"));

        peer_deletes(&doc, dir);

        export_and_prune(&doc, &ws, &mut FileBases::new(), &mut dirs);

        assert_eq!(
            ws.kind(Path::new("src")).unwrap(),
            Some(Kind::Dir),
            "non-empty dir should be kept"
        );
        assert_eq!(ws.read_file(Path::new("src/local.txt")).unwrap(), b"local");
        assert!(dirs.contains(Path::new("src")), "dir base should be kept");
    }

    #[test]
    fn prune_removes_the_old_path_after_a_peer_rename() {
        // A peer renames a.txt to b.txt: the node stays alive, export writes
        // b.txt, and the old path a.txt is not in the written set, so it is a
        // prune candidate; its content still equals the base content, so prune
        // removes a.txt. No duplicate node is minted.
        let ws = MemWorkspace::new();
        let doc = Doc::new();
        let tree = doc.files();
        let node = tree.create_file(None, "a.txt");
        tree.content(node).unwrap().insert(0, "hello").unwrap();
        doc.commit();
        let base = doc.frontiers();

        ws.write_file(Path::new("a.txt"), b"hello").unwrap();
        let mut bases = FileBases::new();
        bases.insert(
            PathBuf::from("a.txt"),
            ExportedBase { frontier: base, node },
        );

        peer_renames(&doc, node, "b.txt");
        assert!(doc.files().alive(node), "node stays alive across rename");

        export_and_prune(&doc, &ws, &mut bases, &mut ExportDirs::new());

        assert!(
            ws.read_file(Path::new("a.txt")).is_err(),
            "old path should be pruned"
        );
        assert_eq!(
            ws.read_file(Path::new("b.txt")).unwrap(),
            b"hello",
            "new path should be written"
        );
        assert!(!bases.contains_key(Path::new("a.txt")), "old base kept");
        assert!(bases.contains_key(Path::new("b.txt")), "new base missing");
        // Exactly one node for b.txt; no duplicate minted at a.txt.
        assert_eq!(node_at(&doc, Path::new("b.txt")), Some(node));
        assert_eq!(node_at(&doc, Path::new("a.txt")), None);
    }

    #[test]
    fn pull_ordering_prunes_a_peer_delete_and_rename() {
        // Mirrors `pull`'s sequencing: seed the tracked maps from the local
        // doc, then import the peer's changes, then export and prune. Seeding
        // after the import misses the delete and the rename's old path (they
        // are gone from the live tree walk), so the stale files survive every
        // pull; this pins the seed-before-sync order.
        let ws = MemWorkspace::new();
        let doc = Doc::new();
        let tree = doc.files();
        let gone = tree.create_file(None, "x.txt");
        tree.content(gone).unwrap().insert(0, "hello").unwrap();
        let moved = tree.create_file(None, "a.txt");
        tree.content(moved).unwrap().insert(0, "keep").unwrap();
        doc.commit();
        ws.write_file(Path::new("x.txt"), b"hello").unwrap();
        ws.write_file(Path::new("a.txt"), b"keep").unwrap();

        let (mut bases, mut dirs) = seed(&ws, &doc).unwrap();
        assert!(bases.contains_key(Path::new("x.txt")));
        assert!(bases.contains_key(Path::new("a.txt")));

        peer_deletes(&doc, gone);
        peer_renames(&doc, moved, "b.txt");

        export_and_prune(&doc, &ws, &mut bases, &mut dirs);

        assert!(
            ws.read_file(Path::new("x.txt")).is_err(),
            "deleted file survived the pull"
        );
        assert!(
            ws.read_file(Path::new("a.txt")).is_err(),
            "rename's old path survived the pull"
        );
        assert_eq!(ws.read_file(Path::new("b.txt")).unwrap(), b"keep");
    }

    #[test]
    fn apply_path_does_not_resurrect_an_echo_after_a_delete() {
        // The resurrection vector (finding 6): a stale file left on disk fires
        // a watch event after the delete imports. With an echoed base, the
        // create-fallback must skip it instead of minting a new node.
        let ws = MemWorkspace::new();
        let doc = Doc::new();
        let tree = doc.files();
        let node = tree.create_file(None, "x.txt");
        tree.content(node).unwrap().insert(0, "hello").unwrap();
        doc.commit();
        let base = doc.frontiers();

        ws.write_file(Path::new("x.txt"), b"hello").unwrap();
        let mut bases = FileBases::new();
        bases.insert(
            PathBuf::from("x.txt"),
            ExportedBase { frontier: base, node },
        );

        // Prune would remove the file, but exercise the fallback directly:
        // the delete lands, the file stays (simulate prune losing the race or
        // a remove failure), and a watch event then runs apply_path.
        peer_deletes(&doc, node);
        assert!(node_at(&doc, Path::new("x.txt")).is_none());

        // disk still holds the echoed base content
        apply_path(&ws, &doc, Path::new("x.txt"), &bases).unwrap();
        assert_eq!(
            node_at(&doc, Path::new("x.txt")),
            None,
            "echo must not mint a new node"
        );

        // A genuine local edit (disk differs from base) does mint a new node
        // (edit-wins-over-delete in the strong sense).
        ws.write_file(Path::new("x.txt"), b"hello edited").unwrap();
        apply_path(&ws, &doc, Path::new("x.txt"), &bases).unwrap();
        let revived = node_at(&doc, Path::new("x.txt")).expect("edited file mints a node");
        assert_ne!(revived, node, "edit-wins created the old dead node");
        assert_eq!(
            doc.files().content(revived).unwrap().to_string(),
            "hello edited"
        );
    }
}
