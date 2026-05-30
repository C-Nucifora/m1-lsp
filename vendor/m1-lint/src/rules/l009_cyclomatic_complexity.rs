//! L009 — cyclomatic-complexity
//!
//! The cyclomatic complexity of a `when` block (or the top-level source file)
//! must not exceed 10.
//!
//! Complexity = 1 + count of decision points within the scope:
//! - each `if` / `else if` (`else if` is itself an `if_statement` node)
//! - each `is` clause and `expand` statement
//! - each `and` / `&&` / `or` / `||` operator
//!
//! Nested `when` blocks are scopes in their own right and are not counted
//! toward their enclosing scope's complexity.

use crate::diagnostic::{LintCode, LintDiagnostic};
use crate::rules::Rule;
use m1_core::{Kind, Node, Severity};

fn is_scope_node(kind: Kind) -> bool {
    matches!(kind, Kind::WhenStatement | Kind::SourceFile)
}

fn is_decision_point(kind: Kind) -> bool {
    matches!(
        kind,
        Kind::IfStatement
            | Kind::IsClause
            | Kind::ExpandStatement
            | Kind::And
            | Kind::Or
            | Kind::AmpAmp
            | Kind::PipePipe
    )
}

fn count_complexity(scope: &Node) -> u32 {
    let mut count = 1u32; // base complexity
    count_children(scope, &mut count);
    count
}

/// Count decision points among `node`'s descendants, without descending into
/// nested scopes (which get their own complexity check).
fn count_children(node: &Node, count: &mut u32) {
    for child in node.children() {
        if is_decision_point(child.kind()) {
            *count += 1;
        }
        if is_scope_node(child.kind()) {
            // A nested scope is checked independently; do not descend.
            continue;
        }
        count_children(&child, count);
    }
}

/// L009 — flags scopes whose cyclomatic complexity exceeds `max_complexity`.
pub struct CyclomaticComplexity {
    pub max_complexity: u32,
}

impl Default for CyclomaticComplexity {
    fn default() -> Self {
        Self { max_complexity: 10 }
    }
}

impl Rule for CyclomaticComplexity {
    fn code(&self) -> LintCode {
        LintCode::L009
    }
    fn name(&self) -> &'static str {
        "cyclomatic-complexity"
    }

    fn check_node(&self, node: &Node, _source: &str, diags: &mut Vec<LintDiagnostic>) {
        if !is_scope_node(node.kind()) {
            return;
        }
        let complexity = count_complexity(node);
        if complexity > self.max_complexity {
            diags.push(LintDiagnostic::new(
                LintCode::L009,
                node.range(),
                node.byte_range(),
                Severity::Warning,
                format!(
                    "cyclomatic complexity {} exceeds maximum of {}",
                    complexity, self.max_complexity
                ),
            ));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::Registry;
    use crate::runner::Runner;

    fn runner() -> Runner {
        let mut r = Registry::empty();
        r.register(Box::new(CyclomaticComplexity::default()));
        Runner::new(r)
    }

    #[test]
    fn simple_script_low_complexity() {
        let source = "x = 1;\ny = 2;\n";
        let result = runner().run_source(source);
        assert!(result.diagnostics.iter().all(|d| d.code != LintCode::L009));
    }

    #[test]
    fn flags_high_complexity_top_level() {
        // 11 `if` statements -> complexity 12 > 10.
        let mut source = String::new();
        for _ in 0..11 {
            source.push_str("if (a) { x = 1; }\n");
        }
        let result = runner().run_source(&source);
        assert!(result.diagnostics.iter().any(|d| d.code == LintCode::L009));
    }

    #[test]
    fn logical_operators_count() {
        // 10 `and` operators -> complexity 11 > 10.
        let mut cond = String::from("a");
        for _ in 0..10 {
            cond.push_str(" and a");
        }
        let source = format!("if ({}) {{ x = 1; }}\n", cond);
        let result = runner().run_source(&source);
        assert!(result.diagnostics.iter().any(|d| d.code == LintCode::L009));
    }
}
