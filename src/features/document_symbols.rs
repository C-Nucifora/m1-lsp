//! textDocument/documentSymbol: a flat outline of locals + top-level targets.
use crate::convert::range;
use crate::line_index::{LineIndex, PositionEncoding};
use m1_core::{Kind, Node};
#[allow(deprecated)]
use tower_lsp::lsp_types::{DocumentSymbol, SymbolKind};

pub fn document_symbols(root: Node, li: &LineIndex, enc: PositionEncoding) -> Vec<DocumentSymbol> {
    let mut out = Vec::new();
    fn name_of(decl: Node) -> Option<Node> {
        decl.named_children()
            .into_iter()
            .find(|c| matches!(c.kind(), Kind::Identifier | Kind::MemberExpression))
    }
    fn walk(n: Node, li: &LineIndex, enc: PositionEncoding, out: &mut Vec<DocumentSymbol>) {
        match n.kind() {
            Kind::LocalDeclaration => {
                if let Some(name) = name_of(n) {
                    out.push(make(name.text(), SymbolKind::VARIABLE, n, name, li, enc));
                }
            }
            Kind::AssignmentStatement => {
                if let Some(name) = name_of(n) {
                    out.push(make(name.text(), SymbolKind::FIELD, n, name, li, enc));
                }
            }
            _ => {}
        }
        for c in n.children() {
            walk(c, li, enc, out);
        }
    }
    walk(root, li, enc, &mut out);
    out
}

#[allow(deprecated)]
fn make(
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
}
