//! textDocument/references + textDocument/documentHighlight.
//!
//! Both answer the same question — "where else in this file is the thing under
//! the cursor?" — and share one occurrence finder. We work within a single file
//! only: locals are file-scoped in the type model, and project symbols are
//! declared in `Project.m1prj` (not the scripts), so cross-file reference search
//! would need a project-wide index we don't maintain here.
use crate::convert::range;
use crate::features::locate::{collect_locals, node_at_byte, path_at_byte};
use crate::line_index::{LineIndex, PositionEncoding};
use m1_core::{Field, Kind, Node};
use tower_lsp::lsp_types::{DocumentHighlight, DocumentHighlightKind, Location, Url};

/// True when `n` is the `property` half of a `member_expression` (after the `.`).
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

/// The outermost path node (`identifier` / `member_expression`) at `n`: climb out
/// of any enclosing member expressions, matching `path_at_byte`.
fn top_path_node(n: Node) -> Node {
    let mut top = n;
    while let Some(p) = top.parent() {
        if p.kind() == Kind::MemberExpression {
            top = p;
        } else {
            break;
        }
    }
    top
}

/// Every identifier that refers to the local named `name` (declaration or use),
/// excluding member-access properties and type-annotation names.
fn collect_local_idents<'a>(n: Node<'a>, name: &str, out: &mut Vec<Node<'a>>) {
    if n.kind() == Kind::Identifier
        && n.text() == name
        && !is_member_property(n)
        && !in_type_annotation(n)
    {
        out.push(n);
    }
    for c in n.children() {
        collect_local_idents(c, name, out);
    }
}

/// Every top-level path node whose dotted text equals `path`.
fn collect_path_matches<'a>(n: Node<'a>, path: &str, out: &mut Vec<Node<'a>>) {
    let is_path = matches!(n.kind(), Kind::Identifier | Kind::MemberExpression);
    let is_top = n
        .parent()
        .map(|p| p.kind() != Kind::MemberExpression)
        .unwrap_or(true);
    if is_path && is_top && !in_type_annotation(n) && n.text() == path {
        out.push(n);
    }
    for c in n.children() {
        collect_path_matches(c, path, out);
    }
}

/// Nodes in `root` that refer to the same entity as the cursor at `byte`.
fn occurrences<'a>(root: Node<'a>, byte: usize) -> Vec<Node<'a>> {
    let Some(node) = node_at_byte(root, byte) else {
        return Vec::new();
    };
    // A bare identifier that names a known local: precise, name-based match.
    if node.kind() == Kind::Identifier
        && !is_member_property(node)
        && !in_type_annotation(node)
        && collect_locals(root).contains_key(node.text())
    {
        let mut out = Vec::new();
        collect_local_idents(root, node.text(), &mut out);
        return out;
    }
    // Otherwise match the full dotted path (channel / project symbol / library
    // member) by text.
    let Some((_, path)) = path_at_byte(root, byte) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    collect_path_matches(root, &path, &mut out);
    out
}

/// True when `n` (or the path it tops) is being written: the target of an
/// assignment or the name of a `local` declaration.
fn is_write(n: Node) -> bool {
    let top = top_path_node(n);
    match top.parent() {
        Some(p) if p.kind() == Kind::AssignmentStatement => p
            .child_by_field(Field::Target)
            .map(|t| t.byte_range() == top.byte_range())
            .unwrap_or(false),
        Some(p) if p.kind() == Kind::LocalDeclaration => p
            .child_by_field(Field::Name)
            .map(|name| name.byte_range() == n.byte_range())
            .unwrap_or(false),
        _ => false,
    }
}

pub fn references(
    root: Node,
    byte: usize,
    uri: Url,
    li: &LineIndex,
    enc: PositionEncoding,
) -> Option<Vec<Location>> {
    let nodes = occurrences(root, byte);
    if nodes.is_empty() {
        return None;
    }
    Some(
        nodes
            .into_iter()
            .map(|n| Location {
                uri: uri.clone(),
                range: range(&n.byte_range(), li, enc),
            })
            .collect(),
    )
}

pub fn document_highlights(
    root: Node,
    byte: usize,
    li: &LineIndex,
    enc: PositionEncoding,
) -> Option<Vec<DocumentHighlight>> {
    let nodes = occurrences(root, byte);
    if nodes.is_empty() {
        return None;
    }
    Some(
        nodes
            .into_iter()
            .map(|n| DocumentHighlight {
                range: range(&n.byte_range(), li, enc),
                kind: Some(if is_write(n) {
                    DocumentHighlightKind::WRITE
                } else {
                    DocumentHighlightKind::READ
                }),
            })
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn url() -> Url {
        Url::parse("file:///t.m1scr").unwrap()
    }

    fn refs(src: &str, at: &str) -> Vec<Location> {
        let cst = m1_core::parse(src);
        let li = LineIndex::new(src);
        let byte = src.find(at).unwrap();
        references(cst.root(), byte, url(), &li, PositionEncoding::Utf16).unwrap_or_default()
    }

    #[test]
    fn finds_all_local_occurrences() {
        let locs = refs("local count = 0;\ncount = count + 1;\n", "count");
        assert_eq!(locs.len(), 3, "declaration + two uses");
    }

    #[test]
    fn local_search_ignores_same_named_member_property() {
        // `Foo.count` is a field access, not the local.
        let locs = refs(
            "local count = 0;\nFoo.count = 1;\ncount = count + 1;\n",
            "count",
        );
        assert_eq!(locs.len(), 3);
    }

    #[test]
    fn finds_channel_path_occurrences() {
        // Not a local -> match by full dotted path. Two writes to the same channel.
        let locs = refs("Output.Value = 1;\nOutput.Value = 2;\n", "Output");
        assert_eq!(locs.len(), 2);
    }

    #[test]
    fn no_references_on_whitespace() {
        let cst = m1_core::parse("x = 1;\n");
        let li = LineIndex::new("x = 1;\n");
        let byte = "x = 1;\n".find("= 1").unwrap() + 1; // the space
        assert!(references(cst.root(), byte, url(), &li, PositionEncoding::Utf16).is_none());
    }

    #[test]
    fn highlights_classify_write_vs_read() {
        let src = "local count = 0;\ncount = count + 1;\n";
        let cst = m1_core::parse(src);
        let li = LineIndex::new(src);
        let byte = src.find("count").unwrap();
        let hl = document_highlights(cst.root(), byte, &li, PositionEncoding::Utf16).unwrap();
        let writes = hl
            .iter()
            .filter(|h| h.kind == Some(DocumentHighlightKind::WRITE))
            .count();
        let reads = hl
            .iter()
            .filter(|h| h.kind == Some(DocumentHighlightKind::READ))
            .count();
        assert_eq!(
            writes, 2,
            "the decl and the `count =` assignment are writes"
        );
        assert_eq!(reads, 1, "the `count + 1` use is a read");
    }
}
