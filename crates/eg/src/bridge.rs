//! Bridge a directory and an okayeg doc.
//!
//! This is where a real folder of text files becomes a doc tree and back again.
//! All filesystem access goes through [`Workspace`], which confines it to the
//! workspace root, so a peer-pushed tree cannot reach outside it.

use std::fs;
use std::path::Path;

use okayeg::{Doc, NodeKind, TreeID};

use crate::ignore::Ignorer;
use crate::workspace::{CapWorkspace, Entry, Workspace};
use crate::{EG_DIR, to_io};

/// Walk the directory at `dir` into a doc and write the snapshot to `out`.
pub fn snapshot(dir: &Path, out: &Path) -> std::io::Result<()> {
    let ws = CapWorkspace::open(dir)?;
    let doc = Doc::new();
    let files = import_tree(&ws, &doc)?;
    fs::write(out, doc.snapshot().map_err(to_io)?)?;
    println!(
        "snapshot: {files} file(s) from {} -> {}",
        dir.display(),
        out.display()
    );
    Ok(())
}

/// Load the snapshot at `file` and write its tree into the directory `dir`.
pub fn restore(file: &Path, dir: &Path) -> std::io::Result<()> {
    fs::create_dir_all(dir)?;
    let ws = CapWorkspace::open(dir)?;
    let doc = Doc::from_snapshot(&fs::read(file)?).map_err(to_io)?;
    let files = export_tree(&doc, &ws)?.files.len();
    println!(
        "restore: {files} file(s) from {} -> {}",
        file.display(),
        dir.display()
    );
    Ok(())
}

/// Read every file under the workspace into the doc's tree. Returns the file count.
pub fn import_tree(ws: &dyn Workspace, doc: &Doc) -> std::io::Result<usize> {
    let ignorer = Ignorer::load(ws)?;
    let mut files = 0;
    import_dir(ws, doc, &ignorer, Path::new(""), None, &mut files)?;
    doc.commit();
    Ok(files)
}

fn import_dir(
    ws: &dyn Workspace,
    doc: &Doc,
    ignorer: &Ignorer,
    rel: &Path,
    parent: Option<TreeID>,
    files: &mut usize,
) -> std::io::Result<()> {
    let tree = doc.files();
    let mut entries = ws.read_dir(rel)?;
    entries.sort_by(|a, b| name_of(a).cmp(name_of(b)));

    for entry in entries {
        // `.eg/` holds the repo's own state (key, and later the snapshot); it is
        // metadata about the doc, not content of it, so never walk into it.
        if rel.as_os_str().is_empty() && name_of(&entry) == EG_DIR {
            continue;
        }
        let path = rel.join(name_of(&entry));
        // Checked after `.eg/`, so `.eg/ignore` adds to that skip, never undoes it.
        if ignorer.should_ignore(&path, matches!(entry, Entry::Dir(_))) {
            continue;
        }
        match entry {
            Entry::Dir(name) => {
                let node = tree.create_dir(parent, &name);
                import_dir(ws, doc, ignorer, &rel.join(&name), Some(node), files)?;
            }
            Entry::File(name) => {
                let bytes = ws.read_file(&rel.join(&name))?;
                // Only text files for now; skip what isn't valid UTF-8.
                match String::from_utf8(bytes) {
                    Ok(text) => {
                        let node = tree.create_file(parent, &name);
                        if let Some(content) = tree.content(node) {
                            content.insert(0, &text).map_err(to_io)?;
                        }
                        *files += 1;
                    }
                    Err(_) => eprintln!(
                        "{}: skipping non-text file {}",
                        std::env::current_exe()
                            .map(|p| p.display().to_string())
                            .unwrap_or_else(|_| "eg".to_string()),
                        rel.join(&name).display()
                    ),
                }
            }
        }
    }
    Ok(())
}

/// The files and directories that would be written, each paired with the doc node it came from.
pub struct ExportPlan {
    /// `(rel_path, node)` for every file written.
    pub files: Vec<(std::path::PathBuf, TreeID)>,
    /// `(rel_path, node)` for every directory ensured.
    pub dirs: Vec<(std::path::PathBuf, TreeID)>,
}

/// Write the doc's tree out into the workspace. Returns every file written and
/// directory ensured, each paired with its doc node.
pub fn export_tree(doc: &Doc, ws: &dyn Workspace) -> std::io::Result<ExportPlan> {
    let ignorer = Ignorer::load(ws)?;
    let mut plan = ExportPlan {
        files: Vec::new(),
        dirs: Vec::new(),
    };
    for node in doc.files().roots() {
        // do not materialize any files in .eg
        if doc.files().name(node).as_deref() == Some(EG_DIR) {
            continue;
        }
        export_node(doc, ws, &ignorer, node, Path::new(""), &mut plan)?;
    }
    Ok(plan)
}

/// Whether export would write a node named `name` at `rel`.
pub(crate) fn materializable(name: &str, rel: &Path, is_dir: bool, ignorer: &Ignorer) -> bool {
    okayeg::valid_name(name) && !ignorer.should_ignore(rel, is_dir)
}

fn export_node(
    doc: &Doc,
    ws: &dyn Workspace,
    ignorer: &Ignorer,
    node: TreeID,
    parent: &Path,
    plan: &mut ExportPlan,
) -> std::io::Result<()> {
    let tree = doc.files();
    let Some(name) = tree.name(node) else {
        return Ok(());
    };
    let rel = parent.join(&name);
    let kind = tree.kind(node);
    if !materializable(&name, &rel, matches!(kind, Some(NodeKind::Dir)), ignorer) {
        if !okayeg::valid_name(&name) {
            eprintln!("eg: skipping tree node with unsafe name {name:?}");
        }
        return Ok(());
    }
    match kind {
        Some(NodeKind::Dir) => {
            ws.create_dir(&rel)?;
            plan.dirs.push((rel.clone(), node));
            for child in tree.children(node) {
                export_node(doc, ws, ignorer, child, &rel, plan)?;
            }
        }
        Some(NodeKind::File) => {
            let text = tree
                .content(node)
                .map(|c| c.to_string())
                .unwrap_or_default();
            ws.write_file(&rel, text.as_bytes())?;
            plan.files.push((rel, node));
        }
        // Boundaries point at another doc; nothing to write inline yet.
        Some(NodeKind::Boundary) | None => {}
    }
    Ok(())
}

