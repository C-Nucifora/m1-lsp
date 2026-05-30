//! L010 — tab-for-indentation
//!
//! Flags lines whose leading whitespace contains a tab. Spaces only.

use crate::diagnostic::{LintCode, LintDiagnostic};
use crate::rules::Rule;
use m1_core::{Range, Severity};

/// L010 — flags tabs in leading indentation.
pub struct TabIndentation;

impl Rule for TabIndentation {
    fn code(&self) -> LintCode {
        LintCode::L010
    }
    fn name(&self) -> &'static str {
        "tab-for-indentation"
    }

    fn check_file(&self, _source: &str, lines: &[&str], diags: &mut Vec<LintDiagnostic>) {
        let mut byte_offset = 0usize;
        for (line_idx, line) in lines.iter().enumerate() {
            let indent_len = line.len() - line.trim_start().len();
            let indent = &line[..indent_len];
            if indent.contains('\t') {
                let start = m1_core::Position {
                    line: line_idx as u32,
                    column: 0,
                };
                let end = m1_core::Position {
                    line: line_idx as u32,
                    column: indent_len as u32,
                };
                diags.push(LintDiagnostic::new(
                    LintCode::L010,
                    Range { start, end },
                    byte_offset..(byte_offset + indent_len),
                    Severity::Warning,
                    "tab character in indentation; use spaces".to_string(),
                ));
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
        r.register(Box::new(TabIndentation));
        Runner::new(r)
    }

    #[test]
    fn flags_leading_tab() {
        let result = runner().run_source("\tx = 1;\n");
        assert_eq!(result.diagnostics.len(), 1);
        assert_eq!(result.diagnostics[0].code, LintCode::L010);
    }

    #[test]
    fn no_diagnostic_on_space_indent() {
        let result = runner().run_source("    x = 1;\n");
        assert!(result.diagnostics.is_empty());
    }
}
