//! L003 — missing-final-newline

use crate::diagnostic::{LintCode, LintDiagnostic};
use crate::rules::Rule;
use m1_core::{Range, Severity};

/// L003 — flags files that do not end with a newline.
pub struct MissingFinalNewline;

impl Rule for MissingFinalNewline {
    fn code(&self) -> LintCode {
        LintCode::L003
    }
    fn name(&self) -> &'static str {
        "missing-final-newline"
    }

    fn check_file(&self, source: &str, _lines: &[&str], diags: &mut Vec<LintDiagnostic>) {
        if source.is_empty() || source.ends_with('\n') {
            return;
        }
        let len = source.len();
        let line_count = source.lines().count() as u32;
        let last_line_len = source.lines().last().map(|l| l.len()).unwrap_or(0) as u32;
        let pos = m1_core::Position {
            line: line_count.saturating_sub(1),
            column: last_line_len,
        };
        let range = Range {
            start: pos,
            end: pos,
        };

        diags.push(LintDiagnostic::new(
            LintCode::L003,
            range,
            (len - 1)..len,
            Severity::Warning,
            "file does not end with a newline".to_string(),
        ));
    }

    fn fix_file(&self, source: &str, _lines: &[&str], edits: &mut Vec<crate::fix::Edit>) {
        if !source.is_empty() && !source.ends_with('\n') {
            let len = source.len();
            edits.push(crate::fix::Edit { byte_range: len..len, replacement: "\n".into() });
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
        r.register(Box::new(MissingFinalNewline));
        Runner::new(r)
    }

    #[test]
    fn no_diagnostic_with_final_newline() {
        let result = runner().run_source("x = 1;\n");
        assert!(result.diagnostics.is_empty());
    }

    #[test]
    fn diagnostic_without_final_newline() {
        let result = runner().run_source("x = 1;");
        assert_eq!(result.diagnostics.len(), 1);
        assert_eq!(result.diagnostics[0].code, LintCode::L003);
    }

    #[test]
    fn no_diagnostic_on_empty_file() {
        let result = runner().run_source("");
        assert!(result.diagnostics.is_empty());
    }

    #[test]
    fn fixes_missing_newline() {
        let mut r = Registry::empty();
        r.register(Box::new(MissingFinalNewline));
        let fixer = crate::fix::Fixer::new(&r);
        let out = fixer.fix_source("x = 1;").unwrap();
        assert_eq!(out.as_deref(), Some("x = 1;\n"));
    }
}
