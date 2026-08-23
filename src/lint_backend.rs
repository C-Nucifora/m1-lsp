//! Real lint provider backed by m1-lint.
use std::path::{Path, PathBuf};
use std::sync::RwLock;

use crate::analysis::LintProvider;
use crate::convert::{LINT_DOCS_BASE, code_description, range, severity};
use crate::line_index::{LineIndex, PositionEncoding};
use m1_lint::config::Config;
use m1_lint::registry::Registry;
use m1_lint::runner::Runner;
use tower_lsp::lsp_types::{Diagnostic as LspDiag, NumberOrString, Url};

struct LintState {
    runner: Runner,
    matcher: m1_lint::config::ExcludeMatcher,
    root: Option<PathBuf>,
}

impl LintState {
    fn from_config(cfg: &Config, root: Option<&Path>) -> Self {
        Self {
            runner: Runner::new(Registry::from_config(cfg)),
            matcher: m1_lint::config::ExcludeMatcher::new(cfg),
            root: root.map(Path::to_path_buf),
        }
    }

    fn is_excluded(&self, uri: &Url) -> bool {
        let Ok(path) = uri.to_file_path() else {
            return false;
        };
        self.matcher.is_excluded(&path)
            || self
                .root
                .as_deref()
                .and_then(|root| path.strip_prefix(root).ok())
                .is_some_and(|relative| self.matcher.is_excluded(relative))
    }
}

pub struct M1Lint {
    /// One lock keeps the runner and its path-exclusion matcher on the same
    /// configuration generation during hot reloads.
    state: RwLock<LintState>,
}

impl M1Lint {
    pub fn new() -> Self {
        // Seed with the full default rule set (all codes at default thresholds),
        // matching what the `m1-lint` CLI reports. `reload_config` later swaps in
        // a project-specific `.m1lint.toml` if one is discovered. Seeding with the
        // reduced v1 set here meant single-file / no-workspace-root sessions
        // silently dropped L010–L012 until a project root was set.
        Self {
            state: RwLock::new(LintState::from_config(&Config::default(), None)),
        }
    }
}

impl Default for M1Lint {
    fn default() -> Self {
        Self::new()
    }
}

impl LintProvider for M1Lint {
    fn lint(&self, uri: &Url, src: &str, li: &LineIndex, enc: PositionEncoding) -> Vec<LspDiag> {
        // Use only lint findings; syntax errors come from m1-core in analyze().
        let state = self.state.read().unwrap();
        if state.is_excluded(uri) {
            return Vec::new();
        }
        state
            .runner
            .run_source(src)
            .diagnostics
            .iter()
            .map(|d| LspDiag {
                range: range(&d.inner.byte_range, li, enc),
                severity: Some(severity(d.inner.severity)),
                code: Some(NumberOrString::String(d.code.to_string())),
                // Mirror the SARIF `helpUri` (m1-lint report.rs) so the rule
                // code links to its docs in the editor, matching what a CI /
                // code-scanning consumer already gets. The fragment is the
                // rule's stable name, exactly as the SARIF emitter uses.
                code_description: code_description(LINT_DOCS_BASE, d.code.name()),
                source: Some("m1-lint".to_string()),
                message: d.inner.message.clone(),
                ..Default::default()
            })
            .collect()
    }

    fn reload_config(&self, root: &Path) {
        // Discover `.m1lint.toml` (walking up from `root`, then the user-global
        // fallback) and rebuild the rule set. On a config error, keep the
        // current ruleset rather than reverting to defaults silently.
        if let Ok(cfg) = Config::discover(root) {
            *self.state.write().unwrap() = LintState::from_config(&cfg, Some(root));
        }
    }

    fn set_lint_config(&self, cfg: &Config, root: &Path) {
        // Apply a config resolved centrally by the unified `m1-tools.toml` layer
        // (thresholds + enabled set), replacing any file-discovered one.
        *self.state.write().unwrap() = LintState::from_config(cfg, Some(root));
    }

