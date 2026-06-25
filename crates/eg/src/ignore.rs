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
