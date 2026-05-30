//! L011 — comment-style
//!
//! Flags line comments with no space after `//` (e.g. `//foo`).

use crate::diagnostic::{LintCode, LintDiagnostic};
use crate::rules::Rule;
use m1_core::{Kind, Node, Severity};

/// L011 — flags `//foo` (missing space after `//`).
pub struct CommentStyle;

/// True if `text` is a line comment needing a space after `//`.
fn needs_space(text: &str) -> bool {
    let bytes = text.as_bytes();
    if !text.starts_with("//") {
        return false;
    }
    match bytes.get(2) {
        None => false,                 // bare `//`
        Some(b'/') => false,           // `///`, separators like `////`
        Some(b' ') | Some(b'\t') => false,
        Some(_) => true,
    }
}

impl Rule for CommentStyle {
    fn code(&self) -> LintCode {
        LintCode::L011
    }
    fn name(&self) -> &'static str {
        "comment-style"
    }

    fn check_node(&self, node: &Node, _source: &str, diags: &mut Vec<LintDiagnostic>) {
        if node.kind() != Kind::LineComment || !needs_space(node.text()) {
            return;
        }
        diags.push(LintDiagnostic::new(
            LintCode::L011,
            node.range(),
            node.byte_range(),
            Severity::Warning,
            "add a space after `//`".to_string(),
        ));
    }

    fn fix_node(&self, node: &m1_core::Node, _source: &str, edits: &mut Vec<crate::fix::Edit>) {
        if node.kind() != Kind::LineComment || !needs_space(node.text()) {
            return;
        }
        // Insert one space just after the `//`.
        let at = node.byte_range().start + 2;
        edits.push(crate::fix::Edit { byte_range: at..at, replacement: " ".into() });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::Registry;
    use crate::runner::Runner;

    fn runner() -> Runner {
        let mut r = Registry::empty();
        r.register(Box::new(CommentStyle));
        Runner::new(r)
    }

    #[test]
    fn flags_missing_space() {
        let result = runner().run_source("//hello\nx = 1;\n");
        assert_eq!(result.diagnostics.len(), 1);
        assert_eq!(result.diagnostics[0].code, LintCode::L011);
    }

    #[test]
    fn no_diagnostic_with_space_or_separator() {
        let result = runner().run_source("// good\n//// sep\nx = 1;\n");
        assert!(result.diagnostics.is_empty());
    }

    #[test]
    fn fixes_missing_space() {
        let mut r = Registry::empty();
        r.register(Box::new(CommentStyle));
        let fixer = crate::fix::Fixer::new(&r);
        let out = fixer.fix_source("//hello\nx = 1;\n").unwrap();
        assert_eq!(out.as_deref(), Some("// hello\nx = 1;\n"));
    }
}
