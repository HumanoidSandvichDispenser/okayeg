//! Comments anchored to file content.
//!
//! Comments live in a top-level map keyed by comment id, one open map per
//! comment. Core interprets the anchor (two encoded stable cursors plus the
//! file node id), the `parent` link for replies, and the `created_at` stamp.
//! Every other key (`body`, `author`, `quote`, `resolved`, ...) is an opaque
//! last-writer-wins scalar owned by the consumer. Deleting a comment removes
//! its key from the map.

use std::ops::Range;
use std::sync::atomic::Ordering;

use loro::cursor::{Cursor, Side};
use loro::{LoroMap, LoroValue, TreeID};

use crate::Doc;

/// Keys core interprets. They are written at creation and refused by
/// [`Comments::set`].
const RESERVED: &[&str] = &["file", "start", "end", "parent", "created_at"];

impl Doc {
    /// The comments held in this doc.
    pub fn comments(&self) -> Comments<'_> {
        Comments { doc: self }
    }
}

/// A view over the comments held in a [`Doc`].
///
/// Reads or writes the comments container on each call. Edits apply to the
/// local copy and write into history at the doc's next [`commit`](Doc::commit).
pub struct Comments<'a> {
    doc: &'a Doc,
}

/// A comment read back out of the doc.
pub struct Comment {
    /// The comment's id, its key in the comments map.
    pub id: String,
    /// The file the comment is anchored to. `None` on replies.
    pub file: Option<TreeID>,
    /// The comment this one replies to. `None` on top-level comments.
    pub parent: Option<String>,
    /// Creation time as given to [`Comments::add`], milliseconds since epoch.
    pub created_at: i64,
    /// The anchor resolved against the current text. `None` when the comment
    /// is a reply, the file is gone, or the anchor no longer resolves.
    pub range: Option<Range<usize>>,
    /// True when the anchored text was deleted; `range` then points at the
    /// nearest surviving position rather than the original span.
    pub orphaned: bool,
    /// Every key core does not interpret, in map iteration order.
    pub fields: Vec<(String, LoroValue)>,
}

