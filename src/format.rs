//! Document formatting via a pluggable Formatter (real backend = m1-fmt, Task 8).
use crate::document::Document;
use crate::line_index::PositionEncoding;
use tower_lsp::lsp_types::{Position, Range, TextEdit};

/// Produces formatted text for a document. Abstracted so the server is testable
/// without m1-fmt, and so the real m1-fmt backend can be swapped in (Task 8).
pub trait Formatter: Send + Sync {
    /// Returns the fully formatted text, or None if no change / cannot format.
    fn format(&self, src: &str) -> Option<String>;
}

/// Identity formatter: never changes anything. Default until m1-fmt lands.
pub struct NoFormat;
impl Formatter for NoFormat {
    fn format(&self, _src: &str) -> Option<String> {
        None
    }
}

/// Build the whole-document TextEdit list for a formatted result.
pub fn format_edits(
    doc: &Document,
    enc: PositionEncoding,
    formatter: &dyn Formatter,
) -> Option<Vec<TextEdit>> {
    let formatted = formatter.format(&doc.text)?;
    if formatted == doc.text {
        return None;
    }
    let end = doc.line_index.position(doc.text.len(), enc);
    Some(vec![TextEdit {
        range: Range::new(Position::new(0, 0), end),
        new_text: formatted,
    }])
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Upper;
    impl Formatter for Upper {
        fn format(&self, s: &str) -> Option<String> {
            let up = s.to_uppercase();
            if up == s {
                None
            } else {
                Some(up)
            }
        }
    }

    #[test]
    fn no_change_returns_none() {
        let doc = Document::new("ABC\n".into(), 1);
        assert!(format_edits(&doc, PositionEncoding::Utf16, &Upper).is_none());
    }

    #[test]
    fn change_returns_full_range_edit() {
        let doc = Document::new("abc\n".into(), 1);
        let edits = format_edits(&doc, PositionEncoding::Utf16, &Upper).unwrap();
        assert_eq!(edits.len(), 1);
        assert_eq!(edits[0].new_text, "ABC\n");
        assert_eq!(edits[0].range.start, Position::new(0, 0));
        assert_eq!(edits[0].range.end, Position::new(1, 0));
    }

    #[test]
    fn noformat_is_identity() {
        let doc = Document::new("abc\n".into(), 1);
        assert!(format_edits(&doc, PositionEncoding::Utf16, &NoFormat).is_none());
    }
}
