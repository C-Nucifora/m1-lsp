//! textDocument/rename + prepareRename, restricted to `local` variables.
//!
//! Only locals are renameable: channels, parameters and other project symbols are
//! declared in `Project.m1prj`, not the script, so renaming them here would be
//! unsound. Locals are file-scoped in the type model (`collect_locals` is flat),
//! so a rename rewrites every matching identifier in the file — excluding
//! member-access properties (`Foo.count`) and type-annotation names (`<Count>`).
use crate::convert::range as to_range;
use crate::features::locate::{collect_locals, node_at_byte};
use crate::line_index::{LineIndex, PositionEncoding};
use m1_core::{Field, Kind, Node};
use std::collections::HashMap;
use tower_lsp::lsp_types::{PrepareRenameResponse, TextEdit, Url, WorkspaceEdit};

/// True when `n` is the `property` half of a `member_expression` (the part after
/// the `.`), which is a channel/field access — never a local.
fn is_member_property(n: Node) -> bool {
    n.parent()
        .filter(|p| p.kind() == Kind::MemberExpression)
        .and_then(|p| p.child_by_field(Field::Property))
        .map(|prop| prop.byte_range() == n.byte_range())
        .unwrap_or(false)
}

fn in_type_annotation(n: Node) -> bool {
    let mut cur = n;
    while let Some(p) = cur.parent() {
        if p.kind() == Kind::TypeAnnotation {
            return true;
        }
        cur = p;
    }
    false
}

/// An identifier that refers to the local named `name` (declaration or reference).
fn is_local_ref(n: Node, name: &str) -> bool {
    n.kind() == Kind::Identifier
        && n.text() == name
        && !is_member_property(n)
        && !in_type_annotation(n)
}

/// The renameable local identifier under `byte`, if any.
fn local_ident_at(root: Node, byte: usize) -> Option<Node> {
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

/// A new name must be a bare identifier: a leading letter/underscore, then
/// letters/digits/underscores. (M1 local names never contain spaces.)
pub fn is_valid_identifier(name: &str) -> bool {
    let mut chars = name.chars();
    matches!(chars.next(), Some(c) if c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

pub fn prepare_rename(
    root: Node,
    byte: usize,
    li: &LineIndex,
    enc: PositionEncoding,
) -> Option<PrepareRenameResponse> {
    let node = local_ident_at(root, byte)?;
    Some(PrepareRenameResponse::Range(to_range(
        &node.byte_range(),
        li,
        enc,
    )))
}

pub fn rename(
    root: Node,
    byte: usize,
    new_name: &str,
    uri: Url,
    li: &LineIndex,
    enc: PositionEncoding,
) -> Option<WorkspaceEdit> {
    let name = local_ident_at(root, byte)?.text().to_string();
    let mut edits = Vec::new();
    collect_refs(root, &name, new_name, li, enc, &mut edits);
    if edits.is_empty() {
        return None;
    }
    let mut changes = HashMap::new();
    changes.insert(uri, edits);
    Some(WorkspaceEdit {
        changes: Some(changes),
        document_changes: None,
        change_annotations: None,
    })
}

fn collect_refs(
    n: Node,
    name: &str,
    new_name: &str,
    li: &LineIndex,
    enc: PositionEncoding,
    out: &mut Vec<TextEdit>,
) {
    if is_local_ref(n, name) {
        out.push(TextEdit {
            range: to_range(&n.byte_range(), li, enc),
            new_text: new_name.to_string(),
        });
    }
    for c in n.children() {
        collect_refs(c, name, new_name, li, enc, out);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn url() -> Url {
        Url::parse("file:///t.m1scr").unwrap()
    }
    fn edits_for(src: &str, at: &str, new: &str) -> Vec<TextEdit> {
        let cst = m1_core::parse(src);
        let li = LineIndex::new(src);
        let byte = src.find(at).unwrap();
        rename(cst.root(), byte, new, url(), &li, PositionEncoding::Utf16)
            .map(|e| e.changes.unwrap().into_values().next().unwrap())
            .unwrap_or_default()
    }

    #[test]
    fn renames_all_local_occurrences() {
        let edits = edits_for("local count = 0;\ncount = count + 1;\n", "count", "total");
        assert_eq!(edits.len(), 3, "declaration + two references");
        assert!(edits.iter().all(|e| e.new_text == "total"));
    }

    #[test]
    fn does_not_touch_same_named_member_property() {
        // `Foo.count` is a field access, not the local — must be left alone.
        let edits = edits_for(
            "local count = 0;\nFoo.count = 1;\ncount = count + 1;\n",
            "count",
            "total",
        );
        assert_eq!(edits.len(), 3);
    }

    #[test]
    fn rejects_non_local() {
        let cst = m1_core::parse("Output.Value = 1;\n");
        let li = LineIndex::new("Output.Value = 1;\n");
        let byte = "Output.Value = 1;\n".find("Output").unwrap();
        assert!(rename(cst.root(), byte, "x", url(), &li, PositionEncoding::Utf16).is_none());
    }

    #[test]
    fn prepare_allows_local_rejects_channel_and_property() {
        let src = "local count = 0;\nOutput.Value = count;\n";
        let cst = m1_core::parse(src);
        let li = LineIndex::new(src);
        let enc = PositionEncoding::Utf16;
        let p = |needle: &str| {
            prepare_rename(cst.root(), src.find(needle).unwrap(), &li, enc).is_some()
        };
        assert!(p("count"), "local is renameable");
        assert!(!p("Output"), "channel is not renameable");
        assert!(!p("Value"), "member property is not renameable");
    }

    #[test]
    fn validates_identifier() {
        assert!(is_valid_identifier("total"));
        assert!(is_valid_identifier("_x9"));
        assert!(!is_valid_identifier(""));
        assert!(!is_valid_identifier("9bad"));
        assert!(!is_valid_identifier("has space"));
    }
}