    fn fix(&self, uri: &Url, src: &str) -> Option<String> {
        // Apply every enabled fixable rule until stable (idempotent in one pass),
        // matching `m1-lint --fix`. An unsafe fix is dropped rather than corrupt
        // the buffer.
        let state = self.state.read().unwrap();
        if state.is_excluded(uri) {
            return None;
        }
        state.runner.fix_source_stable(src).ok().flatten()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn uri() -> Url {
        Url::parse("file:///Test.m1scr").unwrap()
    }

    #[test]
    fn fix_applies_fixable_lint_rules() {
        // #158: a comment-style (L011) issue is fixable but had no LSP quick-fix.
        // The provider's `fix` should return the corrected source.
        let l = M1Lint::new();
        let fixed = l
            .fix(&uri(), "//x\n")
            .expect("a fixable comment-style issue should produce a fix");
        assert!(fixed.contains("// x"), "expected `// x`, got {fixed:?}");
    }

    #[test]
    fn fix_returns_none_when_already_clean() {
        let l = M1Lint::new();
        assert!(
            l.fix(&uri(), "x = 1;\n").is_none(),
            "clean source should yield no fix"
        );
    }

    #[test]
    fn excluded_document_is_neither_linted_nor_fixed() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = Config {
            exclude: vec!["generated/*".into()],
            ..Config::default()
        };
        let lint = M1Lint::new();
        lint.set_lint_config(&cfg, tmp.path());
        let excluded = Url::from_file_path(tmp.path().join("generated/Test.m1scr")).unwrap();
        let included = Url::from_file_path(tmp.path().join("src/Test.m1scr")).unwrap();
        let src = "//x\n";
        let li = LineIndex::new(src);

        assert!(
            lint.lint(&excluded, src, &li, PositionEncoding::Utf16)
                .is_empty()
        );
        assert!(lint.fix(&excluded, src).is_none());
        assert!(lint.fix(&included, src).is_some());
    }

    #[test]
    fn lint_diagnostic_sets_code_description_href() {
        // L004 (`==` -> `eq`) fires deterministically. Its diagnostic must now
        // carry a clickable docs link whose fragment equals the SARIF
        // `helpUri` convention (m1-lint report.rs): base#<rule-name>. L004's
        // stable name is `eq-operator-preferred`.
        let src = "if (a == b) {\n    x = 1;\n}\n";
        let li = LineIndex::new(src);
        let diags = M1Lint::new().lint(&uri(), src, &li, PositionEncoding::Utf16);
        let l004 = diags
            .iter()
            .find(|d| matches!(&d.code, Some(NumberOrString::String(s)) if s == "L004"))
            .expect("L004 should fire on `==`");
        let href = l004
            .code_description
            .as_ref()
            .expect("L004 diagnostic should carry a code_description")
            .href
            .as_str();
        assert_eq!(
            href, "https://github.com/C-Nucifora/m1-lint#eq-operator-preferred",
            "editor link must mirror the SARIF helpUri fragment (the rule name)"
        );
    }

    #[test]
    fn flags_eq_eq() {
        // L004: `==` should be `eq`. Adjust the snippet if the lint snapshot differs.
        let src = "if (a == b) {\n    x = 1;\n}\n";
        let li = LineIndex::new(src);
        let diags = M1Lint::new().lint(&uri(), src, &li, PositionEncoding::Utf16);
        assert!(diags.iter().any(|d| d.source.as_deref() == Some("m1-lint")));
    }

    #[test]
    fn default_seed_includes_rules_beyond_the_v1_set() {
        // Regression: a freshly-constructed backend (no project, no
        // reload_config) must report the same rules as the `m1-lint` CLI
        // default, including codes above L009. The old v1 seed dropped
        // L010–L012 until a workspace root was discovered. L010
        // (indentation style) is purely textual, so it is a stable probe.
        // Since m1-lint v0.5.0 the default flags *space* indentation (the
        // manual mandates tabs), so the probe uses a space-indented line.
        let src = "    x = 1;\n";
        let li = LineIndex::new(src);
        let diags = M1Lint::new().lint(&uri(), src, &li, PositionEncoding::Utf16);
        assert!(
            diags
                .iter()
                .any(|d| matches!(&d.code, Some(NumberOrString::String(s)) if s == "L010")),
            "default seed should surface L010 without reload_config; got {:?}",
            diags
                .iter()
                .filter_map(|d| d.code.clone())
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn reload_config_honours_discovered_m1lint_toml() {
        let dir = std::env::temp_dir().join(format!("m1lint_lsp_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(".m1lint.toml"), "ignore = [\"L004\"]\n").unwrap();

        let lint = M1Lint::new();
        let script_uri = Url::from_file_path(dir.join("Test.m1scr")).unwrap();
        let src = "if (a == b) {\n    x = 1;\n}\n";
        let li = LineIndex::new(src);
        let has_l004 = |l: &M1Lint| {
            l.lint(&script_uri, src, &li, PositionEncoding::Utf16)
                .iter()
                .any(|d| matches!(&d.code, Some(NumberOrString::String(s)) if s == "L004"))
        };

        assert!(has_l004(&lint), "default ruleset flags L004 on `==`");
        lint.reload_config(&dir);
        assert!(
            !has_l004(&lint),
            "after discovering a config that ignores L004, it must not fire"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
