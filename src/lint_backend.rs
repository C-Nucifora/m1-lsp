//! Real lint provider backed by m1-lint.
use crate::analysis::LintProvider;
use crate::convert::{range, severity};
use crate::line_index::{LineIndex, PositionEncoding};
use m1_lint::registry::Registry;
use m1_lint::runner::Runner;
use tower_lsp::lsp_types::{Diagnostic as LspDiag, NumberOrString};

pub struct M1Lint {
    runner: Runner,
}

impl M1Lint {
    pub fn new() -> Self {
        Self {
            runner: Runner::new(Registry::default_v1()),
        }
    }
}

impl Default for M1Lint {
    fn default() -> Self {
        Self::new()
    }
}

impl LintProvider for M1Lint {
    fn lint(&self, src: &str, li: &LineIndex, enc: PositionEncoding) -> Vec<LspDiag> {
        // Use only lint findings; syntax errors come from m1-core in analyze().
        self.runner
            .run_source(src)
            .diagnostics
            .iter()
            .map(|d| LspDiag {
                range: range(&d.inner.byte_range, li, enc),
                severity: Some(severity(d.inner.severity)),
                code: Some(NumberOrString::String(d.code.to_string())),
                source: Some("m1-lint".to_string()),
                message: d.inner.message.clone(),
                ..Default::default()
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flags_eq_eq() {
        // L004: `==` should be `eq`. Adjust the snippet if the lint snapshot differs.
        let src = "if (a == b) {\n    x = 1;\n}\n";
        let li = LineIndex::new(src);
        let diags = M1Lint::new().lint(src, &li, PositionEncoding::Utf16);
        assert!(diags.iter().any(|d| d.source.as_deref() == Some("m1-lint")));
    }
}
