//! In-memory document: full text, its line index, and the parsed CST.
//!
//! The CST is kept warm across edits (#270): `didChange` arrives as
//! incremental ranges (the server advertises `TextDocumentSyncKind::
//! INCREMENTAL`), each converted to an `m1_core::Edit` so tree-sitter reuses
//! every subtree the edit didn't touch — the fast path for editor keystrokes
//! — instead of re-parsing the document from scratch per change. Request
//! handlers share the same tree via `Arc`, so a request after a keystroke
//! costs a pointer clone, not a parse.
use crate::line_index::{LineIndex, PositionEncoding};
use std::sync::Arc;
use tower_lsp::lsp_types::Range;

pub struct Document {
    pub text: String,
    pub line_index: LineIndex,
    pub version: i32,
    pub cst: Arc<m1_core::Cst>,
}

impl Document {
    pub fn new(text: String, version: i32) -> Self {
        let line_index = LineIndex::new(&text);
        let cst = Arc::new(m1_core::parse(&text));
        Self {
            text,
            line_index,
            version,
            cst,
        }
    }

    /// Apply one `TextDocumentContentChangeEvent` worth of change. A ranged
    /// change splices the text and reparses incrementally; `range: None` is
    /// the full-replacement fallback (also what pre-incremental clients send).
    pub fn apply_change(&mut self, range: Option<Range>, new_text: &str, enc: PositionEncoding) {
        let Some(range) = range else {
            self.text = new_text.to_string();
            self.line_index = LineIndex::new(&self.text);
            self.cst = Arc::new(m1_core::parse(&self.text));
            return;
        };
        let start = self.line_index.offset(range.start, enc);
        let end = self.line_index.offset(range.end, enc).max(start);
        let edit = m1_core::Edit {
            start_byte: start,
            old_end_byte: end,
            new_end_byte: start + new_text.len(),
        };
        self.text.replace_range(start..end, new_text);
        self.line_index = LineIndex::new(&self.text);
        self.cst = Arc::new(self.cst.reparse(&edit, &self.text));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tower_lsp::lsp_types::Position;

    fn range(sl: u32, sc: u32, el: u32, ec: u32) -> Range {
        Range::new(Position::new(sl, sc), Position::new(el, ec))
    }

    /// Flatten a tree to `(kind, start, end)` depth-first so two parses can be
    /// compared structurally (m1-core exposes no s-expression dump).
    fn shape(cst: &m1_core::Cst) -> Vec<(m1_core::Kind, usize, usize)> {
        fn walk(n: m1_core::Node, out: &mut Vec<(m1_core::Kind, usize, usize)>) {
            let r = n.byte_range();
            out.push((n.kind(), r.start, r.end));
            for c in n.children() {
                walk(c, out);
            }
        }
        let mut out = Vec::new();
        walk(cst.root(), &mut out);
        out
    }

    #[test]
    fn ranged_change_matches_full_parse() {
        // #270: the incrementally-reparsed tree must be byte-for-byte the
        // tree a fresh parse of the new text produces.
        let mut doc = Document::new("local x = 1;\nlocal y = 2;\n".into(), 1);
        // Replace `1` with `42` (line 0, cols 10..11).
        doc.apply_change(Some(range(0, 10, 0, 11)), "42", PositionEncoding::Utf16);
        assert_eq!(doc.text, "local x = 42;\nlocal y = 2;\n");
        let fresh = m1_core::parse(&doc.text);
        assert_eq!(shape(&doc.cst), shape(&fresh));
        assert!(doc.cst.syntax_diagnostics().is_empty());
    }

    #[test]
    fn multibyte_insertion_keeps_tree_consistent() {
        let mut doc = Document::new("local s = \"abc\";\n".into(), 1);
        // Insert `°` inside the string (UTF-16 col 12 == byte 12 here).
        doc.apply_change(Some(range(0, 12, 0, 12)), "°", PositionEncoding::Utf16);
        assert_eq!(doc.text, "local s = \"a°bc\";\n");
        let fresh = m1_core::parse(&doc.text);
        assert_eq!(shape(&doc.cst), shape(&fresh));
    }

    /// #290: a backwards/inverted range (end < start) is clamped to a
    /// zero-width range at `start` so the server never panics or corrupts
    /// state. The resulting buffer text and CST must match a fresh parse of
    /// the same final text.
    #[test]
    fn backwards_range_clamped_to_zero_width() {
        // Document: "local x = 1;\n"
        // Send a ranged change with end < start (col 5 → col 3 on line 0).
        // The clamp turns this into a zero-width insertion at col 5,
        // i.e. the new_text is just inserted there and nothing is deleted.
        let mut doc = Document::new("local x = 1;\n".into(), 1);
        // Inverted range: start=(0,5) end=(0,3) — end is before start.
        doc.apply_change(Some(range(0, 5, 0, 3)), "_extra", PositionEncoding::Utf16);
        // With end clamped to start (byte 5), replace_range(5..5, "_extra")
        // inserts at position 5: "local_extra x = 1;\n"
        let expected = "local_extra x = 1;\n";
        assert_eq!(doc.text, expected);
        let fresh = m1_core::parse(&doc.text);
        assert_eq!(shape(&doc.cst), shape(&fresh));
    }

    #[test]
    fn full_replacement_fallback_still_works() {
        let mut doc = Document::new("local x = 1;\n".into(), 1);
        doc.apply_change(None, "local y = 2;\n", PositionEncoding::Utf16);
        assert_eq!(doc.text, "local y = 2;\n");
        assert_eq!(shape(&doc.cst), shape(&m1_core::parse(&doc.text)));
    }

    #[test]
    fn sequential_changes_apply_in_order() {
        // LSP: each range refers to the document state AFTER the previous
        // change in the same notification.
        let mut doc = Document::new("a = 1;\n".into(), 1);
        doc.apply_change(Some(range(0, 4, 0, 5)), "2", PositionEncoding::Utf16);
        doc.apply_change(Some(range(0, 0, 0, 1)), "bb", PositionEncoding::Utf16);
        assert_eq!(doc.text, "bb = 2;\n");
        assert_eq!(shape(&doc.cst), shape(&m1_core::parse(&doc.text)));
    }
}
