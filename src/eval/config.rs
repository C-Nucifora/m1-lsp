// SPDX-License-Identifier: GPL-3.0-or-later
//! LSP-local evaluation settings, deserialised from `m1.eval.*` editor settings.
//!
//! Evaluation configuration is **not** part of `m1-workspace`'s `M1ToolsConfig`:
//! that struct (pinned by tag) ships only `lint`/`format`/`diagnostics` sections
//! and has no `[eval]` block, so adding one would need an upstream release first.
//! To stay additive and tag-pure, the eval settings are sourced from the LSP's
//! own editor-settings JSON — the same `initializationOptions` /
//! `didChangeConfiguration` payload the backend already receives — under an
//! `eval` sub-object:
//!
//! ```jsonc
//! { "eval": { "enabled": true, "scenario": "scenarios/idle.toml",
//!             "inlay_values": false } }
//! ```
//!
//! Everything is **off by default**: with no `eval` key (the common case) or an
//! `eval` key set to `false`, [`EvalConfig::default`] applies and hover/inlay
//! behave exactly as before. A malformed `eval` payload never panics and never
//! disables the rest of config — it degrades to the disabled default and reports
//! one human-readable issue line, mirroring
//! [`crate::config::M1Config::resolve_with_issues`].

use serde::Deserialize;
use std::path::PathBuf;

/// Which tick of the cached [`m1_eval::Trace`] a hover/inlay value is read from.
///
/// This is an LSP-rendering choice, not an m1-eval concept: the evaluator
/// produces a full per-tick trace and the LSP picks one tick to surface. The
/// default is the **last** tick — for a settled offline-default run that is the
/// channel's converged value, which is the most useful single number to show.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TickPolicy {
    /// Read the value at the first tick (`t = time[0]`).
    First,
    /// Read the value at the last tick (`t = time[time.len() - 1]`).
    #[default]
    Last,
}

/// The resolved, LSP-local evaluation configuration.
///
/// Deserialised from the `eval` sub-object of the editor-settings JSON. Unknown
/// keys are ignored (forward-compatibility), but a payload whose *shape* is wrong
/// (e.g. `enabled` set to a string) is rejected — [`Self::from_editor_settings`]
/// turns that into a disabled default plus an issue line rather than a panic.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct EvalConfig {
    /// Master switch. **Off by default**: with eval disabled, hover and inlay
    /// behave exactly as today and no engine is ever built.
    pub enabled: bool,
    /// A configured scenario file (`.toml`/`.json`) — the highest-fidelity
    /// source of evaluated values (E2). Relative to the workspace root.
    pub scenario: Option<PathBuf>,
    /// A configured log file (`.csv`; `.ld` only under m1-eval's `ld` feature) —
    /// the counterfactual ground-truth source (E2). Relative to the root.
    pub log: Option<PathBuf>,
    /// Which trace tick a surfaced value is read from (default: last).
    pub tick: TickPolicy,
    /// Whether to render inline computed-value inlay hints (E6). **Off by
    /// default**: with this off, only the existing type/unit/param hints appear.
    pub inlay_values: bool,
}

impl Default for EvalConfig {
    /// Evaluation is **disabled** by default: no scenario, no log, last-tick
    /// policy, no value inlays.
    fn default() -> Self {
        EvalConfig {
            enabled: false,
            scenario: None,
            log: None,
            tick: TickPolicy::Last,
            inlay_values: false,
        }
    }
}