fn name_of(entry: &Entry) -> &str {
    match entry {
        Entry::Dir(name) | Entry::File(name) => name,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workspace::MemWorkspace;

    #[test]
    fn round_trips_through_an_in_memory_workspace() {
        // Build a tree in memory: README.md, src/main.rs, src/sub/deep.txt.
        let src = MemWorkspace::new();
        src.write_file(Path::new("README.md"), b"gib eg\n").unwrap();
        src.write_file(Path::new("src/main.rs"), b"fn main() {}\n")
            .unwrap();
        src.write_file(Path::new("src/nested/deep.txt"), b"dark fantasies\n")
            .unwrap();

        // Snapshot it into a doc, then restore into a fresh workspace.
        let doc = Doc::new();
        let imported = import_tree(&src, &doc).unwrap();
        assert_eq!(imported, 3);

        let bytes = doc.snapshot().unwrap();
        let restored_doc = Doc::from_snapshot(&bytes).unwrap();
        let dst = MemWorkspace::new();
        let exported = export_tree(&restored_doc, &dst).unwrap().files.len();
        assert_eq!(exported, 3);

        // The restored files should match the originals.
        for path in ["README.md", "src/main.rs", "src/nested/deep.txt"] {
            assert_eq!(
                dst.read_file(Path::new(path)).unwrap(),
                src.read_file(Path::new(path)).unwrap(),
                "{path}"
            );
        }
    }

    #[test]
    fn valid_name_rejects_traversal_and_separators() {
        assert!(okayeg::valid_name("ok.txt"));
        assert!(okayeg::valid_name(".hidden"));
        assert!(okayeg::valid_name("a file with spaces"));
        for bad in ["", ".", "..", "../pwned", "a/b", "/abs", "nested/"] {
            assert!(!okayeg::valid_name(bad), "{bad:?} should be rejected");
        }
    }

    #[test]
    fn export_skips_unsafe_node_names_on_real_disk() {
        let root = tempfile::tempdir().unwrap();
        let outside = root.path().parent().unwrap().join("pwned");
        let _ = std::fs::remove_file(&outside);
        let ws = CapWorkspace::open(root.path()).unwrap();

        let doc = Doc::new();
        let tree = doc.files();
        for bad in ["../pwned", "..", "nested/evil"] {
            let node = tree.create_file(None, bad);
            if let Some(content) = tree.content(node) {
                content.insert(0, "x").unwrap();
            }
        }
        let good = tree.create_file(None, "ok.txt");
        if let Some(content) = tree.content(good) {
            content.insert(0, "ok").unwrap();
        }
        doc.commit();

        // Export completes (no aborting error) and writes only the safe file.
        let files = export_tree(&doc, &ws).unwrap().files.len();
        assert_eq!(files, 1, "only ok.txt should materialize");
        assert_eq!(ws.read_file(Path::new("ok.txt")).unwrap(), b"ok");
        // Nothing escaped the root.
        assert!(
            !outside.exists(),
            "traversal escaped to {}",
            outside.display()
        );
    }

    #[test]
    fn export_never_materializes_an_eg_dir() {
        let doc = Doc::new();
        let tree = doc.files();
        let eg = tree.create_dir(None, ".eg");
        let key = tree.create_file(Some(eg), "key");
        if let Some(content) = tree.content(key) {
            content.insert(0, "attacker-key").unwrap();
        }

        // a normal file that shouldn't be ignored
        let readme = tree.create_file(None, "README.md");
        if let Some(content) = tree.content(readme) {
            content.insert(0, "ok").unwrap();
        }

        doc.commit();

        let ws = MemWorkspace::new();
        let files = export_tree(&doc, &ws).unwrap().files.len();

        assert_eq!(files, 1, "only README.md should be written, not .eg/key");
        assert_eq!(ws.read_file(Path::new("README.md")).unwrap(), b"ok");
        assert!(
            ws.read_file(Path::new(".eg/key")).is_err(),
            ".eg must never be materialized from doc content"
        );
    }

    #[test]
    fn ignore_skips_imports_and_prunes_dirs() {
        let src = MemWorkspace::new();
        src.write_file(Path::new(".eg/ignore"), b"secrets.env\ntarget/\n")
            .unwrap();
        src.write_file(Path::new("README.md"), b"ok\n").unwrap();
        src.write_file(Path::new("secrets.env"), b"hunter2\n")
            .unwrap();
        src.write_file(Path::new("target/build.o"), b"junk\n")
            .unwrap();

        let doc = Doc::new();
        let imported = import_tree(&src, &doc).unwrap();
        assert_eq!(imported, 1, "only README.md should be imported");

        // And the ignored paths must not have been materialized on export either.
        let dst = MemWorkspace::new();
        dst.write_file(Path::new(".eg/ignore"), b"secrets.env\ntarget/\n")
            .unwrap();
        let exported = export_tree(&doc, &dst).unwrap().files.len();
        assert_eq!(exported, 1);
        assert_eq!(dst.read_file(Path::new("README.md")).unwrap(), b"ok\n");
        assert!(dst.read_file(Path::new("secrets.env")).is_err());
    }
}
