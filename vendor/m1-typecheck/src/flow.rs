//! Control-flow analysis: single-assignment-per-channel (T040).
//!
//! For each statement subtree we compute, per channel path, the maximum number
//! of assignments on any single execution path (`max_per_path`). Sequential
//! statements add; mutually-exclusive `if`/`else` arms take the per-key max. A
//! channel whose top-level `max_per_path` reaches 2 is assigned twice on one path.
use crate::diagnostics::{make, TypeCode, TypeDiagnostic};
use crate::resolve::{resolve, Resolution, Scope};
use crate::symbols::SymbolKind;
use crate::typer::path_text;
use m1_core::{Kind, Node};
use std::collections::HashMap;

#[derive(Default)]
struct Facts {
    /// channel path -> max assignments on a single path through this subtree.
    max_per_path: HashMap<String, u32>,
}

/// Entry point: run over the whole script, append T040 diagnostics.
pub fn single_assignment(root: Node, scope: &Scope, out: &mut Vec<TypeDiagnostic>) {
    let facts = seq_children(root, scope);
    for (path, n) in &facts.max_per_path {
        if *n >= 2 {
            // Re-walk to find the 2nd assignment node for a precise span.
            if let Some(node) = nth_assignment(root, scope, path, 2) {
                out.push(make(
                    TypeCode::T040,
                    &node,
                    m1_core::Severity::Warning,
                    format!("channel `{path}` is assigned more than once on a single code path"),
                ));
            }
        }
    }
}

/// Compose the facts of a node's *statement* children in sequence.
fn seq_children(node: Node, scope: &Scope) -> Facts {
    let mut acc = Facts::default();
    for child in node.children() {
        let f = facts_of(child, scope);
        acc = merge_seq(acc, f);
    }
    acc
}

/// Facts for a single statement node.
fn facts_of(node: Node, scope: &Scope) -> Facts {
    match node.kind() {
        Kind::AssignmentStatement => {
            let mut f = Facts::default();
            if let Some(path) = assigned_channel(node, scope) {
                f.max_per_path.insert(path, 1);
            }
            f
        }
        Kind::IfStatement => {
            // then-block facts vs else-block facts -> per-key max (mutual exclusion).
            let blocks: Vec<_> = node
                .children()
                .into_iter()
                .filter(|c| c.kind() == Kind::Block || c.kind() == Kind::ElseClause)
                .collect();
            let then_f = blocks
                .first()
                .map(|b| seq_children(*b, scope))
                .unwrap_or_default();
            let else_f = blocks
                .iter()
                .find(|b| b.kind() == Kind::ElseClause)
                .map(|b| seq_children(*b, scope))
                .unwrap_or_default();
            merge_alt(then_f, else_f)
        }
        Kind::WhenStatement | Kind::ExpandStatement => {
            // Treated as a no-else conditional body (see spec §5.2).
            let body = node
                .children()
                .into_iter()
                .find(|c| c.kind() == Kind::Block);
            let body_f = body.map(|b| seq_children(b, scope)).unwrap_or_default();
            merge_alt(body_f, Facts::default())
        }
        Kind::Block | Kind::ElseClause => seq_children(node, scope),
        _ => Facts::default(),
    }
}

fn merge_seq(mut a: Facts, b: Facts) -> Facts {
    for (k, v) in b.max_per_path {
        let entry = a.max_per_path.entry(k).or_insert(0);
        *entry += v;
    }
    a
}

fn merge_alt(a: Facts, b: Facts) -> Facts {
    let mut out = Facts::default();
    for (k, v) in a.max_per_path.iter().chain(b.max_per_path.iter()) {
        let e = out.max_per_path.entry(k.clone()).or_insert(0);
        *e = (*e).max(*v);
    }
    out
}

/// If `node` is `target = …` to a Channel symbol, return its canonical path.
fn assigned_channel(node: Node, scope: &Scope) -> Option<String> {
    if !node.children().iter().any(|c| c.kind() == Kind::Assign) {
        return None; // skip += etc. (the "set once" rule targets plain assignment)
    }
    let target = node
        .named_children()
        .into_iter()
        .find(|c| is_expr(c.kind()))?;
    match resolve(&path_text(target), scope) {
        Resolution::Symbol(s) if s.kind == SymbolKind::Channel => Some(s.path.clone()),
        _ => None,
    }
}

/// Find the n-th (1-based) assignment to `path` in document order, for the span.
fn nth_assignment<'a>(root: Node<'a>, scope: &Scope, path: &str, n: u32) -> Option<Node<'a>> {
    fn walk<'a>(
        node: Node<'a>,
        scope: &Scope,
        path: &str,
        n: u32,
        count: &mut u32,
    ) -> Option<Node<'a>> {
        if node.kind() == Kind::AssignmentStatement
            && assigned_channel(node, scope).as_deref() == Some(path)
        {
            *count += 1;
            if *count == n {
                return Some(node);
            }
        }
        for c in node.children() {
            if let Some(found) = walk(c, scope, path, n, count) {
                return Some(found);
            }
        }
        None
    }
    let mut count = 0;
    walk(root, scope, path, n, &mut count)
}

fn is_expr(k: Kind) -> bool {
    matches!(
        k,
        Kind::Identifier
            | Kind::MemberExpression
            | Kind::CallExpression
            | Kind::UnaryExpression
            | Kind::BinaryExpression
            | Kind::TernaryExpression
            | Kind::ParenthesizedExpression
            | Kind::Number
            | Kind::Boolean
            | Kind::String
    )
}
