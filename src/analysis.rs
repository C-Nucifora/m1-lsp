//! The analysis pass: union of m1-core syntax diagnostics and lint diagnostics.
use crate::convert;
use crate::line_index::{LineIndex, PositionEncoding};
use tower_lsp::lsp_types::Diagnostic as LspDiag;

/// Source of lint diagnostics. Abstracted so the server is testable without
/// m1-lint, and so the real m1-lint backend can be swapped in (Task 7).
pub trait LintProvider: Send + Sync {
    fn lint(&self, src: &str, li: &LineIndex, enc: PositionEncoding) -> Vec<LspDiag>;
}

/// A no-op lint provider (syntax diagnostics only). Default until m1-lint lands.
pub struct NoLint;
impl LintProvider for NoLint {
    fn lint(&self, _src: &str, _li: &LineIndex, _enc: PositionEncoding) -> Vec<LspDiag> {
        Vec::new()
    }
}

pub fn analyze(
    src: &str,
    li: &LineIndex,
    enc: PositionEncoding,
    lint: &dyn LintProvider,
) -> Vec<LspDiag> {
    let cst = m1_core::parse(src);
    let mut out: Vec<LspDiag> = cst
        .syntax_diagnostics()
        .iter()
        .map(|d| convert::core_diagnostic(d, li, enc))
        .collect();
    out.extend(lint.lint(src, li, enc));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clean_source_has_no_diagnostics() {
        let src = "local x = 1;\n";
        let li = LineIndex::new(src);
        let diags = analyze(src, &li, PositionEncoding::Utf16, &NoLint);
        assert!(diags.is_empty());
    }

    #[test]
    fn syntax_error_is_reported() {
        let src = "local <Integer> = 1;\n";
        let li = LineIndex::new(src);
        let diags = analyze(src, &li, PositionEncoding::Utf16, &NoLint);
        assert!(!diags.is_empty());
        assert!(diags.iter().all(|d| d.source.as_deref() == Some("m1-core")));
    }

    #[test]
    fn lint_provider_contributes() {
        struct FakeLint;
        impl LintProvider for FakeLint {
            fn lint(&self, _s: &str, _li: &LineIndex, _e: PositionEncoding) -> Vec<LspDiag> {
                vec![LspDiag {
                    message: "fake".into(),
                    ..Default::default()
                }]
            }
        }
        let src = "local x = 1;\n";
        let li = LineIndex::new(src);
        let diags = analyze(src, &li, PositionEncoding::Utf16, &FakeLint);
        assert_eq!(diags.len(), 1);
        assert_eq!(diags[0].message, "fake");
    }
}
