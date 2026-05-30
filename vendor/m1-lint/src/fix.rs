//! Autofix: collect mechanical edits, apply them, and verify the result is
//! syntactically valid and semantically equivalent (mirrors m1-fmt's guarantee).

use m1_core::{Cst, Kind, Node};

use crate::registry::Registry;

/// A single text replacement over a byte range of the source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Edit {
    pub byte_range: std::ops::Range<usize>,
    pub replacement: String,
}

/// Why a fix was abandoned.
#[derive(Debug)]
pub enum FixError {
    /// The fixed buffer introduced new syntax errors.
    NewSyntaxErrors,
    /// The fixed buffer changed the semantic token sequence beyond the
    /// sanctioned operator substitutions.
    TokensChanged,
}

impl std::fmt::Display for FixError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FixError::NewSyntaxErrors => write!(f, "fix would introduce syntax errors"),
            FixError::TokensChanged => write!(f, "fix would change program semantics"),
        }
    }
}

/// Applies enabled rules' fixes to a source buffer.
pub struct Fixer<'a> {
    registry: &'a Registry,
}

impl<'a> Fixer<'a> {
    pub fn new(registry: &'a Registry) -> Self {
        Self { registry }
    }

    /// Returns `Ok(Some(fixed))` if any safe edit applied, `Ok(None)` if there
    /// was nothing to fix, or `Err` if the only available fixes are unsafe.
    pub fn fix_source(&self, source: &str) -> Result<Option<String>, FixError> {
        let before = m1_core::parse(source);
        let lines: Vec<&str> = source.split('\n').collect();

        let mut edits: Vec<Edit> = Vec::new();
        for rule in self.registry.rules() {
            rule.fix_file(source, &lines, &mut edits);
        }
        let root = before.root();
        collect_node_edits(self.registry, &root, source, &mut edits);

        if edits.is_empty() {
            return Ok(None);
        }

        let candidate = apply_edits(source, edits);
        let after = m1_core::parse(&candidate);

        if after.syntax_diagnostics().len() > before.syntax_diagnostics().len() {
            return Err(FixError::NewSyntaxErrors);
        }
        if !tokens_equivalent(&before, &after) {
            return Err(FixError::TokensChanged);
        }
        Ok(Some(candidate))
    }
}

fn collect_node_edits(reg: &Registry, node: &Node, source: &str, edits: &mut Vec<Edit>) {
    for rule in reg.rules() {
        rule.fix_node(node, source, edits);
    }
    for child in node.children() {
        collect_node_edits(reg, &child, source, edits);
    }
}

/// Apply edits right-to-left after dropping any that overlap an earlier one.
pub fn apply_edits(source: &str, mut edits: Vec<Edit>) -> String {
    edits.sort_by_key(|e| e.byte_range.start);
    let mut kept: Vec<Edit> = Vec::new();
    let mut last_end = 0usize;
    for e in edits {
        if e.byte_range.start >= last_end {
            last_end = e.byte_range.end;
            kept.push(e);
        }
        // else: overlapping edit skipped; a later --fix run handles it.
    }
    let mut out = source.to_string();
    for e in kept.into_iter().rev() {
        out.replace_range(e.byte_range, &e.replacement);
    }
    out
}

/// Sanctioned operator rewrites that `--fix` is allowed to make.
fn sanctioned(a: &str, b: &str) -> bool {
    matches!(
        (a, b),
        ("==", "eq") | ("!=", "neq") | ("&&", "and") | ("||", "or") | ("!", "not")
    )
}

/// Non-trivia leaf tokens as `(Kind, text)` in source order.
fn semantic_tokens(cst: &Cst) -> Vec<(Kind, String)> {
    let mut out = Vec::new();
    collect_tokens(&cst.root(), &mut out);
    out
}

fn collect_tokens(node: &Node, out: &mut Vec<(Kind, String)>) {
    let children = node.children();
    if children.is_empty() {
        match node.kind() {
            Kind::LineComment | Kind::BlockComment => {}
            k => out.push((k, node.text().to_string())),
        }
        return;
    }
    for c in children {
        collect_tokens(&c, out);
    }
}

fn tokens_equivalent(before: &Cst, after: &Cst) -> bool {
    let a = semantic_tokens(before);
    let b = semantic_tokens(after);
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b.iter()).all(|(x, y)| {
        (x.0 == y.0 && x.1 == y.1) || sanctioned(&x.1, &y.1)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn apply_edits_right_to_left() {
        let edits = vec![
            Edit { byte_range: 0..1, replacement: "X".into() },
            Edit { byte_range: 4..5, replacement: "Y".into() },
        ];
        assert_eq!(apply_edits("abcde", edits), "Xbcde".replacen("e", "Y", 1));
    }

    #[test]
    fn overlapping_edit_dropped() {
        let edits = vec![
            Edit { byte_range: 0..3, replacement: "XY".into() },
            Edit { byte_range: 2..4, replacement: "ZZ".into() },
        ];
        // Second overlaps the first; only the first applies.
        assert_eq!(apply_edits("abcd", edits), "XYd");
    }
}
