//! Local, per-repo path filtering.
//!
//! Patterns in `.eg/ignore` (gitignore syntax) are skipped both ways: never
//! imported into the doc, never materialized back to disk.

use std::io;
use std::path::Path;

// The local module is named `ignore` too, so reach the crate explicitly.
use ::ignore::gitignore::{Gitignore, GitignoreBuilder};

use crate::workspace::Workspace;

/// The ignore patterns, under the repo's `.eg/`.
const IGNORE_PATH: &str = ".eg/ignore";

/// A compiled set of `.eg/ignore` patterns, matched on both import and export.
pub struct Ignorer {
    matcher: Gitignore,
}

impl Ignorer {
    /// Load `.eg/ignore`. A missing file ignores nothing.
    pub fn load(ws: &dyn Workspace) -> io::Result<Self> {
        let text = match ws.read_file(Path::new(IGNORE_PATH)) {
            Ok(bytes) => String::from_utf8(bytes)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?,
            Err(e) if e.kind() == io::ErrorKind::NotFound => String::new(),
            Err(e) => return Err(e),
        };
        // The root is only used to anchor absolute-ish patterns; our paths are
        // repo-relative, so any stable root works.
        let mut builder = GitignoreBuilder::new("");
        for line in text.lines() {
            builder
                .add_line(None, line)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        }
        let matcher = builder
            .build()
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        Ok(Self { matcher })
    }

    /// Skip `rel` (repo-relative) on both import and export? `is_dir` lets
    /// directory-only patterns like `target/` match and prune the subtree.
    pub fn should_ignore(&self, rel: &Path, is_dir: bool) -> bool {
        self.matcher
            .matched_path_or_any_parents(rel, is_dir)
            .is_ignore()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workspace::MemWorkspace;

    #[test]
    fn no_ignore_file_should_ignore_nothing() {
        let ws = MemWorkspace::new();
        let ignorer = Ignorer::load(&ws).unwrap();

        assert!(!ignorer.should_ignore(Path::new("foo"), false));
        assert!(!ignorer.should_ignore(Path::new("bar/baz"), false));
    }

    #[test]
    fn plain_ignore_file_should_ignore_patterns() {
        let ws = MemWorkspace::new();
        ws.write_file(Path::new(IGNORE_PATH), b"foo\nbar/baz\n")
            .unwrap();
        let ignorer = Ignorer::load(&ws).unwrap();

        assert!(ignorer.should_ignore(Path::new("foo"), false));
        assert!(ignorer.should_ignore(Path::new("bar/baz"), false));
        assert!(!ignorer.should_ignore(Path::new("bar/qux"), false));
        assert!(ignorer.should_ignore(Path::new("bar/foo"), false));
    }

    #[test]
    fn ignore_patterns_apply_to_subpaths() {
        let ws = MemWorkspace::new();
        ws.write_file(Path::new(IGNORE_PATH), b"foo\n").unwrap();
        let ignorer = Ignorer::load(&ws).unwrap();

        assert!(ignorer.should_ignore(Path::new("foo"), false));
        assert!(ignorer.should_ignore(Path::new("bar/foo"), false));
    }

    #[test]
    fn directory_pattern_should_only_ignore_directories() {
        let ws = MemWorkspace::new();
        ws.write_file(Path::new(IGNORE_PATH), b"target/\n").unwrap();
        let ignorer = Ignorer::load(&ws).unwrap();

        assert!(ignorer.should_ignore(Path::new("target"), true));
        assert!(!ignorer.should_ignore(Path::new("target"), false));
    }

    #[test]
    fn negated_pattern_should_unignore() {
        let ws = MemWorkspace::new();
        ws.write_file(Path::new(IGNORE_PATH), b"*.log\n!no-ignore.log")
            .unwrap();
        let ignorer = Ignorer::load(&ws).unwrap();

        assert!(ignorer.should_ignore(Path::new("ignore.log"), true));
        assert!(!ignorer.should_ignore(Path::new("no-ignore.log"), false));
    }

    #[test]
    fn invalid_utf8_in_ignore_file_should_error() {
        let ws = MemWorkspace::new();
        ws.write_file(Path::new(IGNORE_PATH), &[0xff, 0xfe, 0xfd])
            .unwrap();
        let result = Ignorer::load(&ws);
        assert_eq!(
            result.err().map(|e| e.kind()),
            Some(io::ErrorKind::InvalidData)
        );
    }
}
