//! Unified-config resolution and application: layering editor settings under any
//! `m1-tools.toml`, pushing the resolved lint/format/diagnostics config into the
//! backends, and re-resolving the LSP-local eval config. The seam that owns "what
//! configuration is currently applied", separate from the request handlers.
use tower_lsp::lsp_types::MessageType;

use super::Backend;
use crate::config::M1Config;
use crate::eval::EvalConfig;

impl Backend {
    /// Resolve the unified config for `root` (editor settings layered under any
    /// `m1-tools.toml`) and apply it: lint thresholds/rules, formatter options,
    /// and the cross-source diagnostic filter. Records `root` so a later
    /// `didChangeConfiguration` can re-resolve against the same workspace.
    pub(super) fn apply_config(&self, root: &std::path::Path) {
        let editor = self.editor_settings.read().unwrap().clone();
        let (cfg, mut issues) = M1Config::resolve_with_issues(editor.as_ref(), root);
        self.lint.set_lint_config(&cfg.lint);
        self.types.set_type_config(&cfg.diagnostics);
        self.formatter.set_format_options(&cfg.format);
        *self.config.write().unwrap() = cfg;
        *self.config_root.write().unwrap() = Some(root.to_path_buf());
        // The LSP-local eval config (`m1.eval.*`) rides the same editor-settings
        // value but is resolved separately from `M1Config` — `M1ToolsConfig` is
        // tag-pinned with no `[eval]` section. Off by default; a malformed
        // payload degrades to disabled and adds an issue line below rather than
        // disabling the rest of config.
        let (eval_cfg, eval_issues) = EvalConfig::from_editor_settings(editor.as_ref());
        *self.eval_config.write().unwrap() = eval_cfg;
        issues.extend(eval_issues);
        // Surface config problems instead of silently falling back (#278):
        // a malformed m1-tools.toml or a typo'd key looks exactly like "the
        // LSP ignored my setting" without this. Sent fire-and-forget — config
        // application itself must never block on the client.
        if !issues.is_empty() {
            let client = self.client.clone();
            tokio::spawn(async move {
                for issue in issues {
                    client
                        .log_message(MessageType::WARNING, format!("m1-lsp config: {issue}"))
                        .await;
                }
            });
        }
    }

    /// Re-resolve config against the last known root (used by
    /// `didChangeConfiguration`, which carries new editor settings but no root).
    pub(super) fn reapply_config(&self) {
        let root = self.config_root.read().unwrap().clone();
        if let Some(root) = root {
            self.apply_config(&root);
        }
    }
}
