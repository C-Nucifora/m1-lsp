//! L001 — line-too-long
//!
//! A source line, after stripping trailing whitespace, must not exceed
//! `max_len` characters (default 88).

use crate::diagnostic::{LintCode, LintDiagnostic};
use crate::rules::Rule;
use m1_core::{Range, Severity};

/// L001 — flags lines longer than `max_len` (after rstrip).
pub struct LineTooLong {
    pub max_len: usize,
}

impl Default for LineTooLong {
    fn default() -> Self {
        Self { max_len: 88 }
    }
}

impl Rule for LineTooLong {
    fn code(&self) -> LintCode {
        LintCode::L001
    }
    fn name(&self) -> &'static str {
        "line-too-long"
    }

    fn check_file(&self, _source: &str, lines: &[&str], diags: &mut Vec<LintDiagnostic>) {
        let mut byte_offset = 0usize;

        for (line_idx, line) in lines.iter().enumerate() {
            let trimmed_len = line.trim_end().len();

            if trimmed_len > self.max_len {
                let start = m1_core::Position {
                    line: line_idx as u32,
                    column: self.max_len as u32,
                };
                let end = m1_core::Position {
                    line: line_idx as u32,
                    column: trimmed_len as u32,
                };
                let range = Range { start, end };
                let byte_start = byte_offset + self.max_len;
                let byte_end = byte_offset + trimmed_len;

                diags.push(LintDiagnostic::new(
                    LintCode::L001,
                    range,
                    byte_start..byte_end,
                    Severity::Warning,
                    format!("line is {} characters (max {})", trimmed_len, self.max_len),
                ));
            }

            // Advance past this line + the '\n' separator.
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
        r.register(Box::new(LineTooLong::default()));
        Runner::new(r)
    }

    #[test]
    fn no_diagnostic_on_short_lines() {
        let source = "x = 1;\ny = 2;\n";
        let result = runner().run_source(source);
        assert!(result.diagnostics.is_empty());
    }

    #[test]
    fn no_diagnostic_exactly_88_chars() {
        let line = "x".repeat(88);
        let source = format!("{}\n", line);
        let result = runner().run_source(&source);
        assert!(result.diagnostics.is_empty());
    }

    #[test]
    fn diagnostic_on_89_chars() {
        let line = "x".repeat(89);
        let source = format!("{}\n", line);
        let result = runner().run_source(&source);
        assert_eq!(result.diagnostics.len(), 1);
        assert_eq!(result.diagnostics[0].code, LintCode::L001);
        assert_eq!(result.diagnostics[0].inner.range.start.line, 0);
    }

    #[test]
    fn trailing_whitespace_excluded_from_length() {
        // 89 chars but last char is a space — rstrip gives 88, no diagnostic.
        let line = format!("{} ", "x".repeat(88));
        let source = format!("{}\n", line);
        let result = runner().run_source(&source);
        assert!(
            result.diagnostics.is_empty(),
            "rstrip should bring length to 88"
        );
    }

    #[test]
    fn multiple_long_lines_each_reported() {
        let long = "x".repeat(90);
        let source = format!("{}\n{}\n", long, long);
        let result = runner().run_source(&source);
        assert_eq!(result.diagnostics.len(), 2);
        assert_eq!(result.diagnostics[0].inner.range.start.line, 0);
        assert_eq!(result.diagnostics[1].inner.range.start.line, 1);
    }

    #[test]
    fn respects_custom_max_len() {
        let mut r = Registry::empty();
        r.register(Box::new(LineTooLong { max_len: 10 }));
        let result = Runner::new(r).run_source("xxxxxxxxxxxx = 1;\n"); // 16 chars
        assert_eq!(result.diagnostics.len(), 1);
        assert!(result.diagnostics[0].inner.message.contains("max 10"));
    }
}
