//! L002 — trailing-whitespace

use crate::diagnostic::{LintCode, LintDiagnostic};
use crate::rules::Rule;
use m1_core::{Range, Severity};

/// L002 — flags lines ending in space or tab characters.
pub struct TrailingWhitespace;

impl Rule for TrailingWhitespace {
    fn code(&self) -> LintCode {
        LintCode::L002
    }
    fn name(&self) -> &'static str {
        "trailing-whitespace"
    }

    fn check_file(&self, _source: &str, lines: &[&str], diags: &mut Vec<LintDiagnostic>) {
        let mut byte_offset = 0usize;

        for (line_idx, line) in lines.iter().enumerate() {
            let trimmed_len = line.trim_end().len();
            if trimmed_len < line.len() {
                let start = m1_core::Position {
                    line: line_idx as u32,
                    column: trimmed_len as u32,
                };
                let end = m1_core::Position {
                    line: line_idx as u32,
                    column: line.len() as u32,
                };
                let range = Range { start, end };
                let byte_start = byte_offset + trimmed_len;
                let byte_end = byte_offset + line.len();

                diags.push(LintDiagnostic::new(
                    LintCode::L002,
                    range,
                    byte_start..byte_end,
                    Severity::Warning,
                    "trailing whitespace".to_string(),
                ));
            }
            byte_offset += line.len() + 1;
        }
    }

    fn fix_file(&self, _source: &str, lines: &[&str], edits: &mut Vec<crate::fix::Edit>) {
        let mut byte_offset = 0usize;
        for line in lines {
            let trimmed = line.trim_end().len();
            if trimmed < line.len() {
                edits.push(crate::fix::Edit {
                    byte_range: (byte_offset + trimmed)..(byte_offset + line.len()),
                    replacement: String::new(),
                });
            }
            byte_offset += line.len() + 1;
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
        r.register(Box::new(TrailingWhitespace));
        Runner::new(r)
    }

    #[test]
    fn no_diagnostic_clean_lines() {
        let result = runner().run_source("x = 1;\ny = 2;\n");
        assert!(result.diagnostics.is_empty());
    }

    #[test]
    fn detects_trailing_space() {
        let result = runner().run_source("x = 1;   \n");
        assert_eq!(result.diagnostics.len(), 1);
        assert_eq!(result.diagnostics[0].code, LintCode::L002);
        assert_eq!(result.diagnostics[0].inner.range.start.column, 6);
    }

    #[test]
    fn detects_trailing_tab() {
        let result = runner().run_source("x = 1;\t\n");
        assert_eq!(result.diagnostics.len(), 1);
    }

    #[test]
    fn fixes_trailing_whitespace() {
        let mut r = Registry::empty();
        r.register(Box::new(TrailingWhitespace));
        let fixer = crate::fix::Fixer::new(&r);
        let out = fixer.fix_source("x = 1;  \n").unwrap();
        assert_eq!(out.as_deref(), Some("x = 1;\n"));
    }
}
