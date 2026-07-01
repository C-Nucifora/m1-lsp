// SPDX-License-Identifier: GPL-3.0-or-later
//! Scenario sourcing: build an [`m1_eval::Engine`] for a loaded project and run
//! it **once**, in a fixed precedence order, to produce a single [`m1_eval::Trace`].
//!
//! This is the join between the LSP's loaded project ([`LoadedProject`]) and the
//! evaluator. It owns no evaluation logic of its own — it picks *where the values
//! come from*, calls [`m1_eval::Engine`], and hands back the trace plus a
//! [`Provenance`] tag so downstream rendering (E4+) can be honest about whether a
//! value is measured, counterfactual, or an offline default.
//!
//! ## Source precedence
//!
//! Given a [`LoadedProject`] and the LSP-local [`EvalConfig`] (E1), the source is
//! resolved in this order (matching the design's Milestone E2):
//!
//! 1. **Scenario file** ([`EvalConfig::scenario`]) — highest fidelity. Parsed by
//!    [`Scenario::from_toml_str`] / [`Scenario::from_json_str`] (dispatched on the
//!    file extension) and run via [`Engine::run`]. Provenance: [`Provenance::Scenario`].
//! 2. **Log file** ([`EvalConfig::log`]) — counterfactual ground truth. Attached
//!    with [`Engine::load_log`] and replayed via [`Engine::run_counterfactual_diff`],
//!    whose `.trace` is the result. Provenance: [`Provenance::Log`]. With no channel
//!    override the counterfactual's downstream cone is empty, so it is the
//!    evaluator's documented no-op replay: the logged ground truth is reproduced
//!    verbatim (still [`Provenance::Log`]). A genuine failure — a missing/unreadable
//!    log, or a real override targeting a channel nothing reads — still fails loud
//!    and is handled like any other source failure (see below).
//! 3. **Offline default** — no scenario, no log. A synthesised
//!    [`RunMode::WholeProject`] scenario with **no inputs** is run, so most channels
//!    read the evaluator's offline-default world (calibration defaults, zero-seeded
//!    inputs, Tier-3 stubs). Provenance: [`Provenance::OfflineDefault`]. The hover
//!    text must say so — an offline-default number is never presented as measured.
//!
//! ## Fail-loud, never crash
//!
//! A bad scenario (unparsable / wrong extension), a missing or unreadable log, or
//! a `.ld` log without the `ld` feature is a *fail-loud* condition: the evaluator
//! returns an [`m1_eval::EvalError`] rather than guessing. [`evaluate`] captures it
//! as a one-line **issue** (for the backend to surface once via `window/logMessage`)
//! and **falls back to the offline default**, so hover still works. It never
//! propagates the error and never panics — a misconfigured scenario degrades the
//! feature, it does not break the handler.

use crate::eval::config::EvalConfig;
use crate::eval::{Engine, RunMode, Scenario, Trace};
use crate::project_store::LoadedProject;
use std::path::{Path, PathBuf};

/// Where an evaluated [`Trace`] came from, carried so downstream rendering can be
/// honest. A value sourced from a [`Provenance::Scenario`] or [`Provenance::Log`]
/// reflects a configured run; a [`Provenance::OfflineDefault`] value is the
/// evaluator's default world (no scenario/log) and must be labelled as such.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Provenance {
    /// The trace came from a configured scenario file at this path.
    Scenario(PathBuf),
    /// The trace came from a configured counterfactual log at this path.
    Log(PathBuf),
    /// No scenario/log was configured (or the configured source failed and we
    /// fell back): a synthesised whole-project run in the offline-default world.
    OfflineDefault,
}

