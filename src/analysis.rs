//! The analysis pass: union of m1-core syntax, m1-lint, and m1-typecheck diagnostics.
use crate::convert;
use crate::line_index::{LineIndex, PositionEncoding};
use tower_lsp::lsp_types::{Diagnostic as LspDiag, NumberOrString, Url};

/// Source of lint diagnostics (v1).
pub trait LintProvider: Send + Sync {
    fn lint(&self, src: &str, li: &LineIndex, enc: PositionEncoding) -> Vec<LspDiag>;
}

/// A no-op lint provider (syntax diagnostics only).
pub struct NoLint;
impl LintProvider for NoLint {
    fn lint(&self, _src: &str, _li: &LineIndex, _enc: PositionEncoding) -> Vec<LspDiag> {
        Vec::new()
    }
}

/// Source of type diagnostics (v2). `uri` lets the provider derive the script
/// file name (for group-relative resolution) and consult the loaded project.
pub trait TypeProvider: Send + Sync {
    fn types(&self, uri: &Url, src: &str, li: &LineIndex, enc: PositionEncoding) -> Vec<LspDiag>;
    /// True iff a project is loaded; gates the L006/T002 de-dup.
    fn project_loaded(&self) -> bool;
}

/// A no-op type provider. Default until m1-typecheck is injected / when disabled.
pub struct NoTypes;
impl TypeProvider for NoTypes {
    fn types(&self, _u: &Url, _s: &str, _li: &LineIndex, _e: PositionEncoding) -> Vec<LspDiag> {
        Vec::new()
    }
    fn project_loaded(&self) -> bool {
        false
    }
}

fn is_l006(d: &LspDiag) -> bool {
    matches!(&d.code, Some(NumberOrString::String(s)) if s == "L006")
}

pub fn analyze(
    uri: &Url,
    src: &str,
    li: &LineIndex,
    enc: PositionEncoding,
    lint: &dyn LintProvider,
    types: &dyn TypeProvider,
) -> Vec<LspDiag> {
    let cst = m1_core::parse(src);
    let mut out: Vec<LspDiag> = cst
        .syntax_diagnostics()
        .iter()
        .map(|d| convert::core_diagnostic(d, li, enc))
        .collect();

    let mut lint_diags = lint.lint(src, li, enc);
    // When a project is loaded, m1-typecheck's T002 supersedes m1-lint's L006
    // float-equality heuristic; drop L006 to avoid double-reporting.
    if types.project_loaded() {
        lint_diags.retain(|d| !is_l006(d));
    }
    out.extend(lint_diags);
    out.extend(types.types(uri, src, li, enc));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn uri() -> Url {
        Url::parse("file:///x.m1scr").unwrap()
    }

    #[test]
    fn clean_source_has_no_diagnostics() {
        let src = "local x = 1;\n";
        let li = LineIndex::new(src);
        let diags = analyze(&uri(), src, &li, PositionEncoding::Utf16, &NoLint, &NoTypes);
        assert!(diags.is_empty());
    }

    #[test]
    fn syntax_error_is_reported() {
        let src = "local <Integer> = 1;\n";
        let li = LineIndex::new(src);
        let diags = analyze(&uri(), src, &li, PositionEncoding::Utf16, &NoLint, &NoTypes);
        assert!(!diags.is_empty());
        assert!(diags.iter().all(|d| d.source.as_deref() == Some("m1-core")));
    }

    struct L006Only;
    impl LintProvider for L006Only {
        fn lint(&self, _s: &str, _li: &LineIndex, _e: PositionEncoding) -> Vec<LspDiag> {
            vec![
                LspDiag {
                    code: Some(NumberOrString::String("L006".into())),
                    message: "float eq".into(),
                    ..Default::default()
                },
                LspDiag {
                    code: Some(NumberOrString::String("L004".into())),
                    message: "use eq".into(),
                    ..Default::default()
                },
            ]
        }
    }

    struct ProjLoaded;
    impl TypeProvider for ProjLoaded {
        fn types(&self, _u: &Url, _s: &str, _li: &LineIndex, _e: PositionEncoding) -> Vec<LspDiag> {
            vec![LspDiag {
                code: Some(NumberOrString::String("T002".into())),
                source: Some("m1-typecheck".into()),
                message: "float eq (typed)".into(),
                ..Default::default()
            }]
        }
        fn project_loaded(&self) -> bool {
            true
        }
    }

    #[test]
    fn l006_suppressed_when_project_loaded() {
        let src = "x = 1.0 == y;\n";
        let li = LineIndex::new(src);
        let diags = analyze(
            &uri(),
            src,
            &li,
            PositionEncoding::Utf16,
            &L006Only,
            &ProjLoaded,
        );
        assert!(!diags.iter().any(is_l006), "L006 must be dropped");
        assert!(
            diags.iter().any(|d| d.message == "use eq"),
            "L004 must survive"
        );
        assert!(
            diags
                .iter()
                .any(|d| d.source.as_deref() == Some("m1-typecheck"))
        );
    }

    #[test]
    fn l006_kept_when_no_project() {
        let src = "x = 1.0 == y;\n";
        let li = LineIndex::new(src);
        let diags = analyze(
            &uri(),
            src,
            &li,
            PositionEncoding::Utf16,
            &L006Only,
            &NoTypes,
        );
        assert!(
            diags.iter().any(is_l006),
            "L006 must survive without a project"
        );
    }
}
