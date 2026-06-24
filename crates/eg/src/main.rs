//! `eg`, the okayeg command line.
//!
//! For now it bridges a directory and an okayeg doc, so you can take a real
//! folder of text files into a doc and write it back out. All filesystem
//! access goes through [`Workspace`], which confines it to the workspace root.

mod watch;
mod workspace;

use std::fs;
use std::path::Path;
use std::process::ExitCode;

use okayeg::{Doc, NodeKind, TreeID};

use workspace::{CapWorkspace, Entry, Workspace};

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("snapshot") => match (args.get(2), args.get(3)) {
            (Some(dir), Some(out)) => run(snapshot(dir.as_ref(), out.as_ref())),
            _ => usage(),
        },
        Some("restore") => match (args.get(2), args.get(3)) {
            (Some(file), Some(dir)) => run(restore(file.as_ref(), dir.as_ref())),
            _ => usage(),
        },
        Some("watch") => match (args.get(2), args.get(3)) {
            (Some(dir), Some(out)) => run(watch::watch(dir.as_ref(), out.as_ref())),
            _ => usage(),
        },
        _ => usage(),
    }
}

fn run(result: std::io::Result<()>) -> ExitCode {
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("eg: {err}");
            ExitCode::FAILURE
        }
    }
}

fn usage() -> ExitCode {
    eprintln!(
        "usage:\n  \
         eg snapshot <dir> <out.eg>   take a directory into a doc snapshot\n  \
         eg restore  <in.eg> <dir>    write a doc snapshot back to a directory\n  \
         eg watch    <dir> <out.eg>   keep a doc snapshot in sync with a directory"
    );
    ExitCode::FAILURE
}

/// Walk the directory at `dir` into a doc and write the snapshot to `out`.
fn snapshot(dir: &Path, out: &Path) -> std::io::Result<()> {
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
fn restore(file: &Path, dir: &Path) -> std::io::Result<()> {
    fs::create_dir_all(dir)?;
    let ws = CapWorkspace::open(dir)?;
    let doc = Doc::from_snapshot(&fs::read(file)?).map_err(to_io)?;
    let files = export_tree(&doc, &ws)?;
    println!(
        "restore: {files} file(s) from {} -> {}",
        file.display(),
        dir.display()
    );
    Ok(())
}

/// Read every file under the workspace into the doc's tree. Returns the file count.
fn import_tree(ws: &dyn Workspace, doc: &Doc) -> std::io::Result<usize> {
    let mut files = 0;
    import_dir(ws, doc, Path::new(""), None, &mut files)?;
    doc.commit();
    Ok(files)
}

fn import_dir(
    ws: &dyn Workspace,
    doc: &Doc,
    rel: &Path,
    parent: Option<TreeID>,
    files: &mut usize,
) -> std::io::Result<()> {
    let tree = doc.files();
    let mut entries = ws.read_dir(rel)?;
    entries.sort_by(|a, b| name_of(a).cmp(name_of(b)));

    for entry in entries {
        match entry {
            Entry::Dir(name) => {
                let node = tree.create_dir(parent, &name);
                import_dir(ws, doc, &rel.join(&name), Some(node), files)?;
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

/// Write the doc's tree out into the workspace. Returns the file count.
fn export_tree(doc: &Doc, ws: &dyn Workspace) -> std::io::Result<usize> {
    let mut files = 0;
    for node in doc.files().roots() {
        export_node(doc, ws, node, Path::new(""), &mut files)?;
    }
    Ok(files)
}

fn export_node(
    doc: &Doc,
    ws: &dyn Workspace,
    node: TreeID,
    parent: &Path,
    files: &mut usize,
) -> std::io::Result<()> {
    let tree = doc.files();
    let Some(name) = tree.name(node) else {
        return Ok(());
    };
    let rel = parent.join(&name);
    match tree.kind(node) {
        Some(NodeKind::Dir) => {
            ws.create_dir(&rel)?;
            for child in tree.children(node) {
                export_node(doc, ws, child, &rel, files)?;
            }
        }
        Some(NodeKind::File) => {
            let text = tree.content(node).map(|c| c.to_string()).unwrap_or_default();
            ws.write_file(&rel, text.as_bytes())?;
            *files += 1;
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

fn to_io<E: std::fmt::Display>(err: E) -> std::io::Error {
    std::io::Error::other(err.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use workspace::MemWorkspace;

    #[test]
    fn round_trips_through_an_in_memory_workspace() {
        // Build a tree in memory: README.md, src/main.rs, src/sub/deep.txt.
        let src = MemWorkspace::new();
        src.write_file(Path::new("README.md"), b"gib eg\n").unwrap();
        src.write_file(Path::new("src/main.rs"), b"fn main() {}\n").unwrap();
        src.write_file(Path::new("src/nested/deep.txt"), b"dark fantasies\n").unwrap();

        // Snapshot it into a doc, then restore into a fresh workspace.
        let doc = Doc::new();
        let imported = import_tree(&src, &doc).unwrap();
        assert_eq!(imported, 3);

        let bytes = doc.snapshot().unwrap();
        let restored_doc = Doc::from_snapshot(&bytes).unwrap();
        let dst = MemWorkspace::new();
        let exported = export_tree(&restored_doc, &dst).unwrap();
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
}
