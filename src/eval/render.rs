// SPDX-License-Identifier: GPL-3.0-or-later
//! Rendering a cached [`m1_eval::Trace`] into hover markdown.
//!
//! This is the *view* half of the eval integration: given a channel path, the
//! cached trace, its [`Provenance`], and a [`TickPolicy`], produce the small
//! markdown fragment hover appends after the existing type/symbol information. It
//! owns no evaluation logic — it reads a column out of the trace and formats one
//! number, honestly labelled.
//!
//! ## Honesty
//!
//! An evaluated number is only as trustworthy as where it came from, so the
//! fragment is explicit about that:
//!
//! - A [`Provenance::OfflineDefault`] value is the evaluator's default world (no
//!   scenario, no log) — most channels then read calibration defaults / zero-seeded
//!   inputs / Tier-3 stubs, so the fragment appends `(offline default — no
//!   scenario)`. An offline number is never presented as if it were measured.
//! - A channel flagged [`m1_eval::Trace::is_external`] is externally driven
//!   (scenario-fed or a documented Tier-3 stub) rather than computed; the fragment
//!   appends `(externally driven)` so an input is never mistaken for an output.
//!
//! Both suffixes can apply at once (an offline-default run whose value is also an
//! external stub), in which case both are shown.

use crate::eval::config::TickPolicy;
use crate::eval::engine::Provenance;
use crate::eval::{Trace, Value};

/// Render a single [`Value`] as its bare display text (no surrounding backticks —
/// the caller wraps it). Mirrors the trace's own CSV/JSON scalar rendering so the
/// number a user sees in hover matches an exported trace:
///
/// - numbers print bare (`50`, `-3`, `2.5`); floats use the shortest round-tripping
///   form (an integral float prints as `50`, not `50.0`),
/// - booleans print `true` / `false`,
/// - an enum prints its `member` name (the human-meaningful form, matching the
///   enum-member styling elsewhere in hover),
/// - a string prints verbatim.
pub fn value_markdown(v: &Value) -> String {
    match v {
        Value::Bool(b) => b.to_string(),
        Value::Int(x) => x.to_string(),
        Value::Uint(x) => x.to_string(),
        Value::Float(x) => fmt_f64(*x),
        Value::Enum { member, .. } => member.clone(),
        Value::Str(s) => s.clone(),
    }
}

/// Format an `f64` compactly: the shortest representation that round-trips, with
/// non-finite values spelled out. Matches [`m1_eval::Trace`]'s own scalar
/// formatting so an integral float shows as `50`, not `50.0`.
fn fmt_f64(x: f64) -> String {
    if x.is_nan() {
        "NaN".to_string()
    } else if x.is_infinite() {
        if x > 0.0 { "Infinity" } else { "-Infinity" }.to_string()
    } else {
        format!("{x}")
    }
}

/// Build the hover value fragment for a channel `path`, or `None` when the trace
/// has no column for it (a group/function/table/parameter the run did not produce
/// a value for simply gets no value line — honest, not an error).
///
/// The value is read at the tick chosen by `tick` ([`TickPolicy::Last`] by default
/// — the settled value of a bounded run). The fragment leads with a blank line so
/// it appends cleanly after the existing symbol markdown:
///
/// ```text
///
/// value: `50` (@ t=0.02s)
/// ```
///
/// followed by any honesty suffix (see the module docs): `(offline default — no
/// scenario)` for [`Provenance::OfflineDefault`], and `(externally driven)` when
/// the channel is [`Trace::is_external`]. An empty column (a channel key present
/// but with no recorded ticks) also yields `None`.
pub fn eval_hover_fragment(
    path: &str,
    trace: &Trace,
    provenance: &Provenance,
    tick: TickPolicy,
) -> Option<String> {
    let column = trace.channels.get(path)?;
    let mut frag = column_fragment(column, trace, provenance, tick)?;
    // `(externally driven)` is a *channel* property — an externally-fed input vs.
    // a computed output — so it only applies to a channel fragment, not an
    // expression one. Both honesty suffixes can apply at once.
    if trace.is_external(path) {
        frag.push_str(" (externally driven)");
    }
    Some(frag)
}