/// The outcome of a single evaluation run: the trace, where it came from, and any
/// human-readable issues to surface once via `window/logMessage`.
///
/// [`Self::issues`] is empty on a clean run. It carries exactly one line when a
/// configured scenario/log failed loud and we fell back to the offline default —
/// the backend logs it once (not per hover), and hover still works against the
/// fallback trace.
#[derive(Debug, Clone)]
pub struct EvalOutcome {
    /// The single cached trace this run produced.
    pub trace: Trace,
    /// Where [`Self::trace`] came from, for honest rendering.
    pub provenance: Provenance,
    /// Fail-loud notices (e.g. "scenario failed to parse; using offline default").
    /// Empty on a clean run; one line on a fallback.
    pub issues: Vec<String>,
}

/// The base tick rate for the synthesised offline-default run, in Hz. The offline
/// default has no user-chosen grid, so a fixed sane rate is used; the
/// whole-project scheduler still runs each function at its own call rate over this
/// base tick. 100 Hz mirrors the evaluator's own no-schedule default.
const OFFLINE_BASE_RATE_HZ: f64 = 100.0;

/// A short, bounded duration for the offline-default run, in seconds. Long enough
/// for stateful operators to settle to a converged value (the last-tick policy
/// reads that), short enough to keep a whole-project run cheap on the editor's
/// hot-ish path. Three ticks at [`OFFLINE_BASE_RATE_HZ`].
const OFFLINE_DURATION_S: f64 = 0.03;

/// Build an engine for `lp` and run it once, resolving the value source in the
/// precedence order documented on this module. Never fails: a configured
/// scenario/log that errors is captured as an issue and the run falls back to the
/// offline default, so the returned [`EvalOutcome`] always carries a usable trace.
///
/// The engine is built from exactly the two paths [`LoadedProject`] already
/// carries — `lp.m1prj_path` and `lp.m1cfg_path` — so no path discovery is
/// duplicated here. Relative scenario/log paths in [`EvalConfig`] are resolved
/// against the project root (`lp.root`).
pub fn evaluate(lp: &LoadedProject, cfg: &EvalConfig) -> EvalOutcome {
    // Resolve a configured source first; on any fail-loud error, record the issue
    // and drop through to the offline default so hover still works.
    if let Some(rel) = cfg.scenario.as_ref() {
        let path = resolve_under_root(&lp.root, rel);
        match run_scenario_file(lp, &path) {
            Ok(trace) => {
                return EvalOutcome {
                    trace,
                    provenance: Provenance::Scenario(path),
                    issues: Vec::new(),
                };
            }
            Err(issue) => return offline_fallback(lp, vec![issue]),
        }
    }

    if let Some(rel) = cfg.log.as_ref() {
        let path = resolve_under_root(&lp.root, rel);
        // No override field exists on `EvalConfig` yet (E1); the counterfactual
        // therefore runs with no overrides. With an empty override set the
        // downstream cone is empty and the run is the evaluator's no-op replay —
        // the logged ground truth reproduced verbatim as a `Provenance::Log` trace.
        match run_log_counterfactual(lp, &path, &[]) {
            Ok(trace) => {
                return EvalOutcome {
                    trace,
                    provenance: Provenance::Log(path),
                    issues: Vec::new(),
                };
            }
            Err(issue) => return offline_fallback(lp, vec![issue]),
        }
    }

    offline_fallback(lp, Vec::new())
}

/// Resolve a configured (possibly relative) scenario/log path against the project
/// root. An absolute configured path is used verbatim; a relative one is joined
/// onto `root` so `m1.eval.scenario = "scenarios/idle.toml"` lands inside the
/// workspace.
fn resolve_under_root(root: &Path, configured: &Path) -> PathBuf {
    if configured.is_absolute() {
        configured.to_path_buf()
    } else {
        root.join(configured)
    }
}

