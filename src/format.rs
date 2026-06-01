//! Document formatting via a pluggable Formatter (real backend = m1-fmt, Task 8).
use crate::document::Document;
use crate::line_index::PositionEncoding;
use tower_lsp::lsp_types::{Position, Range, TextEdit};

/// Produces formatted text for a document. Abstracted so the server is testable
/// without m1-fmt, and so the real m1-fmt backend can be swapped in (Task 8).
pub trait Formatter: Send + Sync {
    /// Returns the fully formatted text, or None if no change / cannot format.
    fn format(&self, src: &str) -> Option<String>;

    /// Format only the statements overlapping the 0-based inclusive line range,
    /// returning `(covered_start_line, covered_end_line, replacement_text)` —
    /// the span (snapped outward to whole statements) the replacement covers.
    /// `None` if nothing overlaps or the formatter can't range-format. Default:
    /// unsupported.
    fn format_range(
        &self,
        _src: &str,
        _start_line: u32,
        _end_line: u32,
    ) -> Option<(u32, u32, String)> {
        None
    }
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

/// Build the TextEdit list for formatting just the lines overlapping `range`.
/// The formatter snaps outward to whole statements; the resulting edit replaces
/// the covered whole lines, leaving the rest of the document untouched.
pub fn range_format_edits(
    doc: &Document,
    range: Range,
    formatter: &dyn Formatter,
) -> Option<Vec<TextEdit>> {
    let (start_line, end_line, new_text) =
        formatter.format_range(&doc.text, range.start.line, range.end.line)?;
    // Replace whole lines [start_line, end_line]: from the start of start_line to
    // the start of the line after end_line. `new_text` already carries a trailing
    // newline, so this splices cleanly.
    let edit_range = Range::new(Position::new(start_line, 0), Position::new(end_line + 1, 0));
    Some(vec![TextEdit {
        range: edit_range,
        new_text,
    }])
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Upper;
    impl Formatter for Upper {
        fn format(&self, s: &str) -> Option<String> {
            let up = s.to_uppercase();
            if up == s { None } else { Some(up) }
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

    struct FixedRange;
    impl Formatter for FixedRange {
        fn format(&self, _s: &str) -> Option<String> {
            None
        }
        fn format_range(&self, _s: &str, _a: u32, _b: u32) -> Option<(u32, u32, String)> {
            // Pretend the covered span is lines 1..=1.
            Some((1, 1, "FORMATTED\n".to_string()))
        }
    }

    #[test]
    fn range_edit_replaces_covered_whole_lines() {
        let doc = Document::new("a\nb\nc\n".into(), 1);
        let req = Range::new(Position::new(1, 0), Position::new(1, 1));
        let edits = range_format_edits(&doc, req, &FixedRange).unwrap();
        assert_eq!(edits.len(), 1);
        // Replaces line 1 wholesale: [1,0)..[2,0).
        assert_eq!(
            edits[0].range,
            Range::new(Position::new(1, 0), Position::new(2, 0))
        );
        assert_eq!(edits[0].new_text, "FORMATTED\n");
    }

    #[test]
    fn range_edit_none_when_unsupported() {
        let doc = Document::new("a\nb\n".into(), 1);
        let req = Range::new(Position::new(0, 0), Position::new(0, 1));
        // NoFormat uses the default format_range (always None).
        assert!(range_format_edits(&doc, req, &NoFormat).is_none());
    }
}
