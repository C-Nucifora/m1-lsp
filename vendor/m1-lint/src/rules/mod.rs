//! Lint rules.
//!
//! Each rule is a zero-sized struct implementing [`Rule`].

use crate::diagnostic::LintDiagnostic;

pub mod l001_line_too_long;
pub mod l002_trailing_whitespace;
pub mod l003_missing_final_newline;
pub mod l004_eq_operator_preferred;
pub mod l005_logical_operator_preferred;
pub mod l006_float_eq_comparison;
pub mod l007_operator_spacing;
pub mod l008_nesting_too_deep;
pub mod l009_cyclomatic_complexity;
pub mod l010_tab_indentation;
pub mod l011_comment_style;

/// A lint rule.
///
/// Rules implement one or both of [`check_file`][Rule::check_file] and
/// [`check_node`][Rule::check_node]. The default implementations are no-ops so
/// each rule only needs to override what it uses.
pub trait Rule: Send + Sync {
    /// The machine-readable code for this rule, e.g. `LintCode::L001`.
    fn code(&self) -> crate::diagnostic::LintCode;

    /// A short human-readable name, e.g. `"line-too-long"`.
    fn name(&self) -> &'static str;

    /// Called once per file before the CST walk.
    ///
    /// `source` is the raw file contents. `lines` is the source split on `\n`;
    /// each element has the trailing newline stripped.
    fn check_file(&self, source: &str, lines: &[&str], diags: &mut Vec<LintDiagnostic>) {
        let _ = (source, lines, diags);
    }

    /// Called for every node in the CST (depth-first, pre-order).
    fn check_node(&self, node: &m1_core::Node, source: &str, diags: &mut Vec<LintDiagnostic>) {
        let _ = (node, source, diags);
    }

    /// Emit autofix edits for this node. Default: no fix.
    fn fix_node(&self, node: &m1_core::Node, source: &str, edits: &mut Vec<crate::fix::Edit>) {
        let _ = (node, source, edits);
    }

    /// Emit autofix edits at file scope. Default: no fix.
    fn fix_file(&self, source: &str, lines: &[&str], edits: &mut Vec<crate::fix::Edit>) {
        let _ = (source, lines, edits);
    }
}