/// Load the engine and run a configured scenario file, dispatching on extension
/// (`.toml` / `.json`; anything else is a fail-loud error). The scenario text is
/// read from disk; a read or parse failure is returned as a one-line issue.
fn run_scenario_file(lp: &LoadedProject, path: &Path) -> Result<Trace, String> {
    let engine = load_engine(lp)?;
    let text = std::fs::read_to_string(path)
        .map_err(|e| format!("eval scenario {} unreadable: {e}", path.display()))?;
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase);
    let scenario = match ext.as_deref() {
        Some("toml") => Scenario::from_toml_str(&text),
        Some("json") => Scenario::from_json_str(&text),
        other => {
            let found = other.unwrap_or("(none)");
            return Err(format!(
                "eval scenario {} has unsupported extension `.{found}` (expected `.toml` or `.json`)",
                path.display()
            ));
        }
    }
    .map_err(|e| format!("eval scenario {} failed to parse: {e}", path.display()))?;
    engine
        .run(&scenario)
        .map_err(|e| format!("eval scenario {} failed to run: {e}", path.display()))
}

/// Load the engine, attach a configured log, apply any channel overrides, and
/// return the counterfactual replay's trace. With no overrides this is the
/// evaluator's no-op replay (the logged ground truth reproduced verbatim). A
/// missing/unreadable log, an `.ld` log without the `ld` feature, a malformed
/// override spec, or a real override targeting a channel nothing reads is returned
/// as a one-line issue.
fn run_log_counterfactual(
    lp: &LoadedProject,
    path: &Path,
    overrides: &[String],
) -> Result<Trace, String> {
    let mut engine = load_engine(lp)?;
    engine
        .load_log(path)
        .map_err(|e| format!("eval log {} failed to load: {e}", path.display()))?;
    for spec in overrides {
        engine
            .override_channel(spec)
            .map_err(|e| format!("eval override `{spec}` rejected: {e}"))?;
    }
    engine
        .run_counterfactual_diff()
        .map(|cf| cf.trace)
        .map_err(|e| format!("eval log {} counterfactual failed: {e}", path.display()))
}

/// Build an [`Engine`] from the project's `.m1prj` (+ optional `.m1cfg`). The two
/// paths come straight off [`LoadedProject`]; a load failure is a one-line issue.
fn load_engine(lp: &LoadedProject) -> Result<Engine, String> {
    Engine::load(&lp.m1prj_path, lp.m1cfg_path.as_deref()).map_err(|e| {
        format!(
            "eval engine load failed for {}: {e}",
            lp.m1prj_path.display()
        )
    })
}

/// Run the synthesised offline-default scenario: a [`RunMode::WholeProject`] run
/// with no inputs over a short bounded grid. Returns the trace tagged
/// [`Provenance::OfflineDefault`], carrying any `issues` accumulated from a failed
/// configured source. If even the offline-default run fails (a genuinely broken
/// project), an empty trace is returned with an extra issue line — hover then
/// simply shows no value, never a crash.
fn offline_fallback(lp: &LoadedProject, mut issues: Vec<String>) -> EvalOutcome {
    let trace = match load_engine(lp).and_then(|engine| {
        engine
            .run(&offline_scenario())
            .map_err(|e| format!("offline-default run failed: {e}"))
    }) {
        Ok(trace) => trace,
        Err(issue) => {
            issues.push(issue);
            Trace::new()
        }
    };
    EvalOutcome {
        trace,
        provenance: Provenance::OfflineDefault,
        issues,
    }
}