impl Comments<'_> {
    /// The name of the map container holding all comments.
    const MAP: &'static str = "comments";

    fn map(&self) -> LoroMap {
        self.doc.inner().get_map(Self::MAP)
    }

    fn comment(&self, id: &str) -> Option<LoroMap> {
        self.map().get(id)?.into_container().ok()?.into_map().ok()
    }

    /// A fresh, globally unique comment id.
    fn next_id(&self) -> String {
        let n = self.doc.comment_seq.fetch_add(1, Ordering::Relaxed);
        format!("{:016x}-{n}", self.doc.peer_id())
    }

    /// Anchor a new comment to `range` in `file`'s content, with `created_at` in
    /// milliseconds since epoch and any consumer fields set alongside.
    ///
    /// The range is in unicode codepoints, end exclusive; the start anchors left
    /// and the end right, so text typed inside the span grows it. Returns the new
    /// comment's id, or `None` when `file` is not a file node or the range does not
    /// fit its content.
    pub fn add<K: AsRef<str>>(
        &self,
        file: TreeID,
        range: Range<usize>,
        created_at: i64,
        fields: &[(K, LoroValue)],
    ) -> Option<String> {
        let content = self.doc.files().content(file)?;
        if range.start > range.end || range.end > content.len_unicode() {
            return None;
        }
        let start = content.get_cursor(range.start, Side::Left)?;
        let end = content.get_cursor(range.end, Side::Right)?;

        let id = self.next_id();
        let comment = self
            .map()
            .insert_container(&id, LoroMap::new())
            .expect("insert comment map");

        comment
            .insert("file", file.to_string())
            .expect("insert file");
        comment
            .insert("start", start.encode())
            .expect("insert start");
        comment.insert("end", end.encode()).expect("insert end");
        comment
            .insert("created_at", created_at)
            .expect("insert created_at");
        for (key, value) in fields {
            comment
                .insert(key.as_ref(), value.clone())
                .expect("insert field");
        }

        Some(id)
    }

    /// Add a reply to the comment `parent`, with `created_at` in milliseconds
    /// since epoch and any consumer fields to set alongside.
    ///
    /// A reply carries no anchor of its own. Returns the new comment's id,
    /// or `None` when `parent` does not exist.
    pub fn reply<K: AsRef<str>>(
        &self,
        parent: &str,
        created_at: i64,
        fields: &[(K, LoroValue)],
    ) -> Option<String> {
        self.comment(parent)?;

        let id = self.next_id();
        let comment = self
            .map()
            .insert_container(&id, LoroMap::new())
            .expect("insert comment map");

        comment.insert("parent", parent).expect("insert parent");
        comment
            .insert("created_at", created_at)
            .expect("insert created_at");
        for (key, value) in fields {
            comment
                .insert(key.as_ref(), value.clone())
                .expect("insert field");
        }

        Some(id)
    }

    /// Set a consumer field on a comment. Returns `false` when the comment
    /// does not exist or `key` is one core interprets.
    pub fn set(&self, id: &str, key: &str, value: impl Into<LoroValue>) -> bool {
        if RESERVED.contains(&key) {
            return false;
        }
        match self.comment(id) {
            Some(comment) => {
                comment.insert(key, value).expect("insert field");
                true
            }
            None => false,
        }
    }

    /// Remove a comment. Replies to it stay and keep their `parent` link.
    pub fn remove(&self, id: &str) {
        self.map().delete(id).expect("delete comment");
    }

    /// Read one comment, its anchor resolved against the current text.
    pub fn get(&self, id: &str) -> Option<Comment> {
        Some(self.read(id, &self.comment(id)?))
    }

    /// All comments, or with `file` given only those anchored to that file
    /// (which leaves out replies; follow `parent` links to gather threads).
    pub fn list(&self, file: Option<TreeID>) -> Vec<Comment> {
        let map = self.map();
        let mut comments = Vec::new();
        for id in map.keys() {
            let Some(comment) = self.comment(&id) else {
                continue;
            };
            let comment = self.read(&id, &comment);
            if file.is_none() || comment.file == file {
                comments.push(comment);
            }
        }
        comments
    }

    fn read(&self, id: &str, comment: &LoroMap) -> Comment {
        let file = self
            .string_field(comment, "file")
            .and_then(|s| TreeID::try_from(s.as_str()).ok());
        let (range, orphaned) = match (
            self.cursor_field(comment, "start"),
            self.cursor_field(comment, "end"),
        ) {
            (Some(start), Some(end)) => self.resolve(&start, &end),
            _ => (None, false),
        };
        let created_at = comment
            .get("created_at")
            .and_then(|v| v.as_value()?.as_i64().copied())
            .unwrap_or(0);

        let mut fields = Vec::new();
        for key in comment.keys() {
            if RESERVED.contains(&key.as_ref()) {
                continue;
            }
            if let Some(value) = comment.get(&key).and_then(|v| v.as_value().cloned()) {
                fields.push((key.to_string(), value));
            }
        }

        Comment {
            id: id.to_string(),
            file,
            parent: self.string_field(comment, "parent"),
            created_at,
            range,
            orphaned,
            fields,
        }
    }

    /// Resolve an anchor pair to a range in the current text. A cursor whose
    /// target was deleted resolves to the nearest surviving position and
    /// marks the range orphaned.
    fn resolve(&self, start: &Cursor, end: &Cursor) -> (Option<Range<usize>>, bool) {
        let doc = self.doc.inner();
        let (Ok(s), Ok(e)) = (doc.get_cursor_pos(start), doc.get_cursor_pos(end)) else {
            return (None, false);
        };
        let orphaned = s.update.is_some() || e.update.is_some();
        (
            Some(s.current.pos..e.current.pos.max(s.current.pos)),
            orphaned,
        )
    }

    fn string_field(&self, comment: &LoroMap, key: &str) -> Option<String> {
        Some(comment.get(key)?.as_value()?.as_string()?.to_string())
    }

    fn cursor_field(&self, comment: &LoroMap, key: &str) -> Option<Cursor> {
        let value = comment.get(key)?;
        let bytes = value.as_value()?.as_binary()?;
        Cursor::decode(bytes).ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An empty field list, typed so the key parameter infers.
    const NONE: &[(&str, LoroValue)] = &[];

    fn doc_with_file(text: &str) -> (Doc, TreeID) {
        let doc = Doc::new();
        let file = doc.files().create_file(None, "notes.txt");
        doc.files().set_content(file, text);
        (doc, file)
    }

    #[test]
    fn add_and_read_back() {
        let (doc, file) = doc_with_file("hello world");
        let id = doc
            .comments()
            .add(file, 6..11, 1000, &[("body", "nice".into())])
            .unwrap();

        let comment = doc.comments().get(&id).unwrap();
        assert_eq!(comment.file, Some(file));
        assert_eq!(comment.created_at, 1000);
        assert_eq!(comment.range, Some(6..11));
        assert!(!comment.orphaned);
        assert_eq!(
            comment.fields,
            vec![("body".to_string(), LoroValue::from("nice"))]
        );
    }

    #[test]
    fn anchor_follows_edits() {
        let (doc, file) = doc_with_file("hello world");
        let id = doc.comments().add(file, 6..11, 0, NONE).unwrap();

        let content = doc.files().content(file).unwrap();
        content.insert(0, "well, ").unwrap();
        assert_eq!(doc.comments().get(&id).unwrap().range, Some(12..17));

        // Typing inside the span grows it.
        content.insert(15, "!!").unwrap();
        assert_eq!(doc.comments().get(&id).unwrap().range, Some(12..19));
    }

    #[test]
    fn deleting_the_span_orphans() {
        let (doc, file) = doc_with_file("hello world");
        let id = doc.comments().add(file, 6..11, 0, NONE).unwrap();

        doc.files().content(file).unwrap().delete(5, 6).unwrap();
        let comment = doc.comments().get(&id).unwrap();
        assert!(comment.orphaned);
        assert_eq!(comment.range, Some(5..5));
    }

    #[test]
    fn anchors_survive_a_snapshot() {
        let (doc, file) = doc_with_file("hello world");
        let id = doc.comments().add(file, 0..5, 0, NONE).unwrap();
        doc.commit();

        let reopened = Doc::from_snapshot(&doc.snapshot().unwrap()).unwrap();
        assert_eq!(reopened.comments().get(&id).unwrap().range, Some(0..5));
    }

    #[test]
    fn anchors_survive_concurrent_edits() {
        let (doc, file) = doc_with_file("hello world");
        let id = doc.comments().add(file, 6..11, 0, NONE).unwrap();
        doc.commit();

        let peer = Doc::from_snapshot(&doc.snapshot().unwrap()).unwrap();
        peer.files()
            .content(file)
            .unwrap()
            .insert(0, "ah, ")
            .unwrap();
        peer.commit();

        doc.import(&peer.updates_since(&doc.version()).unwrap())
            .unwrap();
        assert_eq!(doc.comments().get(&id).unwrap().range, Some(10..15));
    }

    #[test]
    fn replies_thread_and_removal_keeps_them() {
        let (doc, file) = doc_with_file("hello world");
        let root = doc.comments().add(file, 0..5, 1, NONE).unwrap();
        let reply = doc
            .comments()
            .reply(&root, 2, &[("body", "agreed".into())])
            .unwrap();
        assert!(doc.comments().reply("missing", 3, NONE).is_none());

        let read = doc.comments().get(&reply).unwrap();
        assert_eq!(read.parent.as_deref(), Some(root.as_str()));
        assert_eq!(read.file, None);
        assert_eq!(read.range, None);

        // Listing by file yields anchored comments only.
        let listed = doc.comments().list(Some(file));
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].id, root);
        assert_eq!(doc.comments().list(None).len(), 2);

        doc.comments().remove(&root);
        assert!(doc.comments().get(&root).is_none());
        assert!(doc.comments().get(&reply).is_some());
    }

    #[test]
    fn set_guards_core_fields() {
        let (doc, file) = doc_with_file("hello world");
        let id = doc.comments().add(file, 0..5, 0, NONE).unwrap();

        assert!(doc.comments().set(&id, "resolved", true));
        assert!(!doc.comments().set(&id, "file", "0@0"));
        assert!(!doc.comments().set("missing", "body", "x"));

        let fields = doc.comments().get(&id).unwrap().fields;
        assert_eq!(
            fields,
            vec![("resolved".to_string(), LoroValue::from(true))]
        );
    }

    #[test]
    fn add_rejects_bad_targets() {
        let (doc, file) = doc_with_file("hi");
        let dir = doc.files().create_dir(None, "src");
        assert!(doc.comments().add(dir, 0..1, 0, NONE).is_none());
        assert!(doc.comments().add(file, 0..99, 0, NONE).is_none());
    }
}
