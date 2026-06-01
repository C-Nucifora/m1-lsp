//! The analysis pass: union of m1-core syntax, m1-lint, and m1-typecheck diagnostics.
use crate::convert;
use crate::line_index::{LineIndex, PositionEncoding};
use tower_lsp::lsp_types::{Diagnostic as LspDiag, DiagnosticSeverity, NumberOrString, Url};

/// `unsupported-c-token`: flag C operators that M1 doesn't accept (`==`/`!=`/
/// `&&`/`||`/`!`), with the M1 replacement from the intrinsic language table.
fn unsupported_c_tokens(
    root: m1_core::Node,
    li: &LineIndex,
    enc: PositionEncoding,
) -> Vec<LspDiag> {
    let intr = m1_typecheck::intrinsics::get();
    let mut out = Vec::new();
    fn walk(
        n: m1_core::Node,
        intr: &'static m1_typecheck::intrinsics::Intrinsics,
        li: &LineIndex,
        enc: PositionEncoding,
        out: &mut Vec<LspDiag>,
    ) {
        if let Some(replacement) = intr.unsupported_c_token(n.kind_str()) {
            out.push(LspDiag {
                range: convert::range(&n.byte_range(), li, enc),
                severity: Some(DiagnosticSeverity::ERROR),
                code: Some(NumberOrString::String("unsupported-c-token".into())),
                source: Some("m1-intrinsics".into()),
                message: format!("`{}` is not valid in M1 — {replacement}", n.kind_str()),
                ..Default::default()
            });
        }
        for c in n.children() {
            walk(c, intr, li, enc, out);
        }
    }
    walk(root, intr, li, enc, &mut out);
    out
}

/// Source of lint diagnostics (v1).
pub trait LintProvider: Send + Sync {
    fn lint(&self, src: &str, li: &LineIndex, enc: PositionEncoding) -> Vec<LspDiag>;

    /// Re-resolve lint configuration by discovering a `.m1lint.toml` from `root`
    /// (and the user-global fallback). Called on `initialize` and whenever a
    /// `.m1lint.toml` changes. Default: no-op (providers without config).
    fn reload_config(&self, _root: &std::path::Path) {}
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
    out.extend(unsupported_c_tokens(cst.root(), li, enc));

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

    #[test]
    fn flags_unsupported_c_tokens() {
        let src = "x = a == b and c;\n"; // == is a C token; 'and' is fine
        let li = LineIndex::new(src);
        let diags = analyze(&uri(), src, &li, PositionEncoding::Utf16, &NoLint, &NoTypes);
        assert!(
            diags.iter().any(|d| d.code
                == Some(tower_lsp::lsp_types::NumberOrString::String(
                    "unsupported-c-token".into()
                ))),
            "expected an unsupported-c-token diagnostic for `==`"
        );
    }
}
