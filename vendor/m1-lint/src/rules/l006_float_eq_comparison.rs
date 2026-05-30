//! L006 — float-eq-comparison
//!
//! Flags equality comparisons (`==`, `!=`, `eq`, `neq`) where at least one
//! immediate operand is a float literal. This is a CST-only heuristic; it does
//! not flag float *variables* (those require type information).

use crate::diagnostic::{LintCode, LintDiagnostic};
use crate::rules::Rule;
use m1_core::{Kind, Node, Severity};

/// L006 — flags equality comparisons against float literals.
pub struct FloatEqComparison;

/// Returns true if the node is a float literal.
///
/// Float notation is detected by the presence of a `.` or an exponent marker
/// (`e`/`E`). Hexadecimal literals (`0x..`) contain neither and so are treated
/// as integers, which is the desired behaviour.
fn is_float_literal(node: &Node) -> bool {
    if node.kind() != Kind::Number {
        return false;
    }
    let text = node.text();
    text.contains('.') || text.to_ascii_lowercase().contains('e')
}

/// Returns true if the node is an equality operator token.
///
/// Both the symbolic (`==`, `!=`) and the keyword (`eq`, `neq`) forms are
/// equality comparisons.
fn is_eq_op(kind: Kind) -> bool {
    matches!(kind, Kind::EqEq | Kind::BangEq | Kind::Eq | Kind::Neq)
}

impl Rule for FloatEqComparison {
    fn code(&self) -> LintCode {
        LintCode::L006
    }
    fn name(&self) -> &'static str {
        "float-eq-comparison"
    }

    fn check_node(&self, node: &Node, _source: &str, diags: &mut Vec<LintDiagnostic>) {
        if node.kind() != Kind::BinaryExpression {
            return;
        }
        let children = node.children();
        let has_eq_op = children.iter().any(|c| is_eq_op(c.kind()));
        if !has_eq_op {
            return;
        }
        let has_float = children.iter().any(is_float_literal);
        if has_float {
            diags.push(LintDiagnostic::new(
                LintCode::L006,
                node.range(),
                node.byte_range(),
                Severity::Error,
                "never compare floats with equality operators; use a tolerance check",
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
        r.register(Box::new(FloatEqComparison));
        Runner::new(r)
    }

    #[test]
    fn flags_float_eq_eq() {
        let source = "x = a == 1.0;\n";
        let result = runner().run_source(source);
        assert!(result.diagnostics.iter().any(|d| d.code == LintCode::L006));
    }

    #[test]
    fn flags_float_bang_eq() {
        let source = "x = a != 0.5;\n";
        let result = runner().run_source(source);
        assert!(result.diagnostics.iter().any(|d| d.code == LintCode::L006));
    }

    #[test]
    fn flags_float_with_eq_keyword() {
        let source = "x = a eq 1.0;\n";
        let result = runner().run_source(source);
        assert!(result.diagnostics.iter().any(|d| d.code == LintCode::L006));
    }

    #[test]
    fn no_false_positive_int_eq() {
        let source = "x = a == 1;\n";
        let result = runner().run_source(source);
        assert!(result.diagnostics.iter().all(|d| d.code != LintCode::L006));
    }

    #[test]
    fn no_false_positive_eq_idents() {
        let source = "x = a eq b;\n";
        let result = runner().run_source(source);
        assert!(result.diagnostics.iter().all(|d| d.code != LintCode::L006));
    }
}
