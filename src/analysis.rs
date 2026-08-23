//! The analysis pass: union of m1-core syntax, m1-lint, and m1-typecheck diagnostics.
use crate::config::DiagFilter;
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
    // Iterate the tree with m1-core's explicit work-stack pre-order iterator
    // rather than recursion, so a pathologically deep document can't overflow the
    // call stack (#133). This pass runs on every open/change, so a deeply nested
    // script must not abort the server here either.
    for n in root.descendants() {
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
    }
    out
}

/// Source of lint diagnostics (v1).
pub trait LintProvider: Send + Sync {
    fn lint(&self, uri: &Url, src: &str, li: &LineIndex, enc: PositionEncoding) -> Vec<LspDiag>;

    /// Re-resolve lint configuration by discovering a `.m1lint.toml` from `root`
    /// (and the user-global fallback). Called on `initialize` and whenever a
    /// `.m1lint.toml` changes. Default: no-op (providers without config).
    fn reload_config(&self, _root: &std::path::Path) {}

    /// Apply a lint config resolved by the unified `m1-tools.toml` layer. Default:
    /// no-op (providers without config). Supersedes `reload_config`'s own
    /// discovery when the backend drives configuration centrally.
    fn set_lint_config(&self, _cfg: &m1_lint::config::Config) {}

    /// Apply every enabled auto-fixable rule to `src`, returning the fully-fixed
    /// source — or `None` when there is nothing to fix. Backs the editor
    /// "fix all auto-fixable lint issues" action (#158). Default: `None`.
    fn fix(&self, _uri: &Url, _src: &str) -> Option<String> {
        None
    }
}

/// A no-op lint provider (syntax diagnostics only).
pub struct NoLint;
impl LintProvider for NoLint {
    fn lint(
        &self,
        _uri: &Url,
        _src: &str,
        _li: &LineIndex,
        _enc: PositionEncoding,
    ) -> Vec<LspDiag> {
        Vec::new()
    }
}

/// Source of type diagnostics (v2). `uri` lets the provider derive the script
/// file name (for group-relative resolution) and consult the loaded project.
pub trait TypeProvider: Send + Sync {
    fn types(&self, uri: &Url, src: &str, li: &LineIndex, enc: PositionEncoding) -> Vec<LspDiag>;
    /// True iff a project is loaded; gates the L006/T002 de-dup.
    fn project_loaded(&self) -> bool;

    /// Apply the resolved diagnostics filter so **opt-in** type rules (e.g. T064)
    /// activate when the unified config's `[diagnostics] select` names them. Without
    /// this a `select = ["T064"]` yields a blank editor: the opt-in rule never runs
    /// (the default registry excludes it) *and* the post-filter drops every other
    /// code — so the one code the user asked for is the one thing missing. Mirrors
    /// [`LintProvider::set_lint_config`]; the backend calls it on every config
    /// (re)resolve. Default: no-op (providers without opt-in rules).
    fn set_type_config(&self, _diagnostics: &crate::config::DiagFilter) {}
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
    filter: &DiagFilter,
) -> Vec<LspDiag> {
    // Closed-file / test path: no warm tree available, so parse here.
    let cst = m1_core::parse(src);
    analyze_with_cst(&cst, uri, src, li, enc, lint, types, filter)
}

