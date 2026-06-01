//! textDocument/documentSymbol: a nested outline — locals and assignment
//! targets, with `when`/`if` blocks as containing nodes (#32).
use crate::convert::range;
use crate::line_index::{LineIndex, PositionEncoding};
use m1_core::{Field, Kind, Node};
#[allow(deprecated)]
use tower_lsp::lsp_types::{DocumentSymbol, SymbolKind};

pub fn document_symbols(root: Node, li: &LineIndex, enc: PositionEncoding) -> Vec<DocumentSymbol> {
    collect(root, li, enc)
}

fn name_of(decl: Node) -> Option<Node> {
    decl.named_children()
        .into_iter()
        .find(|c| matches!(c.kind(), Kind::Identifier | Kind::MemberExpression))
}

/// A short header label for a block construct, e.g. `when (driveMode)` or
/// `if (ready)`, whitespace-collapsed and truncated so the outline stays
/// readable.
fn header_label(keyword: &str, header: Option<Node>) -> String {
    match header {
        Some(h) => {
            let text = h.text().split_whitespace().collect::<Vec<_>>().join(" ");
            let text = if text.chars().count() > 40 {
                format!("{}…", text.chars().take(40).collect::<String>())
            } else {
                text
            };
            format!("{keyword} ({text})")
        }
        None => keyword.to_string(),
    }
}

/// Build the symbols for the statements within `n`'s subtree, nesting `when`/
/// `if` blocks. Leaf statements (local decls, assignments) become symbols;
/// block constructs become containers holding the symbols found inside them.
fn collect(n: Node, li: &LineIndex, enc: PositionEncoding) -> Vec<DocumentSymbol> {
    let mut out = Vec::new();
    for child in n.children() {
        match child.kind() {
            Kind::LocalDeclaration => {
                if let Some(name) = name_of(child) {
                    out.push(leaf(
                        name.text(),
                        SymbolKind::VARIABLE,
                        child,
                        name,
                        li,
                        enc,
                    ));
                }
            }
            Kind::AssignmentStatement => {
                if let Some(name) = name_of(child) {
                    out.push(leaf(name.text(), SymbolKind::FIELD, child, name, li, enc));
                }
            }
            Kind::IfStatement => {
                let kids = collect(child, li, enc);
                if !kids.is_empty() {
                    let label = header_label("if", child.child_by_field(Field::Condition));
                    out.push(container(label, child, kids, li, enc));
                }
            }
            Kind::WhenStatement => {
                let kids = collect(child, li, enc);
                if !kids.is_empty() {
                    let label = header_label("when", child.child_by_field(Field::Subject));
                    out.push(container(label, child, kids, li, enc));
                }
            }
            // Descend through blocks, is/else clauses, and anything else so the
            // symbols inside them surface under the nearest block container.
            _ => out.extend(collect(child, li, enc)),
        }
    }
    out
}

#[allow(deprecated)]
fn leaf(
    name: &str,
    kind: SymbolKind,
    full: Node,
    sel: Node,
    li: &LineIndex,
    enc: PositionEncoding,
) -> DocumentSymbol {
    DocumentSymbol {
        name: name.to_string(),
        detail: None,
        kind,
        tags: None,
        deprecated: None,
        range: range(&full.byte_range(), li, enc),
        selection_range: range(&sel.byte_range(), li, enc),
        children: None,
    }
}

#[allow(deprecated)]
fn container(
    name: String,
    node: Node,
    children: Vec<DocumentSymbol>,
    li: &LineIndex,
    enc: PositionEncoding,
) -> DocumentSymbol {
    // selection_range must be within range; use the keyword token (first child)
    // when present, else the whole node.
    let sel = node.children().first().copied().unwrap_or(node);
    DocumentSymbol {
        name,
        detail: None,
        kind: SymbolKind::NAMESPACE,
        tags: None,
        deprecated: None,
        range: range(&node.byte_range(), li, enc),
        selection_range: range(&sel.byte_range(), li, enc),
        children: Some(children),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lists_locals_and_assignments() {
        let src = "local fGain = 1.0;\nRatio = 2;\n";
        let cst = m1_core::parse(src);
        let li = LineIndex::new(src);
        let syms = document_symbols(cst.root(), &li, PositionEncoding::Utf16);
        let names: Vec<_> = syms.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"fGain"));
        assert!(names.contains(&"Ratio"));
    }

    #[test]
    #[allow(deprecated)]
    fn nests_symbols_under_when_block() {
        let src = "when (driveMode) {\nis (true) {\nOut = 1;\n}\n}\n";
        let cst = m1_core::parse(src);
        let li = LineIndex::new(src);
        let syms = document_symbols(cst.root(), &li, PositionEncoding::Utf16);
        // Single top-level `when` container holding the inner assignment.
        assert_eq!(
            syms.len(),
            1,
            "expected one top-level when container: {syms:?}"
        );
        assert!(syms[0].name.starts_with("when"), "label: {}", syms[0].name);
        let kids = syms[0].children.as_ref().expect("when has children");
        assert!(kids.iter().any(|k| k.name == "Out"));
    }

    #[test]
    #[allow(deprecated)]
    fn nests_symbols_under_if_block() {
        let src = "if (ready) {\nOut = 1;\n}\n";
        let cst = m1_core::parse(src);
        let li = LineIndex::new(src);
        let syms = document_symbols(cst.root(), &li, PositionEncoding::Utf16);
        assert_eq!(syms.len(), 1);
        assert!(syms[0].name.starts_with("if"));
        assert!(
            syms[0]
                .children
                .as_ref()
                .unwrap()
                .iter()
                .any(|k| k.name == "Out")
        );
    }
}
