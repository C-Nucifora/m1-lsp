//! Local (file-scoped) rename: identifiers declared with `local`. Rewritten
//! in-buffer; member-access properties (`Foo.count`) and type-annotation names
//! (`<Count>`) are left alone.
use super::helpers::is_valid_identifier;
use crate::convert::range as to_range;
use crate::features::locate::{
    collect_locals, in_type_annotation, is_member_property, node_at_byte,
};
use crate::line_index::{LineIndex, PositionEncoding};
use m1_core::{Kind, Node};
use tower_lsp::lsp_types::TextEdit;

/// An identifier that refers to the local named `name` (declaration or reference).
/// Public so the extract/inline-local refactors (#174) can collect a local's
/// occurrences with the same member-property / type-annotation exclusions.
pub fn is_local_ref(n: Node, name: &str) -> bool {
    n.kind() == Kind::Identifier
        && n.text() == name
        && !is_member_property(n)
        && !in_type_annotation(n)
}

/// The renameable local identifier under `byte`, if any. Public so the
/// inline-local refactor (#174) can confirm the cursor lands on a local.
pub fn local_ident_at(root: Node, byte: usize) -> Option<Node> {
    let node = node_at_byte(root, byte)?;
    if node.kind() != Kind::Identifier || is_member_property(node) || in_type_annotation(node) {
        return None;
    }
    if collect_locals(root).contains_key(node.text()) {
        Some(node)
    } else {
        None
    }
}

/// Edits that rename the local declared/referenced at `byte` to `new_name`
/// across the whole document — the same set `textDocument/rename` produces.
/// Returns `None` when `byte` isn't on a local, or `new_name` isn't a valid
/// identifier. Public so code actions (e.g. the L016 naming quick-fix) can reuse
/// the rename machinery instead of re-deriving the reference set (#162).
pub fn local_rename_edits(
    root: Node,
    byte: usize,
    new_name: &str,
    li: &LineIndex,
    enc: PositionEncoding,
) -> Option<Vec<TextEdit>> {
    let ident = local_ident_at(root, byte)?;
    if !is_valid_identifier(new_name) {
        return None;
    }
    let mut out = Vec::new();
    collect_local_edits(root, ident.text(), new_name, li, enc, &mut out);
    Some(out)
}

pub(super) fn collect_local_edits(
    root: Node,
    name: &str,
    new_name: &str,
    li: &LineIndex,
    enc: PositionEncoding,
    out: &mut Vec<TextEdit>,
) {
    // Iterative pre-order traversal (m1-core's `descendants`) rather than
    // recursion, so a pathologically deep tree can't overflow the stack (#133).
    for n in root.descendants() {
        if is_local_ref(n, name) {
            out.push(TextEdit {
                range: to_range(&n.byte_range(), li, enc),
                new_text: new_name.to_string(),
            });
        }
    }
}