/// [`analyze`] but reusing an already-parsed CST for the syntax + unsupported-
/// c-token pass, instead of re-parsing `src` from scratch (#270). The open-buffer
/// diagnostics path holds the document's incrementally-maintained tree, so on
/// every keystroke it passes that warm tree here rather than paying a full
/// reparse for this pass. (The lint and typecheck backends still parse
/// internally — they live in separate crates and take `&str` — so this removes
/// one of the several per-keystroke parses, not all of them.)
#[allow(clippy::too_many_arguments)]
pub fn analyze_with_cst(
    cst: &m1_core::Cst,
    uri: &Url,
    src: &str,
    li: &LineIndex,
    enc: PositionEncoding,
    lint: &dyn LintProvider,
    types: &dyn TypeProvider,
    filter: &DiagFilter,
) -> Vec<LspDiag> {
    let mut out: Vec<LspDiag> = cst
        .syntax_diagnostics()
        .iter()
        .map(|d| convert::core_diagnostic(d, li, enc))
        .collect();
    out.extend(unsupported_c_tokens(cst.root(), li, enc));

    let mut lint_diags = lint.lint(uri, src, li, enc);
    // When a project is loaded, m1-typecheck's T002 supersedes m1-lint's L006
    // float-equality heuristic; drop L006 to avoid double-reporting.
    if types.project_loaded() {
        lint_diags.retain(|d| !is_l006(d));
    }
    out.extend(lint_diags);
    out.extend(types.types(uri, src, li, enc));

    // Unified cross-source filter (m1-tools.toml `[diagnostics]`): lint codes
    // have already been projected into m1-lint's registry, then overlaid by the
    // higher-precedence `.m1lint.toml`. Do not filter them a second time here or
    // the lower unified layer would incorrectly win over the tool-specific file.
    // Core/intrinsic/type diagnostics still use this shared post-filter.
    if !filter.is_empty() {
        out.retain(|d| match &d.code {
            Some(NumberOrString::String(c))
                if m1_lint::diagnostic::LintCode::from_code_str(c).is_some() =>
            {
                true
            }
            Some(NumberOrString::String(c)) => filter.allows(c),
            Some(NumberOrString::Number(n)) => filter.allows(&n.to_string()),
            None => true,
        });
    }
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
        let diags = analyze(
            &uri(),
            src,
            &li,
            PositionEncoding::Utf16,
            &NoLint,
            &NoTypes,
            &DiagFilter::default(),
        );
        assert!(diags.is_empty());
    }

    #[test]
    fn syntax_error_is_reported() {
        let src = "local <Integer> = 1;\n";
        let li = LineIndex::new(src);
        let diags = analyze(
            &uri(),
            src,
            &li,
            PositionEncoding::Utf16,
            &NoLint,
            &NoTypes,
            &DiagFilter::default(),
        );
        assert!(!diags.is_empty());
        assert!(diags.iter().all(|d| d.source.as_deref() == Some("m1-core")));
    }

    struct L006Only;
    impl LintProvider for L006Only {
        fn lint(
            &self,
            _uri: &Url,
            _s: &str,
            _li: &LineIndex,
            _e: PositionEncoding,
        ) -> Vec<LspDiag> {
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
            &DiagFilter::default(),
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
            &DiagFilter::default(),
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
        let diags = analyze(
            &uri(),
            src,
            &li,
            PositionEncoding::Utf16,
            &NoLint,
            &NoTypes,
            &DiagFilter::default(),
        );
        assert!(
            diags.iter().any(|d| d.code
                == Some(tower_lsp::lsp_types::NumberOrString::String(
                    "unsupported-c-token".into()
                ))),
            "expected an unsupported-c-token diagnostic for `==`"
        );
    }

    #[test]
    fn unified_filter_drops_ignored_codes_across_sources() {
        // The lint registry owns L-code filtering while the shared post-filter
        // owns intrinsic/type codes. Together they enforce one unified config.
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join("m1-tools.toml"),
            "[diagnostics]\nignore = [\"unsupported-c-token\", \"L004\"]\n",
        )
        .unwrap();
        let cfg = crate::config::M1Config::resolve(None, tmp.path());
        let lint = crate::lint_backend::M1Lint::new();
        lint.set_lint_config(&cfg.lint);

        let src = "x = 1.0 == y;\n";
        let li = LineIndex::new(src);
        let diags = analyze(
            &uri(),
            src,
            &li,
            PositionEncoding::Utf16,
            &lint,
            &NoTypes,
            &cfg.diagnostics,
        );
        assert!(
            !diags.iter().any(|d| matches!(&d.code,
                Some(NumberOrString::String(c)) if c == "unsupported-c-token" || c == "L004")),
            "ignored codes from any source must be dropped: {diags:?}"
        );
        assert!(diags.iter().any(is_l006), "non-ignored L006 must survive");
    }

    #[test]
    fn tool_specific_lint_filter_overrides_the_unified_filter() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join("m1-tools.toml"),
            "[diagnostics]\nignore = [\"L004\"]\n",
        )
        .unwrap();
        std::fs::write(tmp.path().join(".m1lint.toml"), "select = [\"L004\"]\n").unwrap();

        let cfg = crate::config::M1Config::resolve(None, tmp.path());
        let lint = crate::lint_backend::M1Lint::new();
        lint.set_lint_config(&cfg.lint);
        let src = "x = a == b;\n";
        let li = LineIndex::new(src);
        let diags = analyze(
            &uri(),
            src,
            &li,
            PositionEncoding::Utf16,
            &lint,
            &NoTypes,
            &cfg.diagnostics,
        );

        assert!(
            diags.iter().any(|d| matches!(
                &d.code,
                Some(NumberOrString::String(code)) if code == "L004"
            )),
            "the higher-precedence .m1lint.toml selection must re-enable L004: {diags:?}"
        );
    }
}
