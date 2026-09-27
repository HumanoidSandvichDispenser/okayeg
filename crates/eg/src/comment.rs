use loro::{LoroValue, ToJson};
use std::path::Path;

use clap::Subcommand;

use crate::net::load_doc;

#[derive(Subcommand)]
pub enum CommentAction {
    List {
        /// A path to a file in the doc to list comments for, or none for all comments.
        file: Option<String>,
    },
}

/// The 1-based line and column of the `offset`th code point in `text`, or
/// `None` past the end. Columns count code points.
pub fn offset_to_line_col(text: &str, offset: usize) -> Option<(usize, usize)> {
    let mut line = 1;
    let mut col = 1;
    let mut chars = text.chars();

    for _ in 0..offset {
        if chars.next()? == '\n' {
            line += 1;
            col = 1;
        } else {
            col += 1;
        }
    }

    Some((line, col))
}

/// The code-point offset of 1-based `line` and `col` in `text`, or `None` when
/// that position does not exist.
pub fn line_col_to_offset(text: &str, line: usize, col: usize) -> Option<usize> {
    if line == 0 || col == 0 {
        return None;
    }

    let mut current_line = 1;
    let mut current_col = 1;
    let mut offset = 0;

    for c in text.chars() {
        if current_line == line && current_col == col {
            return Some(offset);
        }

        if c == '\n' {
            current_line += 1;
            current_col = 1;
        } else {
            current_col += 1;
        }

        offset += 1;
    }

    if current_line == line && current_col == col {
        return Some(offset);
    }

    None
}

pub fn handle_comment_action(dir: &Path, action: CommentAction) -> std::io::Result<()> {
    let doc = load_doc(dir)?;

    match action {
        CommentAction::List { file } => {
            // if file is specified, find the Loro TreeID associated with it and list comments for
            // that TreeID, otherwise list all comments
            let tree_id = file.as_ref().and_then(|f| doc.fs().resolve(f).ok());

            let comments = doc.comments().list(tree_id);

            for comment in comments {
                let filename = comment
                    .file
                    .and_then(|tree_id| doc.fs().path_of(tree_id))
                    .unwrap_or_else(|| "<unknown>".to_string());

                // TODO: can probably format the comments better and also group by thread to show
                // replies, which show <unknown> currently
                let range_fmt = comment
                    .range
                    .map(|r| {
                        let start = offset_to_line_col(
                            &doc.fs().read(&filename).unwrap_or_default(),
                            r.start,
                        )
                        .map(|(line, col)| format!("{}.{}", line, col))
                        .unwrap_or_else(|| r.start.to_string());

                        if r.start == r.end {
                            format!(":{}", start)
                        } else {
                            let end = offset_to_line_col(
                                &doc.fs().read(&filename).unwrap_or_default(),
                                r.end,
                            )
                            .map(|(line, col)| format!("{}.{}", line, col))
                            .unwrap_or_else(|| r.end.to_string());

                            format!(":{}-{}", start, end)
                        }
                    })
                    .unwrap_or_else(|| "".to_string());

                let fields = comment.fields.iter().map(|(k, v)| {
                    let value_str: String = match v {
                        LoroValue::String(s) => s.to_string(),
                        _ => v.to_json_value().to_string(),
                    };

                    format!("\t{}: {}", k, value_str)
                });

                println!("Comment {} @ {}{}", comment.id, filename, range_fmt);
                println!("{}", fields.collect::<Vec<_>>().join("\n"));
            }

            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn offset_to_line_col_returns_line_col() {
        let text = "Hello\nWorld\nThis is a test.";
        assert_eq!(offset_to_line_col(text, 0), Some((1, 1)));
        assert_eq!(offset_to_line_col(text, 5), Some((1, 6)));
        assert_eq!(offset_to_line_col(text, 6), Some((2, 1)));
        assert_eq!(offset_to_line_col(text, 11), Some((2, 6)));
        assert_eq!(offset_to_line_col(text, 12), Some((3, 1)));
        assert_eq!(offset_to_line_col(text, 26), Some((3, 15)));
        assert_eq!(offset_to_line_col(text, 27), Some((3, 16)));
        assert_eq!(offset_to_line_col(text, 28), None);
    }

    #[test]
    fn line_col_to_offset_returns_offset() {
        let text = "Hello\nWorld\nThis is a test.";
        assert_eq!(line_col_to_offset(text, 1, 1), Some(0));
        assert_eq!(line_col_to_offset(text, 1, 6), Some(5));
        assert_eq!(line_col_to_offset(text, 2, 1), Some(6));
        assert_eq!(line_col_to_offset(text, 2, 6), Some(11));
        assert_eq!(line_col_to_offset(text, 3, 1), Some(12));
        assert_eq!(line_col_to_offset(text, 3, 15), Some(26));
        assert_eq!(line_col_to_offset(text, 4, 1), None);
    }

    #[test]
    fn line_col_counts_code_points_not_bytes() {
        let text = "é🦀x\nyé";
        assert_eq!(offset_to_line_col(text, 2), Some((1, 3)));
        assert_eq!(offset_to_line_col(text, 4), Some((2, 1)));
        assert_eq!(offset_to_line_col(text, 6), Some((2, 3)));
        assert_eq!(offset_to_line_col(text, 7), None);
        assert_eq!(line_col_to_offset(text, 1, 3), Some(2));
        assert_eq!(line_col_to_offset(text, 2, 3), Some(6));

        for offset in 0..=text.chars().count() {
            let (line, col) = offset_to_line_col(text, offset).unwrap();
            assert_eq!(line_col_to_offset(text, line, col), Some(offset));
        }
    }
}