/// Build the hover value fragment for an expression occurrence at
/// `(file_name, byte_offset)`, or `None` when [`Trace::exprs`] holds no column for
/// that site.
///
/// [`Trace::exprs`] is *sparse*: the offline-default / scenario run only records the
/// expressions its sink evaluated, so a hovered sub-expression the run never visited
/// simply yields `None` — no value line, honest rather than an error. The rendered
/// fragment is identical to [`eval_hover_fragment`]'s (`value: \`50\` (@ t=…)` plus
/// the `(offline default — no scenario)` suffix for [`Provenance::OfflineDefault`]),
/// minus the channel-only `(externally driven)` suffix, which does not apply to an
/// expression value.
///
/// Caller's responsibility (a known limitation documented in the eval plan, E5): the
/// `byte_offset` keys are the evaluator's view of the **saved** script, so the caller
/// must only consult this when the open buffer is unmodified-since-load — an edited
/// buffer drifts the offsets and would mis-key. Channel hover ([`eval_hover_fragment`])
/// is path-keyed and unaffected.
pub fn eval_expr_fragment(
    file_name: &str,
    byte_offset: usize,
    trace: &Trace,
    provenance: &Provenance,
    tick: TickPolicy,
) -> Option<String> {
    let column = trace.exprs.get(&(file_name.to_string(), byte_offset))?;
    column_fragment(column, trace, provenance, tick)
}