/// The synthesised offline-default scenario: run every scheduled function at its
/// own rate (`WholeProject`), with no externally-supplied inputs, over a short
/// bounded grid. Channels with no schedule simply do not appear — honest, since
/// the offline default computes only what the project itself drives.
fn offline_scenario() -> Scenario {
    Scenario {
        mode: RunMode::WholeProject,
        inputs: Vec::new(),
        duration_s: OFFLINE_DURATION_S,
        base_rate_hz: OFFLINE_BASE_RATE_HZ,
        overrides: Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::eval::config::EvalConfig;
    use crate::project_store::ProjectStore;
    use std::path::{Path, PathBuf};

    /// Path to the in-tree mini fixture (a self-contained, synthetic project —
    /// no proprietary content). It mirrors m1-eval's own `mini`: one `Demo.Update`
    /// function scaling `Speed * Gain` into `Output`.
    fn mini_dir() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/mini")
    }

    /// Load the mini fixture through the real [`ProjectStore`] and run `f` against
    /// the resulting [`LoadedProject`] — the same path the backend uses, so the
    /// engine sees exactly the `m1prj`/`m1cfg` the store discovered.
    fn with_mini<R>(f: impl FnOnce(&LoadedProject) -> R) -> R {
        let store = ProjectStore::new();
        assert!(
            store.discover_and_load(&mini_dir()).unwrap(),
            "mini fixture must load"
        );
        store.with_project(|p| f(p.expect("mini project loaded")))
    }

    /// A scenario file → a trace whose `Output` column carries the computed value,
    /// tagged with `Provenance::Scenario`. Mirrors m1-eval's own function-mode run.
    #[test]
    fn scenario_path_yields_channel_column() {
        let tmp = tempfile::tempdir().unwrap();
        let scenario = tmp.path().join("idle.toml");
        std::fs::write(
            &scenario,
            "mode = \"function\"\ntarget = \"Demo.Update\"\n\
             duration_s = 0.03\nbase_rate_hz = 100.0\n\
             [[inputs]]\nchannel = \"Root.Demo.Speed\"\nconst = 20.0\n",
        )
        .unwrap();
        let cfg = EvalConfig {
            enabled: true,
            scenario: Some(scenario.clone()),
            ..EvalConfig::default()
        };
        with_mini(|lp| {
            let out = evaluate(lp, &cfg);
            assert_eq!(out.provenance, Provenance::Scenario(scenario.clone()));
            assert!(
                out.issues.is_empty(),
                "clean scenario run: {:?}",
                out.issues
            );
            let col = out
                .trace
                .channels
                .get("Root.Demo.Output")
                .expect("Output column present");
            // 0.03 s at 100 Hz = 3 ticks; Output = 20 * 2.5 (Gain) = 50 each.
            assert_eq!(col, &vec![crate::eval::Value::Float(50.0); 3]);
        });
    }

    /// A scenario path **relative** to the workspace root resolves against it.
    #[test]
    fn relative_scenario_path_resolves_under_root() {
        with_mini(|lp| {
            // Write the scenario inside the project root so a relative path finds it.
            let scen_rel = PathBuf::from("idle.toml");
            std::fs::write(
                lp.root.join(&scen_rel),
                "mode = \"function\"\ntarget = \"Demo.Update\"\n\
                 duration_s = 0.03\nbase_rate_hz = 100.0\n\
                 [[inputs]]\nchannel = \"Root.Demo.Speed\"\nconst = 20.0\n",
            )
            .unwrap();
            let cfg = EvalConfig {
                enabled: true,
                scenario: Some(scen_rel),
                ..EvalConfig::default()
            };
            let out = evaluate(lp, &cfg);
            assert!(
                matches!(out.provenance, Provenance::Scenario(_)),
                "relative scenario resolved: {:?}",
                out.provenance
            );
            assert!(out.trace.channels.contains_key("Root.Demo.Output"));
            // Tidy up the fixture dir we wrote into.
            let _ = std::fs::remove_file(lp.root.join("idle.toml"));
        });
    }

    /// No scenario and no log → the synthesised offline-default whole-project run,
    /// tagged `Provenance::OfflineDefault`, with no issues.
    #[test]
    fn no_source_is_offline_default() {
        let cfg = EvalConfig::default();
        with_mini(|lp| {
            let out = evaluate(lp, &cfg);
            assert_eq!(out.provenance, Provenance::OfflineDefault);
            assert!(out.issues.is_empty(), "clean fallback: {:?}", out.issues);
            // The offline default produces a (possibly empty) usable trace; the run
            // succeeded, which is all E2 asserts (rendering honesty is E4).
        });
    }

    /// A scenario path that does not exist fails loud: the error is captured as one
    /// issue line and the run falls back to the offline default.
    #[test]
    fn missing_scenario_falls_back_with_issue() {
        let cfg = EvalConfig {
            enabled: true,
            scenario: Some(PathBuf::from("/no/such/scenario.toml")),
            ..EvalConfig::default()
        };
        with_mini(|lp| {
            let out = evaluate(lp, &cfg);
            assert_eq!(
                out.provenance,
                Provenance::OfflineDefault,
                "a missing scenario falls back to the offline default"
            );
            assert_eq!(
                out.issues.len(),
                1,
                "exactly one fail-loud line: {:?}",
                out.issues
            );
            assert!(
                out.issues[0].contains("scenario") && out.issues[0].contains("unreadable"),
                "issue names the unreadable scenario: {:?}",
                out.issues
            );
        });
    }

    /// An unparsable scenario (valid path, garbage TOML) fails loud and falls back.
    #[test]
    fn unparsable_scenario_falls_back_with_issue() {
        let tmp = tempfile::tempdir().unwrap();
        let scenario = tmp.path().join("broken.toml");
        std::fs::write(&scenario, "this is = = not valid toml [[[").unwrap();
        let cfg = EvalConfig {
            enabled: true,
            scenario: Some(scenario),
            ..EvalConfig::default()
        };
        with_mini(|lp| {
            let out = evaluate(lp, &cfg);
            assert_eq!(out.provenance, Provenance::OfflineDefault);
            assert_eq!(out.issues.len(), 1, "{:?}", out.issues);
            assert!(
                out.issues[0].contains("failed to parse"),
                "issue names the parse failure: {:?}",
                out.issues
            );
        });
    }

    /// A scenario with an unsupported extension fails loud before any I/O parse.
    #[test]
    fn wrong_extension_scenario_falls_back_with_issue() {
        let tmp = tempfile::tempdir().unwrap();
        let scenario = tmp.path().join("idle.yaml");
        std::fs::write(&scenario, "mode: function").unwrap();
        let cfg = EvalConfig {
            enabled: true,
            scenario: Some(scenario),
            ..EvalConfig::default()
        };
        with_mini(|lp| {
            let out = evaluate(lp, &cfg);
            assert_eq!(out.provenance, Provenance::OfflineDefault);
            assert_eq!(out.issues.len(), 1, "{:?}", out.issues);
            assert!(
                out.issues[0].contains("unsupported extension"),
                "{:?}",
                out.issues
            );
        });
    }

    /// A `.ld` log without the `ld` feature fails loud (the engine names the
    /// feature to rebuild with) and the run falls back to the offline default.
    #[test]
    fn ld_log_without_feature_falls_back_with_issue() {
        let tmp = tempfile::tempdir().unwrap();
        let log = tmp.path().join("run.ld");
        std::fs::write(&log, b"not really a binary ld file").unwrap();
        let cfg = EvalConfig {
            enabled: true,
            log: Some(log),
            ..EvalConfig::default()
        };
        with_mini(|lp| {
            let out = evaluate(lp, &cfg);
            assert_eq!(
                out.provenance,
                Provenance::OfflineDefault,
                "a .ld log without the feature falls back"
            );
            assert_eq!(out.issues.len(), 1, "{:?}", out.issues);
            assert!(
                out.issues[0].contains("ld") && out.issues[0].contains("feature"),
                "issue names the missing ld feature: {:?}",
                out.issues
            );
        });
    }

    /// A missing log path fails loud and falls back with an issue.
    #[test]
    fn missing_log_falls_back_with_issue() {
        let cfg = EvalConfig {
            enabled: true,
            log: Some(PathBuf::from("/no/such/run.csv")),
            ..EvalConfig::default()
        };
        with_mini(|lp| {
            let out = evaluate(lp, &cfg);
            assert_eq!(out.provenance, Provenance::OfflineDefault);
            assert_eq!(out.issues.len(), 1, "{:?}", out.issues);
            assert!(out.issues[0].contains("log"), "{:?}", out.issues);
        });
    }

    /// A configured `.csv` log with **no** override reproduces the log verbatim: an
    /// empty override set is the evaluator's documented no-op invariant (`--log`
    /// with no `--override`), so the counterfactual has an empty downstream cone and
    /// simply replays the logged ground truth. The run therefore succeeds with
    /// [`Provenance::Log`] and no issue — the logged (measured) channels are carried
    /// straight through. (This is the E1 state until `EvalConfig` grows an override
    /// field; with an override, the cone recomputes — see
    /// `log_with_override_yields_log_provenance_trace`.)
    #[test]
    fn log_without_override_reproduces_log_verbatim() {
        let tmp = tempfile::tempdir().unwrap();
        let log = tmp.path().join("run.csv");
        std::fs::write(
            &log,
            "time,Root.Demo.Speed,Root.Demo.Output\n0,20,50\n0.01,20,50\n",
        )
        .unwrap();
        let cfg = EvalConfig {
            enabled: true,
            log: Some(log.clone()),
            ..EvalConfig::default()
        };
        with_mini(|lp| {
            let out = evaluate(lp, &cfg);
            assert_eq!(
                out.provenance,
                Provenance::Log(log.clone()),
                "a log with no override is the no-op counterfactual: replay the log"
            );
            assert!(
                out.issues.is_empty(),
                "the no-op replay does not fail loud: {:?}",
                out.issues
            );
            assert!(
                out.trace.channels.contains_key("Root.Demo.Output"),
                "the replayed trace carries the logged channels verbatim: {:?}",
                out.trace.channels.keys().collect::<Vec<_>>()
            );
        });
    }

    /// The log branch *with* an override recomputes the override's downstream cone
    /// and yields a `Provenance::Log` trace. This exercises the successful log path
    /// the public `evaluate` cannot yet reach (no override field on `EvalConfig`),
    /// proving the plumbing is correct for when one is added.
    #[test]
    fn log_with_override_yields_log_provenance_trace() {
        let tmp = tempfile::tempdir().unwrap();
        let log = tmp.path().join("run.csv");
        std::fs::write(
            &log,
            "time,Root.Demo.Speed,Root.Demo.Output\n0,20,50\n0.01,20,50\n",
        )
        .unwrap();
        with_mini(|lp| {
            // Override Speed so the Demo.Update cone (which reads Speed) recomputes.
            let trace = run_log_counterfactual(lp, &log, &["Root.Demo.Speed=40".to_string()])
                .expect("log counterfactual with an override succeeds");
            assert!(
                trace.channels.contains_key("Root.Demo.Output"),
                "the recomputed cone carries Output: {:?}",
                trace.channels.keys().collect::<Vec<_>>()
            );
        });
    }

    /// `evaluate` never returns an error path: even with both a broken scenario and
    /// a broken log configured, the scenario (checked first) fails loud and the run
    /// falls back to the offline default with exactly its one issue line.
    #[test]
    fn scenario_checked_before_log() {
        let cfg = EvalConfig {
            enabled: true,
            scenario: Some(PathBuf::from("/no/such/scenario.toml")),
            log: Some(PathBuf::from("/no/such/run.csv")),
            ..EvalConfig::default()
        };
        with_mini(|lp| {
            let out = evaluate(lp, &cfg);
            assert_eq!(out.provenance, Provenance::OfflineDefault);
            // Only the scenario issue — the log is never reached.
            assert_eq!(out.issues.len(), 1, "{:?}", out.issues);
            assert!(out.issues[0].contains("scenario"), "{:?}", out.issues);
        });
    }
}
