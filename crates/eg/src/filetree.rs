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
            return Err(std::io::Error::other(format!(
                "cannot access '{path}': {e}"
            )));
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

#[cfg(test)]
mod tests {
    use super::*;

    fn doc_with_files() -> Doc {
        let doc = Doc::new();

        doc.fs().create_dir("src").unwrap();
        doc.fs().create_file("src/main.typ").unwrap();
        doc.fs().create_file("src/file2.typ").unwrap();
        doc.fs()
            .text("src/main.typ")
            .unwrap()
            .insert(0, "hello")
            .unwrap();
        doc.fs()
            .text("src/file2.typ")
            .unwrap()
            .insert(0, "world")
            .unwrap();

        doc
    }

    #[test]
    fn ls_lists_root_directory() {
        let doc = doc_with_files();
        let listing = ls(&doc, "").unwrap();

        assert!(matches!(listing, Listing::Dir(_)));
        let Listing::Dir(entries) = listing else {
            panic!("expected directory listing");
        };

        let names: Vec<_> = entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names.len(), 1);
        assert_eq!(names[0], "src");
    }

    #[test]
    fn ls_lists_directory() {
        let doc = doc_with_files();
        let listing = ls(&doc, "src").unwrap();

        assert!(matches!(listing, Listing::Dir(_)));
        let Listing::Dir(entries) = listing else {
            panic!("expected directory listing");
        };

        let names: Vec<_> = entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names.len(), 2);
        assert!(names.contains(&"main.typ"));
        assert!(names.contains(&"file2.typ"));
    }

    #[test]
    fn ls_returns_leaf_for_file() {
        let doc = doc_with_files();
        let listing = ls(&doc, "src/main.typ").unwrap();

        assert!(matches!(listing, Listing::Leaf));
    }

    #[test]
    fn ls_on_missing_path_returns_error() {
        let doc = doc_with_files();
        let result = ls(&doc, "missing");

        assert!(matches!(result, Err(FsError::NotFound)));
    }

    #[test]
    fn ls_returns_error_for_file_as_directory() {
        let doc = doc_with_files();
        let result = ls(&doc, "src/main.typ/extra");

        assert!(matches!(result, Err(FsError::NotADirectory)));
    }

    #[test]
    fn cat_returns_contents_for_existing_files() {
        let doc = doc_with_files();
        let paths = vec!["src/main.typ".to_owned()];
        let results = cat(&doc, &paths);

        assert_eq!(results.len(), 1);
        assert_eq!(results[0].as_ref().unwrap(), "hello");
    }

    #[test]
    fn cat_returns_contents_for_multiple_existing_files() {
        let doc = doc_with_files();
        let paths = vec!["src/main.typ".to_owned(), "src/file2.typ".to_owned()];
        let results = cat(&doc, &paths);

        assert_eq!(results.len(), 2);
        assert_eq!(results[0].as_ref().unwrap(), "hello");
        assert_eq!(results[1].as_ref().unwrap(), "world");
    }

    #[test]
    fn cat_returns_error_for_missing_files() {
        let doc = doc_with_files();
        let paths = vec!["missing.typ".to_owned()];
        let results = cat(&doc, &paths);

        assert_eq!(results.len(), 1);
        assert!(matches!(results[0], Err(FsError::NotFound)));
    }

    #[test]
    fn cat_returns_error_for_directories() {
        let doc = doc_with_files();
        let paths = vec!["src".to_owned()];
        let results = cat(&doc, &paths);

        assert_eq!(results.len(), 1);
        assert!(matches!(results[0], Err(FsError::NotAFile)));
    }

    #[test]
    fn cat_returns_mixed_results_for_existing_and_missing_files() {
        let doc = doc_with_files();
        let paths = vec![
            "src/main.typ".to_owned(),
            "missing.typ".to_owned(),
            "src/file2.typ".to_owned(),
        ];
        let results = cat(&doc, &paths);

        assert_eq!(results.len(), 3);
        assert_eq!(results[0].as_ref().unwrap(), "hello");
        assert!(matches!(results[1], Err(FsError::NotFound)));
        assert_eq!(results[2].as_ref().unwrap(), "world");
    }
}
