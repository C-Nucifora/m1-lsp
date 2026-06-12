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

/// The callee path node of a call-expression statement — the `Output.SetState`
/// of `Output.SetState(1);` — when `stmt`'s direct child is a `CallExpression`.
/// Restricted to a direct child so a nested call inside a larger expression
/// (`a + Foo(x);`) doesn't mislabel the statement.
fn call_callee(stmt: Node) -> Option<Node> {
    let call = stmt
        .named_children()
        .into_iter()
        .find(|c| c.kind() == Kind::CallExpression)?;
    call.child_by_field(Field::Function)
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
                        rhs_detail(child),
                        li,
                        enc,
                    ));
                }
            }
            Kind::AssignmentStatement => {
                if let Some(name) = name_of(child) {
                    out.push(leaf(
                        name.text(),
                        SymbolKind::FIELD,
                        child,
                        name,
                        rhs_detail(child),
                        li,
                        enc,
                    ));
                }
            }
            // A bare call statement (`Output.SetState(1);`, `Timer.Start();`) is
            // the actual behaviour of many actuator/fault scripts; surface it as a
            // leaf labelled by the callee path so those scripts get an outline
            // instead of a blank one (#152). A non-call expression statement just
            // descends.
            Kind::ExpressionStatement => match call_callee(child) {
                Some(callee) => out.push(leaf(
                    callee.text(),
                    SymbolKind::METHOD,
                    child,
                    callee,
                    None,
                    li,
                    enc,
                )),
                None => out.extend(collect(child, li, enc)),
            },
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
            // #269: an `expand (VAR = start to end)` compile-time loop is a
            // nesting construct (L009 counts it). Surface it as a container, like
            // if/when, labelled by its loop variable, so its repeated body folds
            // in the outline instead of flattening into the parent scope.
            Kind::ExpandStatement => {
                let kids = collect(child, li, enc);
                if !kids.is_empty() {
                    let label = header_label("expand", child.child_by_field(Field::Variable));
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

/// The assignment operator and right-hand side, as outline detail (`= 1`,
/// `+= a + b`) — whitespace-collapsed and truncated. Disambiguates several writes
/// to the same target, which otherwise share a label (#156).
///
/// Both the operator and the value come from the CST fields rather than a textual
/// `find('=')`: a compound assignment (`+=`, `>>=`, …) is an accumulation/mutation,
/// not a plain `=`, so it must keep its real operator; and the RHS may itself
/// contain `=` (a `==` comparison), which the old first-`=` scan truncated. A
/// `local` declaration has no operator field — it is always `=`.
fn rhs_detail(node: Node) -> Option<String> {
    let value = node.child_by_field(Field::Value)?;
    let op = node
        .child_by_field(Field::Operator)
        .map(|o| o.text())
        .unwrap_or("=");
    let rhs = value
        .text()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    if rhs.is_empty() {
        return None;
    }
    let rhs = if rhs.chars().count() > 40 {
        format!("{}…", rhs.chars().take(40).collect::<String>())
    } else {
        rhs
    };
    Some(format!("{op} {rhs}"))
}

#[allow(deprecated)]
fn leaf(
    name: &str,
    kind: SymbolKind,
    full: Node,
    sel: Node,
    detail: Option<String>,
    li: &LineIndex,
    enc: PositionEncoding,
) -> DocumentSymbol {
    DocumentSymbol {
        name: name.to_string(),
        detail,
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
    fn assignment_leaves_carry_distinguishing_detail() {
        // #156: two writes to the same channel share a label, so without detail
        // they're indistinguishable in the outline. The RHS disambiguates them.
        let src = "Out = 1;\nOut = 2;\n";
        let cst = m1_core::parse(src);
        let li = LineIndex::new(src);
        let syms = document_symbols(cst.root(), &li, PositionEncoding::Utf16);
        let outs: Vec<_> = syms.iter().filter(|s| s.name == "Out").collect();
        assert_eq!(outs.len(), 2, "two assignments to Out: {syms:?}");
        assert!(outs[0].detail.is_some(), "leaves should carry detail");
        assert_ne!(
            outs[0].detail, outs[1].detail,
            "the two writes should be distinguishable: {outs:?}"
        );
        assert!(outs[0].detail.as_deref().unwrap().contains('1'));
    }

    #[test]
    #[allow(deprecated)]
    fn compound_assignment_detail_keeps_its_operator() {
        // A compound assignment (`+=`, `>>=`, …) is an accumulation/mutation, not a
        // plain `=`. The outline detail must preserve the actual operator, both so
        // the reader sees what the statement does and so two compound writes to the
        // same target stay distinguishable (#156). The real corpus uses these:
        // e.g. `Energy Used += DCCurrent*DCVoltage * 0.05 * -1;`.
        let src = "Out += 1;\nOut -= 1;\n";
        let cst = m1_core::parse(src);
        let li = LineIndex::new(src);
        let syms = document_symbols(cst.root(), &li, PositionEncoding::Utf16);
        let outs: Vec<_> = syms.iter().filter(|s| s.name == "Out").collect();
        assert_eq!(outs.len(), 2, "two compound writes to Out: {syms:?}");
        assert_eq!(
            outs[0].detail.as_deref(),
            Some("+= 1"),
            "first write should show its `+=` operator, not a bare `=`"
        );
        assert_eq!(
            outs[1].detail.as_deref(),
            Some("-= 1"),
            "second write should show its `-=` operator"
        );
        // …and therefore the two are distinguishable, which a bare `= 1` for both
        // would defeat.
        assert_ne!(outs[0].detail, outs[1].detail);
    }

    #[test]
    #[allow(deprecated)]
    fn detail_handles_rhs_containing_an_equals() {
        // The RHS itself may contain `=` (a comparison). Detail must take the whole
        // RHS expression, not stop at the first `=`.
        let src = "Out = a == b;\n";
        let cst = m1_core::parse(src);
        let li = LineIndex::new(src);
        let syms = document_symbols(cst.root(), &li, PositionEncoding::Utf16);
        let out = syms.iter().find(|s| s.name == "Out").unwrap();
        assert_eq!(out.detail.as_deref(), Some("= a == b"));
    }

    #[test]
    fn lists_call_statements_as_leaves() {
        // A script whose body is only side-effecting calls (actuator/fault
        // scripts) must not produce a blank outline (#152).
        let src = "Output.SetState(1);\nTimer.Start();\n";
        let cst = m1_core::parse(src);
        let li = LineIndex::new(src);
        let syms = document_symbols(cst.root(), &li, PositionEncoding::Utf16);
        let names: Vec<_> = syms.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"Output.SetState"), "got {names:?}");
        assert!(names.contains(&"Timer.Start"), "got {names:?}");
    }

    #[test]
    #[allow(deprecated)]
    fn surfaces_if_block_whose_only_children_are_calls() {
        let src = "if (ready) {\nOutput.SetState(1);\n}\n";
        let cst = m1_core::parse(src);
        let li = LineIndex::new(src);
        let syms = document_symbols(cst.root(), &li, PositionEncoding::Utf16);
        assert_eq!(
            syms.len(),
            1,
            "an if block whose only child is a call should still appear: {syms:?}"
        );
        assert!(syms[0].name.starts_with("if"));
        assert!(
            syms[0]
                .children
                .as_ref()
                .unwrap()
                .iter()
                .any(|k| k.name == "Output.SetState")
        );
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
    fn nests_symbols_under_expand_block() {
        // #269: an `expand` compile-time loop is a container in the outline, like
        // if/when — its body symbols nest under it instead of flattening into the
        // parent scope, and the expand itself is visible.
        let src = "expand (i = 0 to 3) {\nOut = 1;\n}\n";
        let cst = m1_core::parse(src);
        let li = LineIndex::new(src);
        let syms = document_symbols(cst.root(), &li, PositionEncoding::Utf16);
        assert_eq!(
            syms.len(),
            1,
            "expected one top-level expand container: {syms:?}"
        );
        assert!(
            syms[0].name.starts_with("expand"),
            "label should name the expand: {}",
            syms[0].name
        );
        let kids = syms[0].children.as_ref().expect("expand has children");
        assert!(kids.iter().any(|k| k.name == "Out"), "got {kids:?}");
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