/// Format a single trace value column (channel or expression) into the shared
/// `value: \`…\` (@ t=…s)` fragment plus the offline-default honesty suffix. Returns
/// `None` for an empty column (a key present but with no recorded ticks). The
/// channel-only `(externally driven)` suffix is appended by the caller, since it is
/// keyed on the channel path the column came from.
fn column_fragment(
    column: &[Value],
    trace: &Trace,
    provenance: &Provenance,
    tick: TickPolicy,
) -> Option<String> {
    if column.is_empty() {
        return None;
    }
    // Columns are aligned to the *end* of the shared time axis (a column first seen
    // mid-run is appended, not left-padded), so the column's last entry is the last
    // tick and its first entry sits `time.len() - column.len()` in.
    let (value, time_idx) = match tick {
        TickPolicy::First => (&column[0], trace.time.len().saturating_sub(column.len())),
        TickPolicy::Last => (
            column.last().expect("non-empty column"),
            trace.time.len().saturating_sub(1),
        ),
    };

    let mut frag = format!("\n\nvalue: `{}`", value_markdown(value));
    if let Some(t) = trace.time.get(time_idx) {
        frag.push_str(&format!(" (@ t={}s)", fmt_f64(*t)));
    }
    if *provenance == Provenance::OfflineDefault {
        frag.push_str(" (offline default — no scenario)");
    }
    Some(frag)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// A trace with a single channel column over a two-tick axis.
    fn trace_with(path: &str, col: Vec<Value>) -> Trace {
        let mut tr = Trace::new();
        for (i, v) in col.into_iter().enumerate() {
            tr.push_tick(i as f64 * 0.01);
            tr.record_channel(path, v);
        }
        tr
    }

    /// A trace with a single expression column at `(file, offset)` over a
    /// matching-length time axis.
    fn trace_with_expr(file: &str, offset: usize, col: Vec<Value>) -> Trace {
        let mut tr = Trace::new();
        for (i, v) in col.into_iter().enumerate() {
            tr.push_tick(i as f64 * 0.01);
            tr.record_expr((file.to_string(), offset), v);
        }
        tr
    }

    // ---- value_markdown ----

    #[test]
    fn integral_float_renders_without_trailing_decimal() {
        assert_eq!(value_markdown(&Value::Float(50.0)), "50");
    }

    #[test]
    fn fractional_float_renders_compactly() {
        assert_eq!(value_markdown(&Value::Float(2.5)), "2.5");
    }

    #[test]
    fn int_uint_bool_render_plainly() {
        assert_eq!(value_markdown(&Value::Int(-3)), "-3");
        assert_eq!(value_markdown(&Value::Uint(7)), "7");
        assert_eq!(value_markdown(&Value::Bool(true)), "true");
        assert_eq!(value_markdown(&Value::Bool(false)), "false");
    }

    #[test]
    fn enum_renders_member_name_not_id() {
        let v = Value::Enum {
            id: 3,
            member: "Driving".into(),
        };
        assert_eq!(value_markdown(&v), "Driving");
    }

    #[test]
    fn string_renders_verbatim() {
        assert_eq!(value_markdown(&Value::Str("hello".into())), "hello");
    }

    // ---- eval_hover_fragment ----

    #[test]
    fn scenario_channel_shows_value_and_time() {
        let tr = trace_with("Root.Demo.Output", vec![Value::Float(50.0); 3]);
        let frag = eval_hover_fragment(
            "Root.Demo.Output",
            &tr,
            &Provenance::Scenario(PathBuf::from("s.toml")),
            TickPolicy::Last,
        )
        .expect("a channel with a column yields a fragment");
        assert!(frag.contains("value: `50`"), "got: {frag}");
        // Last tick of a 3-tick run at 0.01s spacing is t=0.02s.
        assert!(frag.contains("(@ t=0.02s)"), "got: {frag}");
        // A configured scenario carries no honesty suffix.
        assert!(!frag.contains("offline default"), "got: {frag}");
        assert!(!frag.contains("externally driven"), "got: {frag}");
        // Leads with a blank line so it appends after the symbol markdown.
        assert!(frag.starts_with("\n\n"), "got: {frag:?}");
    }

    #[test]
    fn offline_default_value_is_labelled() {
        let tr = trace_with("Root.Demo.Output", vec![Value::Float(50.0)]);
        let frag = eval_hover_fragment(
            "Root.Demo.Output",
            &tr,
            &Provenance::OfflineDefault,
            TickPolicy::Last,
        )
        .expect("fragment present");
        assert!(frag.contains("value: `50`"), "got: {frag}");
        assert!(
            frag.contains("(offline default — no scenario)"),
            "offline default must be labelled: {frag}"
        );
    }

    #[test]
    fn external_channel_is_labelled() {
        let mut tr = trace_with("Root.Demo.CanIn", vec![Value::Int(1)]);
        tr.mark_external("Root.Demo.CanIn");
        let frag = eval_hover_fragment(
            "Root.Demo.CanIn",
            &tr,
            &Provenance::Scenario(PathBuf::from("s.toml")),
            TickPolicy::Last,
        )
        .expect("fragment present");
        assert!(
            frag.contains("(externally driven)"),
            "external channel must be labelled: {frag}"
        );
    }

    #[test]
    fn offline_and_external_show_both_suffixes() {
        let mut tr = trace_with("Root.Demo.CanIn", vec![Value::Int(1)]);
        tr.mark_external("Root.Demo.CanIn");
        let frag = eval_hover_fragment(
            "Root.Demo.CanIn",
            &tr,
            &Provenance::OfflineDefault,
            TickPolicy::Last,
        )
        .unwrap();
        assert!(
            frag.contains("(offline default — no scenario)"),
            "got: {frag}"
        );
        assert!(frag.contains("(externally driven)"), "got: {frag}");
    }

    #[test]
    fn missing_channel_yields_none() {
        let tr = trace_with("Root.Demo.Output", vec![Value::Float(50.0)]);
        assert!(
            eval_hover_fragment(
                "Root.Demo.Nope",
                &tr,
                &Provenance::OfflineDefault,
                TickPolicy::Last
            )
            .is_none(),
            "a path with no column gets no value line"
        );
    }

    #[test]
    fn empty_column_yields_none() {
        // A channel key present but with no recorded ticks renders nothing.
        let mut tr = Trace::new();
        tr.channels.insert("Root.Demo.Output".into(), Vec::new());
        assert!(
            eval_hover_fragment(
                "Root.Demo.Output",
                &tr,
                &Provenance::OfflineDefault,
                TickPolicy::Last
            )
            .is_none()
        );
    }

    #[test]
    fn first_tick_policy_reads_first_value_and_time() {
        let tr = trace_with(
            "Root.Demo.Output",
            vec![Value::Float(10.0), Value::Float(20.0), Value::Float(30.0)],
        );
        let frag = eval_hover_fragment(
            "Root.Demo.Output",
            &tr,
            &Provenance::Scenario(PathBuf::from("s.toml")),
            TickPolicy::First,
        )
        .unwrap();
        assert!(frag.contains("value: `10`"), "first tick value: {frag}");
        assert!(frag.contains("(@ t=0s)"), "first tick time: {frag}");
    }

    // ---- eval_expr_fragment (E5) ----

    #[test]
    fn present_expr_site_renders_value_and_time() {
        let tr = trace_with_expr("demo.m1scr", 42, vec![Value::Int(7); 3]);
        let frag = eval_expr_fragment(
            "demo.m1scr",
            42,
            &tr,
            &Provenance::Scenario(PathBuf::from("s.toml")),
            TickPolicy::Last,
        )
        .expect("a recorded (script, offset) yields a fragment");
        assert!(frag.contains("value: `7`"), "got: {frag}");
        // Last tick of a 3-tick run at 0.01s spacing is t=0.02s.
        assert!(frag.contains("(@ t=0.02s)"), "got: {frag}");
        assert!(frag.starts_with("\n\n"), "got: {frag:?}");
    }

    #[test]
    fn absent_expr_site_yields_none() {
        // A sparse miss — the run never recorded this site — is honest, not an
        // error: no value line at all.
        let tr = trace_with_expr("demo.m1scr", 42, vec![Value::Int(7)]);
        assert!(
            eval_expr_fragment(
                "demo.m1scr",
                99,
                &tr,
                &Provenance::Scenario(PathBuf::from("s.toml")),
                TickPolicy::Last
            )
            .is_none(),
            "an offset with no recorded expr column gets no value line"
        );
        // A different file name with the same offset is also a miss.
        assert!(
            eval_expr_fragment(
                "other.m1scr",
                42,
                &tr,
                &Provenance::Scenario(PathBuf::from("s.toml")),
                TickPolicy::Last
            )
            .is_none(),
            "the key is (script, offset) — a different script does not match"
        );
    }

    #[test]
    fn offline_default_expr_is_labelled() {
        let tr = trace_with_expr("demo.m1scr", 42, vec![Value::Float(2.5)]);
        let frag = eval_expr_fragment(
            "demo.m1scr",
            42,
            &tr,
            &Provenance::OfflineDefault,
            TickPolicy::Last,
        )
        .expect("fragment present");
        assert!(frag.contains("value: `2.5`"), "got: {frag}");
        assert!(
            frag.contains("(offline default — no scenario)"),
            "offline-default expr value must be labelled: {frag}"
        );
    }

    #[test]
    fn expr_fragment_never_shows_externally_driven() {
        // `(externally driven)` is a channel-only concept; even if a channel of the
        // same trace is external, an expression fragment never carries the suffix.
        let mut tr = trace_with_expr("demo.m1scr", 42, vec![Value::Int(1)]);
        tr.mark_external("Root.Demo.CanIn");
        let frag = eval_expr_fragment(
            "demo.m1scr",
            42,
            &tr,
            &Provenance::Scenario(PathBuf::from("s.toml")),
            TickPolicy::Last,
        )
        .unwrap();
        assert!(
            !frag.contains("externally driven"),
            "expr fragment must not carry the channel-only suffix: {frag}"
        );
    }

    #[test]
    fn empty_expr_column_yields_none() {
        let mut tr = Trace::new();
        tr.exprs.insert(("demo.m1scr".to_string(), 42), Vec::new());
        assert!(
            eval_expr_fragment(
                "demo.m1scr",
                42,
                &tr,
                &Provenance::OfflineDefault,
                TickPolicy::Last
            )
            .is_none()
        );
    }
}
