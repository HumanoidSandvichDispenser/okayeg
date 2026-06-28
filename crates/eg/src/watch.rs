//! Live syncing: watch a directory and fold its changes into a doc.
//!
//! On each debounced batch the watcher does a *scoped* reconcile: for each path
//! that changed, it asks the filesystem what is there now and makes the doc
//! match. It never trusts the event's kind (that part is a cross-platform
//! mess), only its path. Work is proportional to what changed, not to repo
//! size. Writing a file's current text back through `set_content` is a no-op
//! when nothing changed, which absorbs the watcher's own echo.

use std::path::{Component, Path, PathBuf};
use std::time::Duration;

use notify::RecursiveMode;
use notify_debouncer_full::{new_debouncer, DebounceEventResult};
use okayeg::{Doc, NodeKind, TreeID};

use crate::workspace::{CapWorkspace, Kind, Workspace};

/// Watch `dir`, keeping the snapshot at `out` in sync until interrupted.
pub fn watch(dir: &Path, out: &Path) -> std::io::Result<()> {
    let ws = CapWorkspace::open(dir)?;
    let doc = Doc::new();
    crate::bridge::import_tree(&ws, &doc)?;
    persist(&doc, out)?;

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

        for rel in &changed {
            apply_path(&ws, &doc, rel)?;
        }
        if !changed.is_empty() {
            doc.commit();
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
/// Separated from the notify plumbing so it can be tested against an in-memory
/// workspace with no watcher and no disk.
pub fn apply_path(ws: &dyn Workspace, doc: &Doc, rel: &Path) -> std::io::Result<()> {
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
                    tree.set_content(node, &text);
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

        // Create: a nested file appears.
        ws.write_file(Path::new("src/main.rs"), b"fn main() {}").unwrap();
        apply_path(&ws, &doc, Path::new("src/main.rs")).unwrap();
        let node = node_at(&doc, Path::new("src/main.rs")).unwrap();
        assert_eq!(doc.files().content(node).unwrap().to_string(), "fn main() {}");

        // Modify: same path, new content, same node (identity preserved).
        ws.write_file(Path::new("src/main.rs"), b"fn main() { todo!() }").unwrap();
        apply_path(&ws, &doc, Path::new("src/main.rs")).unwrap();
        assert_eq!(node_at(&doc, Path::new("src/main.rs")), Some(node));
        assert_eq!(
            doc.files().content(node).unwrap().to_string(),
            "fn main() { todo!() }"
        );

        // Remove: the file is gone from the workspace, so the node goes too.
        ws.remove(Path::new("src/main.rs"));
        apply_path(&ws, &doc, Path::new("src/main.rs")).unwrap();
        assert_eq!(node_at(&doc, Path::new("src/main.rs")), None);
    }
}
