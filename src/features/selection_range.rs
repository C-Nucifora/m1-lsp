//! textDocument/selectionRange: hierarchical "expand selection" ranges. For a
//! requested position, return the chain of enclosing CST nodes from the leaf
//! outward, so the editor can grow the selection statement → block → `if`/`when`
//! → file (#173).
use crate::convert::range;
use crate::features::locate::node_at_byte;
use crate::line_index::{LineIndex, PositionEncoding};
use m1_core::Node;
use tower_lsp::lsp_types::SelectionRange;

/// The `SelectionRange` for `byte`: the innermost node, with `.parent` linking
/// outward through each distinctly-sized ancestor to the root.
pub fn selection_range(
    root: Node,
    byte: usize,
    li: &LineIndex,
    enc: PositionEncoding,
) -> Option<SelectionRange> {
    let leaf = node_at_byte(root, byte)?;
    // Innermost-first list of distinct spans up the parent chain.
    let mut spans = vec![leaf.byte_range()];
    let mut node = leaf;
    while let Some(parent) = node.parent() {
        let pr = parent.byte_range();
        if Some(&pr) != spans.last() {
            spans.push(pr);
        }
        node = parent;
    }
    // Fold outermost → innermost so each new range carries the previous as parent.
    let mut sel: Option<SelectionRange> = None;
    for span in spans.iter().rev() {
        sel = Some(SelectionRange {
            range: range(span, li, enc),
            parent: sel.map(Box::new),
        });
    }
    sel
}

#[cfg(test)]
mod tests {
    use super::*;

    fn depth(mut s: &SelectionRange) -> usize {
        let mut n = 1;
        while let Some(p) = &s.parent {
            n += 1;
            s = p;
        }
        n
    }

    #[test]
    fn grows_from_statement_out_to_the_if_block() {
        let src = "if (a)\n{\nValue = 1;\n}\n";
        let cst = m1_core::parse(src);
        let li = LineIndex::new(src);
        // Cursor on `Value` inside the block.
        let byte = src.find("Value").unwrap();
        let sel = selection_range(cst.root(), byte, &li, PositionEncoding::Utf16)
            .expect("a selection range at a valid position");
        // The innermost range covers `Value`; the chain widens to the whole file.
        assert!(
            depth(&sel) >= 3,
            "expected nested ranges (stmt < block < if)"
        );
        // Each parent strictly contains its child.
        let mut cur = &sel;
        while let Some(parent) = &cur.parent {
            let inner = (cur.range.start, cur.range.end);
            let outer = (parent.range.start, parent.range.end);
            assert!(
                outer.0 <= inner.0 && inner.1 <= outer.1,
                "parent must contain child"
            );
            cur = parent;
        }
    }
}
