//! Unified toolchain configuration — `m1-tools.toml` (m1-vscode#16, expanded).
//!
//! One file configures *every* part the editors ship: lint thresholds, formatter
//! options, and a cross-source diagnostic `ignore`/`select` filter that applies to
//! L-codes, T-codes and the named intrinsic checks alike. The LSP owns parsing and
//! resolution, so VS Code and Neovim share identical behaviour.
//!
//! Precedence (low → high): tool defaults → editor settings (sent as
//! `initializationOptions` / `didChangeConfiguration`, same shape as the toml) →
//! `m1-tools.toml` discovered in the workspace. A legacy `.m1lint.toml` still
//! drives the lint section when no `m1-tools.toml` is present.
use m1_fmt::FormatOptions;
use m1_lint::config::Config as LintConfig;
use m1_workspace::config::M1ToolsConfig;
use std::collections::HashSet;
use std::path::Path;

/// Cross-source diagnostic filter. `ignore` drops codes from *any* source; a
/// non-empty `select` keeps **only** the listed codes. Codes are matched against
/// each diagnostic's `code` string (`L001`, `T041`, `unsupported-c-token`, …).
#[derive(Debug, Clone, Default)]
pub struct DiagFilter {
    pub ignore: HashSet<String>,
    pub select: HashSet<String>,
}

impl DiagFilter {
    /// Whether a diagnostic with this code should be published.
    pub fn allows(&self, code: &str) -> bool {
        if !self.select.is_empty() && !self.select.contains(code) {
            return false;
        }
        !self.ignore.contains(code)
    }

    /// True when no filtering is configured (the common case — skip the walk).
    pub fn is_empty(&self) -> bool {
        self.ignore.is_empty() && self.select.is_empty()
    }
}

/// The fully-resolved configuration the LSP applies to its backends.
#[derive(Debug, Clone, Default)]
pub struct M1Config {
    pub lint: LintConfig,
    pub format: FormatOptions,
    pub diagnostics: DiagFilter,
}

impl M1Config {
    /// Resolve the effective config for a project rooted at `root`, layering
    /// `editor` settings (JSON, same shape as the toml) over the defaults and the
    /// workspace `m1-tools.toml` over both. With no `m1-tools.toml`, a legacy
    /// `.m1lint.toml` still configures the lint section (back-compat).
    pub fn resolve(editor: Option<&serde_json::Value>, root: &Path) -> M1Config {
        let mut cfg = M1Config::default();
        if let Some(v) = editor
            && let Ok(tc) = serde_json::from_value::<M1ToolsConfig>(v.clone())
        {
            apply(tc, &mut cfg);
        }
        match M1ToolsConfig::discover(root) {
            Some(tc) => apply(tc, &mut cfg),
            // No unified file: keep the editor/default lint config unless a legacy
            // `.m1lint.toml` is present, which then takes over the lint section.
            None => {
                if m1_workspace::find_upward(root, ".m1lint.toml").is_some()
                    && let Ok(lint) = LintConfig::discover(root)
                {
                    cfg.lint = lint;
                }
            }
        }
        cfg
    }
}

/// Overlay a parsed unified config onto `cfg`; unset fields leave the lower layer
/// untouched. `[format].indent_style` is shared — it drives both the formatter and
/// the linter (L010).
fn apply(tc: M1ToolsConfig, cfg: &mut M1Config) {
    if let Some(n) = tc.lint.max_line_length {
        cfg.lint.max_line_length = n;
    }
    if let Some(n) = tc.lint.max_nesting_depth {
        cfg.lint.max_nesting_depth = n;
    }
    if let Some(n) = tc.lint.max_complexity {
        cfg.lint.max_complexity = n;
    }
    if let Some(n) = tc.lint.max_cognitive_complexity {
        cfg.lint.max_cognitive_complexity = n;
    }
    if let Some(ex) = tc.lint.exclude {
        cfg.lint.exclude = ex;
    }
    if let Some(n) = tc.format.line_width {
        cfg.format.line_width = n;
    }
    if let Some(n) = tc.format.max_blank_lines {
        cfg.format.max_blank_lines = n;
    }
    if let Some(n) = tc.format.indent_width {
        cfg.format.indent_width = n;
    }
    if let Some(s) = tc.format.indent_style.as_deref() {
        if let Some(fs) = m1_fmt::config::parse_indent_style(s) {
            cfg.format.indent_style = fs;
        }
        if let Some(ls) = m1_lint::config::IndentStyle::parse(s) {
            cfg.lint.indent_style = ls;
        }
    }
    if let Some(s) = tc
        .format
        .brace_style
        .as_deref()
        .and_then(m1_fmt::config::parse_brace_style)
    {
        cfg.format.brace_style = s;
    }
    if let Some(ig) = tc.diagnostics.ignore {
        cfg.diagnostics.ignore = ig.into_iter().collect();
    }
    if let Some(se) = tc.diagnostics.select {
        cfg.diagnostics.select = se.into_iter().collect();
    }
}