impl EvalConfig {
    /// Resolve the eval config from the editor-settings JSON the backend holds
    /// (the same `{ lint, format, diagnostics, eval }`-shaped value passed to
    /// [`crate::config::M1Config::resolve_with_issues`]).
    ///
    /// Behaviour, mirroring the unified config's issue handling (#278):
    /// - No `editor` value, or no `eval` key → the disabled [`Self::default`],
    ///   no issues. This is the common case.
    /// - A well-shaped `eval` object → the parsed config, no issues.
    /// - A malformed `eval` payload (wrong types, unknown keys) → the disabled
    ///   default plus **one** issue line for `window/logMessage`. It never
    ///   panics and never affects the surrounding (lint/format/diagnostics)
    ///   config, which the backend resolves separately.
    pub fn from_editor_settings(editor: Option<&serde_json::Value>) -> (EvalConfig, Vec<String>) {
        let Some(eval) = editor.and_then(|v| v.get("eval")) else {
            return (EvalConfig::default(), Vec::new());
        };
        match serde_json::from_value::<EvalConfig>(eval.clone()) {
            Ok(cfg) => (cfg, Vec::new()),
            Err(e) => (
                EvalConfig::default(),
                vec![format!("eval settings ignored (invalid shape): {e}")],
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// With nothing configured, eval is disabled and inert — the contract the
    /// rest of the integration leans on for "behaves exactly as today".
    #[test]
    fn default_is_disabled() {
        let cfg = EvalConfig::default();
        assert!(!cfg.enabled);
        assert!(cfg.scenario.is_none());
        assert!(cfg.log.is_none());
        assert_eq!(cfg.tick, TickPolicy::Last);
        assert!(!cfg.inlay_values);
    }

    /// No editor settings at all, or settings with no `eval` key, yield the
    /// disabled default with no issues reported.
    #[test]
    fn no_eval_key_is_default_no_issues() {
        let (cfg, issues) = EvalConfig::from_editor_settings(None);
        assert_eq!(cfg, EvalConfig::default());
        assert!(issues.is_empty());

        // An editor payload that configures the other tools but not eval.
        let editor = serde_json::json!({
            "lint": { "max_line_length": 100 },
            "format": { "line_width": 70 }
        });
        let (cfg, issues) = EvalConfig::from_editor_settings(Some(&editor));
        assert_eq!(cfg, EvalConfig::default(), "no eval key → disabled default");
        assert!(issues.is_empty(), "{issues:?}");
    }

    /// A well-shaped `eval` object parses into the expected config.
    #[test]
    fn valid_json_parses() {
        let editor = serde_json::json!({
            "eval": { "enabled": true, "scenario": "s.toml" }
        });
        let (cfg, issues) = EvalConfig::from_editor_settings(Some(&editor));
        assert!(issues.is_empty(), "{issues:?}");
        assert!(cfg.enabled);
        assert_eq!(cfg.scenario, Some(PathBuf::from("s.toml")));
        assert!(cfg.log.is_none());
        // Unset keys keep their disabled defaults.
        assert_eq!(cfg.tick, TickPolicy::Last);
        assert!(!cfg.inlay_values);
    }

    /// Every field can be set, including the log path, an explicit tick policy,
    /// and the value-inlay opt-in.
    #[test]
    fn all_fields_parse() {
        let editor = serde_json::json!({
            "eval": {
                "enabled": true,
                "scenario": "scenarios/idle.toml",
                "log": "logs/run.csv",
                "tick": "first",
                "inlay_values": true
            }
        });
        let (cfg, issues) = EvalConfig::from_editor_settings(Some(&editor));
        assert!(issues.is_empty(), "{issues:?}");
        assert_eq!(
            cfg,
            EvalConfig {
                enabled: true,
                scenario: Some(PathBuf::from("scenarios/idle.toml")),
                log: Some(PathBuf::from("logs/run.csv")),
                tick: TickPolicy::First,
                inlay_values: true,
            }
        );
    }

    /// A garbage `eval` payload (wrong value type) degrades to the disabled
    /// default with one issue line — it never panics and never enables eval.
    #[test]
    fn garbage_payload_degrades_to_disabled_with_issue() {
        let editor = serde_json::json!({ "eval": { "enabled": "yes please" } });
        let (cfg, issues) = EvalConfig::from_editor_settings(Some(&editor));
        assert_eq!(cfg, EvalConfig::default(), "garbage → disabled default");
        assert!(!cfg.enabled);
        assert_eq!(issues.len(), 1, "{issues:?}");
        assert!(issues[0].contains("eval settings ignored"), "{issues:?}");
    }

    /// An unknown key inside `eval` (likely a typo) is rejected as a malformed
    /// shape rather than silently keeping a stale default, so the user learns
    /// their setting did not take effect.
    #[test]
    fn unknown_eval_key_is_reported() {
        let editor = serde_json::json!({ "eval": { "enabld": true } });
        let (cfg, issues) = EvalConfig::from_editor_settings(Some(&editor));
        assert_eq!(cfg, EvalConfig::default());
        assert_eq!(issues.len(), 1, "{issues:?}");
        assert!(issues[0].contains("eval settings ignored"), "{issues:?}");
    }

    /// An `eval` value that is not even an object degrades cleanly.
    #[test]
    fn non_object_eval_degrades() {
        let editor = serde_json::json!({ "eval": "off" });
        let (cfg, issues) = EvalConfig::from_editor_settings(Some(&editor));
        assert_eq!(cfg, EvalConfig::default());
        assert_eq!(issues.len(), 1, "{issues:?}");
    }

    /// An empty `eval` object is valid and yields the disabled default — the
    /// user opted in to the section but set nothing, so nothing changes.
    #[test]
    fn empty_eval_object_is_default() {
        let editor = serde_json::json!({ "eval": {} });
        let (cfg, issues) = EvalConfig::from_editor_settings(Some(&editor));
        assert_eq!(cfg, EvalConfig::default());
        assert!(issues.is_empty(), "{issues:?}");
    }
}
