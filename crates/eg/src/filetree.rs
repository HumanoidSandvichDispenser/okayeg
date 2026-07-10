use std::path::Path;

use okayeg::{Doc, Entry, FsError};

use crate::net::load_doc;

pub enum Listing {
    Dir(Vec<Entry>),
    Leaf,
}

/// What is at a path: a directory's entries, or the leaf itself when the path
/// names a file or boundary.
pub fn ls(doc: &Doc, path: &str) -> Result<Listing, FsError> {
    match doc.fs().readdir(path) {
        Ok(entries) => Ok(Listing::Dir(entries)),
        Err(FsError::NotADirectory) => doc.fs().stat(path).map(|_| Listing::Leaf),
        Err(e) => Err(e),
    }
}

/// Read each path's content, one result per path in order.
pub fn cat(doc: &Doc, paths: &[String]) -> Vec<Result<String, FsError>> {
    let fs = doc.fs();
    paths.iter().map(|path| fs.read(path)).collect()
}

/// List `path` in the repo's doc to stdout. A directory prints its entries,
/// anything else prints the path itself, like ls.
pub fn ls_stdio(eg_dir: &Path, path: &str) -> std::io::Result<()> {
    let doc = load_doc(eg_dir)?;

    match ls(&doc, path) {
        Ok(Listing::Dir(entries)) => {
            for entry in entries {
                println!("{}", entry.name);
            }
        }
        Ok(Listing::Leaf) => {
            println!("{path}");
        }
        Err(e) => {
            return Err(std::io::Error::other(format!("cannot access '{path}': {e}")));
        }
    }
    Ok(())
}

/// Print each path's content from the repo's doc to stdout, in order. A path
/// that cannot be read reports to stderr and the rest still print, but the
/// command fails, like cat.
pub fn cat_stdio(eg_dir: &Path, paths: &[String]) -> std::io::Result<()> {
    let doc = load_doc(eg_dir)?;
    let mut failed = false;

    for (path, result) in paths.iter().zip(cat(&doc, paths)) {
        match result {
            Ok(contents) => print!("{contents}"),
            Err(e) => {
                eprintln!("eg: {path}: {e}");
                failed = true;
            }
        }
    }

    if failed {
        // Each failure was already reported per path; fail with no message,
        // matching cat's bare nonzero exit.
        return Err(std::io::Error::other(""));
    }
    Ok(())
}