/// A fully-commented `m1-tools.toml` pre-filled with every default, plus the full
/// catalogue of L- and T-codes so a user knows what `ignore`/`select` can name.
/// Built from the live defaults and code catalogues — it never drifts. Emitted by
/// `m1-lsp --scaffold-config`; the editors write it to the workspace.
pub fn scaffold() -> String {
    use m1_lint::diagnostic::LintCode;
    use m1_typecheck::diagnostics::TypeCode;
    use std::fmt::Write;

    let lint = LintConfig::default();
    let fmt = FormatOptions::default();
    let mut s = String::new();

    s.push_str("# m1-tools.toml — M1 toolchain configuration\n");
    s.push_str("# Shared by the VS Code extension and the Neovim plugins.\n");
    s.push_str("# Generated with every default filled in; edit a value to change it.\n\n");

    s.push_str("[lint]\n");
    let _ = writeln!(s, "max_line_length = {}", lint.max_line_length);
    let _ = writeln!(s, "max_nesting_depth = {}", lint.max_nesting_depth);
    let _ = writeln!(s, "max_complexity = {}", lint.max_complexity);
    let _ = writeln!(
        s,
        "max_cognitive_complexity = {}",
        lint.max_cognitive_complexity
    );
    s.push_str("exclude = []            # globs to skip (e.g. \"*.gen.m1scr\")\n\n");

    s.push_str("[format]\n");
    let _ = writeln!(s, "line_width = {}", fmt.line_width);
    let _ = writeln!(s, "max_blank_lines = {}", fmt.max_blank_lines);
    let indent = match fmt.indent_style {
        m1_fmt::IndentStyle::Tab => "tab",
        m1_fmt::IndentStyle::Spaces => "spaces",
    };
    let brace = match fmt.brace_style {
        m1_fmt::BraceStyle::Allman => "allman",
        m1_fmt::BraceStyle::KAndR => "kr",
    };
    // indent_style is shared by the formatter and the linter (L010).
    let _ = writeln!(
        s,
        "indent_style = \"{indent}\"   # \"tab\" | \"spaces\" (shared with lint)"
    );
    let _ = writeln!(s, "indent_width = {}", fmt.indent_width);
    let _ = writeln!(s, "brace_style = \"{brace}\"      # \"allman\" | \"kr\"");
    s.push('\n');

    s.push_str("[diagnostics]\n");
    s.push_str("# Disable any diagnostic from any tool, or restrict to a subset.\n");
    s.push_str("# Accepts any code listed below (lint L*, typecheck T*).\n");
    s.push_str("ignore = []             # disable these codes\n");
    s.push_str("select = []             # if non-empty, run ONLY these codes\n\n");

    s.push_str("# Lint rules (m1-lint):\n");
    for code in LintCode::all_codes() {
        let fixable = if code.fixable() { "  (fixable)" } else { "" };
        let _ = writeln!(s, "#   {code}  {}{fixable}", code.name());
    }
    s.push_str("# Type checks (m1-typecheck):\n");
    for code in TypeCode::all_codes() {
        let _ = writeln!(s, "#   {}  {}", code.as_str(), code.name());
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filter_ignore_and_select() {
        let mut f = DiagFilter::default();
        assert!(f.allows("L001") && f.allows("T041"));
        f.ignore.insert("L001".into());
        assert!(!f.allows("L001") && f.allows("T041"));
        let mut g = DiagFilter::default();
        g.select.insert("T041".into());
        assert!(g.allows("T041") && !g.allows("L001"));
    }

    #[test]
    fn toml_overrides_editor_overrides_defaults() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join("m1-tools.toml"),
            "[lint]\nmax_line_length = 120\n[diagnostics]\nignore = [\"T041\", \"L010\"]\n",
        )
        .unwrap();
        // Editor settings set a different line length + a format width; the toml
        // wins on line length, the editor value survives where the toml is silent.
        let editor = serde_json::json!({
            "lint": { "max_line_length": 100 },
            "format": { "line_width": 70 }
        });
        let cfg = M1Config::resolve(Some(&editor), tmp.path());
        assert_eq!(cfg.lint.max_line_length, 120, "toml wins over editor");
        assert_eq!(cfg.format.line_width, 70, "editor wins over default");
        assert_eq!(cfg.format.max_blank_lines, 2, "untouched default");
        assert!(!cfg.diagnostics.allows("T041"));
        assert!(!cfg.diagnostics.allows("L010"));
        assert!(cfg.diagnostics.allows("L001"));
    }

    #[test]
    fn scaffold_parses_back_to_defaults_and_lists_all_codes() {
        use m1_lint::diagnostic::LintCode;
        use m1_typecheck::diagnostics::TypeCode;
        let toml = scaffold();
        // Lists every L and T code.
        for c in LintCode::all_codes() {
            assert!(toml.contains(&c.to_string()), "missing {c}");
        }
        for c in TypeCode::all_codes() {
            assert!(toml.contains(c.as_str()), "missing {}", c.as_str());
        }
        // Re-parsing the generated file yields the defaults (it's just defaults).
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("m1-tools.toml"), &toml).unwrap();
        let cfg = M1Config::resolve(None, tmp.path());
        let d = M1Config::default();
        assert_eq!(cfg.lint.max_line_length, d.lint.max_line_length);
        assert_eq!(cfg.format.line_width, d.format.line_width);
        assert!(cfg.diagnostics.is_empty());
    }

    #[test]
    fn format_style_keys_map_to_fmt_and_lint() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join("m1-tools.toml"),
            "[format]\nbrace_style = \"kr\"\nindent_style = \"spaces\"\nindent_width = 2\n",
        )
        .unwrap();
        let cfg = M1Config::resolve(None, tmp.path());
        assert_eq!(cfg.format.brace_style, m1_fmt::BraceStyle::KAndR);
        assert_eq!(cfg.format.indent_style, m1_fmt::IndentStyle::Spaces);
        assert_eq!(cfg.format.indent_width, 2);
        // The shared indent decision also drives the linter (L010).
        assert_eq!(cfg.lint.indent_style, m1_lint::config::IndentStyle::Spaces);
    }

    #[test]
    fn scaffold_emits_style_keys() {
        let toml = scaffold();
        assert!(toml.contains("brace_style"));
        assert!(toml.contains("indent_style"));
        assert!(toml.contains("indent_width"));
    }
}
