//! L007 — operator-spacing
//!
//! Binary and assignment operator tokens must be surrounded by a space on each
//! side. Checked operators: `+`, `-`, `*`, `/`, `%`, the assignment forms
//! `=`/`+=`/`-=`/`*=`/`/=`, and the relational operators `<`, `>`, `<=`, `>=`.

use crate::diagnostic::{LintCode, LintDiagnostic};
use crate::rules::Rule;
use m1_core::{Kind, Node, Severity};

/// L007 — flags operators not surrounded by spaces.
pub struct OperatorSpacing;

fn is_checked_operator(kind: Kind) -> bool {
    matches!(
        kind,
        Kind::Plus
            | Kind::Minus
            | Kind::Star
            | Kind::Slash
            | Kind::Percent
            | Kind::Assign // assignment =
            | Kind::PlusEq
            | Kind::MinusEq
            | Kind::StarEq
            | Kind::SlashEq
            | Kind::Lt
            | Kind::Gt
            | Kind::LtEq
            | Kind::GtEq
    )
}

fn has_space_before(source: &[u8], byte_start: usize) -> bool {
    byte_start > 0 && source[byte_start - 1] == b' '
}

fn has_space_after(source: &[u8], byte_end: usize) -> bool {
    byte_end < source.len() && source[byte_end] == b' '
}

impl Rule for OperatorSpacing {
    fn code(&self) -> LintCode {
        LintCode::L007
    }
    fn name(&self) -> &'static str {
        "operator-spacing"
    }

    fn check_node(&self, node: &Node, source: &str, diags: &mut Vec<LintDiagnostic>) {
        // Operators appear as direct children of binary expressions and
        // assignment statements.
        if !matches!(
            node.kind(),
            Kind::BinaryExpression | Kind::AssignmentStatement
        ) {
            return;
        }
        let source_bytes = source.as_bytes();
        for child in node.children() {
            if !is_checked_operator(child.kind()) {
                continue;
            }
            let br = child.byte_range();
            let missing_before = !has_space_before(source_bytes, br.start);
            let missing_after = !has_space_after(source_bytes, br.end);
            if missing_before || missing_after {
                diags.push(LintDiagnostic::new(
                    LintCode::L007,
                    child.range(),
                    br,
                    Severity::Warning,
                    format!("missing space around operator `{}`", child.text()),
                ));
            }
        }
    }

    fn fix_node(&self, node: &m1_core::Node, source: &str, edits: &mut Vec<crate::fix::Edit>) {
        if !matches!(node.kind(), Kind::BinaryExpression | Kind::AssignmentStatement) {
            return;
        }
        let bytes = source.as_bytes();
        for child in node.children() {
            if !is_checked_operator(child.kind()) {
                continue;
            }
            let br = child.byte_range();
            if !has_space_before(bytes, br.start) {
                edits.push(crate::fix::Edit { byte_range: br.start..br.start, replacement: " ".into() });
            }
            if !has_space_after(bytes, br.end) {
                edits.push(crate::fix::Edit { byte_range: br.end..br.end, replacement: " ".into() });
            }
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
        r.register(Box::new(OperatorSpacing));
        Runner::new(r)
    }

    #[test]
    fn no_diagnostic_on_spaced_operators() {
        let source = "x = a + b;\n";
        let result = runner().run_source(source);
        assert!(result.diagnostics.iter().all(|d| d.code != LintCode::L007));
    }

    #[test]
    fn flags_missing_space_before_plus() {
        let source = "x = a+ b;\n";
        let result = runner().run_source(source);
        assert!(result.diagnostics.iter().any(|d| d.code == LintCode::L007));
    }

    #[test]
    fn flags_missing_space_after_plus() {
        let source = "x = a +b;\n";
        let result = runner().run_source(source);
        assert!(result.diagnostics.iter().any(|d| d.code == LintCode::L007));
    }

    #[test]
    fn flags_no_space_around_plus() {
        let source = "x = a+b;\n";
        let result = runner().run_source(source);
        assert!(result.diagnostics.iter().any(|d| d.code == LintCode::L007));
    }

    #[test]
    fn flags_missing_space_around_assignment() {
        let source = "x=1;\n";
        let result = runner().run_source(source);
        assert!(result.diagnostics.iter().any(|d| d.code == LintCode::L007));
    }

    #[test]
    fn fixes_missing_spacing() {
        let mut r = Registry::empty();
        r.register(Box::new(OperatorSpacing));
        let fixer = crate::fix::Fixer::new(&r);
        let out = fixer.fix_source("x = a+b;\n").unwrap();
        assert_eq!(out.as_deref(), Some("x = a + b;\n"));
    }
}
