//! textDocument/foldingRange: fold `{ … }` blocks and multi-line block comments.
//!
//! VS Code has no built-in folding for `.m1scr` (Neovim gets it from
//! tree-sitter), so the server provides it for parity. Single-line constructs
//! are skipped — a fold that hides nothing is just noise.
use crate::line_index::{LineIndex, PositionEncoding};
use m1_core::{Kind, Node};
use tower_lsp::lsp_types::{FoldingRange, FoldingRangeKind};

pub fn folding_ranges(root: Node, li: &LineIndex, enc: PositionEncoding) -> Vec<FoldingRange> {
    let mut out = Vec::new();
    collect(root, li, enc, &mut out);
    out
}

fn collect(n: Node, li: &LineIndex, enc: PositionEncoding, out: &mut Vec<FoldingRange>) {
    let kind = match n.kind() {
        Kind::Block => Some(None),
        Kind::BlockComment => Some(Some(FoldingRangeKind::Comment)),
        _ => None,
    };
    if let Some(fold_kind) = kind {
        let r = n.byte_range();
        let start = li.position(r.start, enc);
        let end = li.position(r.end, enc);
        // Only fold when it actually spans more than one line.
        if end.line > start.line {
            out.push(FoldingRange {
                start_line: start.line,
                start_character: Some(start.character),
                end_line: end.line,
                end_character: Some(end.character),
                kind: fold_kind,
                collapsed_text: None,
            });
        }
    }
    for c in n.children() {
        collect(c, li, enc, out);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn folds_a_multiline_block() {
        // `if (cond)\n{ … }` is the corpus block form that yields a `Block` node.
        let src = "if (x eq 1)\n{\n  Reset = 1;\n  y = 2;\n}\n";
        let cst = m1_core::parse(src);
        let li = LineIndex::new(src);
        let folds = folding_ranges(cst.root(), &li, PositionEncoding::Utf16);
        assert!(
            folds.iter().any(|f| f.start_line == 1 && f.end_line == 4),
            "expected a block fold spanning the braces, got {folds:?}"
        );
    }

    #[test]
    fn folds_a_multiline_block_comment() {
        let src = "/* line one\n   line two */\nx = 1;\n";
        let cst = m1_core::parse(src);
        let li = LineIndex::new(src);
        let folds = folding_ranges(cst.root(), &li, PositionEncoding::Utf16);
        assert!(
            folds
                .iter()
                .any(|f| f.kind == Some(FoldingRangeKind::Comment) && f.start_line == 0),
            "expected a comment fold, got {folds:?}"
        );
    }

    #[test]
    fn does_not_fold_single_line_block() {
        let src = "when (a is equal to 1) { x = 1; }\n";
        let cst = m1_core::parse(src);
        let li = LineIndex::new(src);
        let folds = folding_ranges(cst.root(), &li, PositionEncoding::Utf16);
        assert!(
            folds.is_empty(),
            "single-line block should not fold: {folds:?}"
        );
    }
}
