//! textDocument/inlayHint: an inline `: Type` after each `local` declaration that
//! has no explicit `<Type>` annotation and whose type is known. Reuses the same
//! inference as hover (`locate::local_decl_type`), so the two always agree.
use crate::eval::Trace;
use crate::eval::config::TickPolicy;
use crate::eval::engine::Provenance;
use crate::eval::render::value_markdown;
use crate::features::hover::value_type_str;
use crate::features::locate::local_decl_type;
use crate::line_index::{LineIndex, PositionEncoding};
use m1_core::{Field, Kind, Node};
use m1_typecheck::types::ValueType;
use tower_lsp::lsp_types::{InlayHint, InlayHintKind, InlayHintLabel, Range};

/// The cached-evaluation view passed into [`inlay_hints_with_eval`]: a borrowed
/// [`Trace`], where it came from, and which tick to read. Bundled so the eval
/// inputs travel as one optional argument — `None` means "no value inlays", which
/// reproduces the pre-eval inlay output exactly. Mirrors `hover::EvalContext`.
#[derive(Clone, Copy)]
pub struct EvalInlayContext<'a> {
    /// The cached trace whose channel columns hold the evaluated values.
    pub trace: &'a Trace,
    /// Where the trace came from, so an offline-default value renders the muted
    /// marker rather than passing as a measured one.
    pub provenance: &'a Provenance,
    /// Which tick of the trace a value is read from.
    pub tick: TickPolicy,
}

/// Inline hints within `range`: `: Type` after unannotated `local`s, `paramName:`
/// at call-site arguments, and — when a `project` is loaded — `[unit]` after each
/// channel/parameter reference that carries a unit (#154).
///
/// This is the long-standing entry point every existing call site uses; it
/// delegates to [`inlay_hints_with_eval`] with no [`EvalInlayContext`], so its
/// output is byte-identical to before the eval integration (no `= value` hints).
pub fn inlay_hints(
    root: Node,
    range: Range,
    li: &LineIndex,
    enc: PositionEncoding,
    project: Option<&m1_typecheck::project::Project>,
    file_name: Option<&str>,
) -> Vec<InlayHint> {
    inlay_hints_with_eval(root, range, li, enc, project, file_name, None)
}

/// [`inlay_hints`] plus optional inline computed-value hints (E6).
///
/// Identical to [`inlay_hints`] except that, when an [`EvalInlayContext`] is
/// supplied and a project is loaded, each channel reference / assignment target
/// that resolves to a symbol with a column in the cached trace also gets a
/// trailing `= <value>` hint. With `eval == None` — the default — the output is
/// exactly what [`inlay_hints`] produces. The value hints are gated by the
/// backend on `EvalConfig.inlay_values` (off by default) *and* an available trace,
/// so a client that does not opt in sees today's behaviour.
pub fn inlay_hints_with_eval(
    root: Node,
    range: Range,
    li: &LineIndex,
    enc: PositionEncoding,
    project: Option<&m1_typecheck::project::Project>,
    file_name: Option<&str>,
    eval: Option<EvalInlayContext<'_>>,
) -> Vec<InlayHint> {
    let mut out = Vec::new();
    // One scope (project + group + declaration-order locals) drives the
    // local-type, unit and value hints; with no project it still resolves
    // literal and local-to-local-copy types (#153).
    let scope = crate::features::locate::build_scope(root, project, file_name);
    collect(root, &range, li, enc, &scope, &mut out);
    if project.is_some() {
        collect_unit_hints(root, &range, li, enc, &scope, &mut out);
        if let Some(ctx) = eval {
            collect_value_hints(root, &range, li, enc, &scope, &ctx, &mut out);
        }
    }
    out
}

/// `[unit]` hints after each channel/parameter reference that carries a unit. Only
/// outermost dotted references (a `MemberExpression` not nested in another and not
/// a call callee) are considered, so we don't double-hint sub-paths or label a
/// `Foo.Bar(…)` call name.
fn collect_unit_hints(
    root: Node,
    range: &Range,
    li: &LineIndex,
    enc: PositionEncoding,
    scope: &m1_typecheck::resolve::Scope,
    out: &mut Vec<InlayHint>,
) {
    use m1_typecheck::resolve::{Resolution, resolve};
    for n in root.descendants() {
        if n.kind() != Kind::MemberExpression {
            continue;
        }
        if let Some(parent) = n.parent() {
            // Sub-path of a larger reference, or the callee of a call → skip.
            if parent.kind() == Kind::MemberExpression {
                continue;
            }
            if parent.kind() == Kind::CallExpression
                && parent
                    .child_by_field(Field::Function)
                    .map(|f| f.byte_range())
                    == Some(n.byte_range())
            {
                continue;
            }
        }
        if let Resolution::Symbol(sym) = resolve(n.text(), scope)
            && let Some(unit) = &sym.unit
        {
            let position = li.position(n.byte_range().end, enc);
            if position.line < range.start.line || position.line > range.end.line {
                continue;
            }
            out.push(InlayHint {
                position,
                label: InlayHintLabel::String(format!("[{unit}]")),
                kind: Some(InlayHintKind::TYPE),
                text_edits: None,
                tooltip: None,
                padding_left: Some(true),
                padding_right: Some(false),
                data: None,
            });
        }
    }
}

/// `= <value>` hints after each channel reference / assignment target that
/// resolves to a project symbol with a column in the cached [`Trace`] (E6). Reuses
/// the same outermost-path traversal as the unit hints
/// ([`crate::features::locate::for_each_top_path`]) so reads (`Demo.Output`) and
/// assignment targets (`Output = …`) are both covered, including bare identifiers
/// the unit-hint pass skips.
///
/// A symbol with no trace column (a group/function/table/parameter the run
/// produced no value for) simply gets no hint — honest, not an error. An
/// [`Provenance::OfflineDefault`] value renders the muted `= <value>?` form with a
/// tooltip, so an inline number is never mistaken for a measured one.
fn collect_value_hints(
    root: Node,
    range: &Range,
    li: &LineIndex,
    enc: PositionEncoding,
    scope: &m1_typecheck::resolve::Scope,
    ctx: &EvalInlayContext<'_>,
    out: &mut Vec<InlayHint>,
) {
    use m1_typecheck::resolve::{Resolution, resolve};
    let offline = *ctx.provenance == Provenance::OfflineDefault;
    crate::features::locate::for_each_top_path(root, |n, _is_write| {
        let Resolution::Symbol(sym) = resolve(n.text(), scope) else {
            return;
        };
        let Some(column) = ctx.trace.channels.get(&sym.path) else {
            return;
        };
        // Columns are aligned to the *end* of the shared time axis; pick the
        // tick the policy asks for (the default last tick is a settled run's
        // converged value). An empty column yields no hint.
        let value = match ctx.tick {
            TickPolicy::First => column.first(),
            TickPolicy::Last => column.last(),
        };
        let Some(value) = value else {
            return;
        };
        let position = li.position(n.byte_range().end, enc);
        if position.line < range.start.line || position.line > range.end.line {
            return;
        }
        // An offline-default value is the evaluator's default world, not a
        // measured one — append `?` and explain it in a tooltip so the inline
        // number is never read as ground truth.
        let rendered = value_markdown(value);
        let label = if offline {
            format!("= {rendered}?")
        } else {
            format!("= {rendered}")
        };
        let tooltip = offline.then(|| {
            tower_lsp::lsp_types::InlayHintTooltip::String(
                "offline default — no scenario or log configured; this is the \
                 evaluator's default world, not a measured value"
                    .to_string(),
            )
        });
        out.push(InlayHint {
            position,
            label: InlayHintLabel::String(label),
            kind: Some(InlayHintKind::TYPE),
            text_edits: None,
            tooltip,
            padding_left: Some(true),
            padding_right: Some(false),
            data: None,
        });
    });
}

fn collect(
    root: Node,
    range: &Range,
    li: &LineIndex,
    enc: PositionEncoding,
    scope: &m1_typecheck::resolve::Scope,
    out: &mut Vec<InlayHint>,
) {
    // Iterate the tree with m1-core's explicit work-stack pre-order iterator
    // rather than recursion, so a pathologically deep document can't overflow the
    // call stack (#133). Same pre-order visit, same result.
    for n in root.descendants() {
        if n.kind() == Kind::CallExpression {
            collect_param_hints(n, range, li, enc, out);
            continue;
        }
        if n.kind() != Kind::LocalDeclaration {
            continue;
        }
        // Skip locals the author already annotated with `<Type>`.
        let annotated = n
            .named_children()
            .iter()
            .any(|c| c.kind() == Kind::TypeAnnotation);
        if !annotated && let Some(name) = n.child_by_field(Field::Name) {
            let t = local_decl_type(n, scope);
            if !matches!(t, ValueType::Unknown) {
                let position = li.position(name.byte_range().end, enc);
                if position.line >= range.start.line && position.line <= range.end.line {
                    out.push(InlayHint {
                        position,
                        label: InlayHintLabel::String(format!(": {}", value_type_str(t))),
                        kind: Some(InlayHintKind::TYPE),
                        text_edits: None,
                        tooltip: None,
                        padding_left: Some(false),
                        padding_right: Some(false),
                        data: None,
                    });
                }
            }
        }
    }
}

/// `paramName:` hints before each argument of a library / object-method call,
/// drawn from the intrinsics model — the same param names signature help shows,
/// surfaced inline without opening the popup (#155). Project-independent: the
/// intrinsics are global.
fn collect_param_hints(
    call: Node,
    range: &Range,
    li: &LineIndex,
    enc: PositionEncoding,
    out: &mut Vec<InlayHint>,
) {
    let intr = m1_typecheck::intrinsics::get();
    let Some(callee) = call.child_by_field(Field::Function) else {
        return;
    };
    let path = callee.text();
    let (head, method) = path.rsplit_once('.').unwrap_or(("", path));
    // Candidate overloads: a library object's function, or a modelled object
    // method (`X.Lookup`).
    let overloads: Vec<&m1_typecheck::intrinsics::Overload> = match intr.library_object(head) {
        Some(obj) => obj.functions.iter().filter(|f| f.name == method).collect(),
        None => intr.object_method(method),
    };
    if overloads.is_empty() {
        return;
    }
    let Some(args_node) = call.child_by_field(Field::Arguments) else {
        return;
    };
    let arg_exprs: Vec<Node> = args_node
        .named_children()
        .into_iter()
        .filter(|c| is_arg_expr(c.kind()))
        .collect();
    // Pick the overload whose arity covers the call, else the widest.
    let Some(ov) = overloads
        .iter()
        .find(|o| o.params.len() >= arg_exprs.len())
        .or_else(|| overloads.iter().max_by_key(|o| o.params.len()))
    else {
        return;
    };
    for (i, arg) in arg_exprs.iter().enumerate() {
        let Some(p) = ov.params.get(i) else { break };
        let position = li.position(arg.byte_range().start, enc);
        if position.line < range.start.line || position.line > range.end.line {
            continue;
        }
        out.push(InlayHint {
            position,
            label: InlayHintLabel::String(format!("{}:", p.name)),
            kind: Some(InlayHintKind::PARAMETER),
            text_edits: None,
            tooltip: None,
            padding_left: Some(false),
            padding_right: Some(true),
            data: None,
        });
    }
}

fn is_arg_expr(k: Kind) -> bool {
    matches!(
        k,
        Kind::Identifier
            | Kind::MemberExpression
            | Kind::CallExpression
            | Kind::UnaryExpression
            | Kind::BinaryExpression
            | Kind::TernaryExpression
            | Kind::ParenthesizedExpression
            | Kind::Number
            | Kind::String
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use tower_lsp::lsp_types::Position;

    fn hints(src: &str) -> Vec<InlayHint> {
        let cst = m1_core::parse(src);
        let li = LineIndex::new(src);
        let full = Range::new(Position::new(0, 0), Position::new(10_000, 0));
        inlay_hints(cst.root(), full, &li, PositionEncoding::Utf16, None, None)
    }

    fn label(h: &InlayHint) -> String {
        match &h.label {
            InlayHintLabel::String(s) => s.clone(),
            _ => String::new(),
        }
    }

    #[test]
    fn hints_inferred_local_type() {
        let h = hints("local count = 0;\n");
        assert_eq!(h.len(), 1);
        assert_eq!(label(&h[0]), ": Integer");
        assert_eq!(h[0].kind, Some(InlayHintKind::TYPE));
    }

    #[test]
    fn local_to_local_copy_propagates_type_without_project() {
        // `copy` is initialised from another local; its type propagates even with
        // no project loaded (#153).
        let h = hints("local count = 0;\nlocal copy = count;\n");
        let labels: Vec<String> = h.iter().map(label).collect();
        assert!(
            labels.iter().filter(|l| *l == ": Integer").count() >= 2,
            "both the literal local and its copy should be hinted Integer: {labels:?}"
        );
    }

    #[test]
    fn channel_read_and_copy_infer_type_with_project() {
        use crate::project_store::ProjectStore;
        use std::io::Write;
        let tmp = tempfile::tempdir().unwrap();
        std::fs::File::create(tmp.path().join("Project.m1prj"))
            .unwrap()
            .write_all(
                br#"<?xml version="1.0"?>
<Project>
  <Component Classname="BuiltIn.GroupCompound" Name="Root"/>
  <Component Classname="BuiltIn.GroupCompound" Name="Root.Demo"/>
  <Component Classname="BuiltIn.Channel" Name="Root.Demo.Speed"><Props Qty="rad/s"/></Component>
</Project>"#,
            )
            .unwrap();
        let store = ProjectStore::new();
        store.discover_and_load(tmp.path()).unwrap();
        // speed = channel read (Float); copy = local-to-local copy (Float).
        let src = "local speed = Demo.Speed;\nlocal copy = speed;\n";
        let cst = m1_core::parse(src);
        let li = LineIndex::new(src);
        let full = Range::new(Position::new(0, 0), Position::new(10_000, 0));
        store.with_project(|p| {
            let hs = inlay_hints(
                cst.root(),
                full,
                &li,
                PositionEncoding::Utf16,
                p.map(|lp| &lp.project),
                Some("Demo.Update.m1scr"),
            );
            let type_labels: Vec<String> = hs
                .iter()
                .filter(|h| h.kind == Some(InlayHintKind::TYPE))
                .map(label)
                .collect();
            assert!(
                type_labels.iter().filter(|l| *l == ": Float").count() >= 2,
                "channel-read local and its copy should both hint Float: {type_labels:?}"
            );
        });
    }

    #[test]
    fn unit_hints_at_channel_references() {
        use crate::project_store::ProjectStore;
        use std::io::Write;
        let tmp = tempfile::tempdir().unwrap();
        let prj = tmp.path().join("Project.m1prj");
        std::fs::File::create(&prj)
            .unwrap()
            .write_all(
                br#"<?xml version="1.0"?>
<Project>
  <Component Classname="BuiltIn.GroupCompound" Name="Root"/>
  <Component Classname="BuiltIn.GroupCompound" Name="Root.Demo"/>
  <Component Classname="BuiltIn.Channel" Name="Root.Demo.Speed"><Props Qty="rad/s"/></Component>
</Project>"#,
            )
            .unwrap();
        let store = ProjectStore::new();
        store.discover_and_load(tmp.path()).unwrap();
        let src = "x = Demo.Speed + 1;\n";
        let cst = m1_core::parse(src);
        let li = LineIndex::new(src);
        let full = Range::new(Position::new(0, 0), Position::new(10_000, 0));
        store.with_project(|p| {
            let hs = inlay_hints(
                cst.root(),
                full,
                &li,
                PositionEncoding::Utf16,
                p.map(|lp| &lp.project),
                Some("X.m1scr"),
            );
            // base unit of rad/s is deg/s per the manual's angle exception.
            assert!(
                hs.iter()
                    .any(|h| label(h).contains('[') && label(h).contains("/s")),
                "expected a unit hint at the channel reference: {:?}",
                hs.iter().map(label).collect::<Vec<_>>()
            );
        });
    }

    #[test]
    fn param_name_hints_at_library_call_args() {
        // #155: a library call shows `paramName:` before each argument, from the
        // intrinsics model (no project needed).
        let h = hints("x = Calculate.Max(a, b);\n");
        let params: Vec<String> = h
            .iter()
            .filter(|h| h.kind == Some(InlayHintKind::PARAMETER))
            .map(label)
            .collect();
        assert!(
            params.iter().any(|p| p.ends_with(':')),
            "expected `name:` parameter hints, got {params:?}"
        );
        assert_eq!(params.len(), 2, "one hint per argument: {params:?}");
    }

    #[test]
    fn hint_follows_initializer_not_name_prefix() {
        // `fGain` would once have been hinted Float by its name prefix; the
        // initializer (Integer) is now authoritative.
        let h = hints("local fGain = 0;\n");
        assert_eq!(label(&h[0]), ": Integer");
    }

    #[test]
    fn no_hint_when_explicitly_annotated() {
        let h = hints("local <Float> fGain = 1.0;\n");
        assert!(
            h.is_empty(),
            "explicitly-annotated local should get no hint"
        );
    }

    #[test]
    fn no_hint_for_unknown_type() {
        let h = hints("local thing = Something.Else;\n");
        assert!(h.is_empty());
    }

    #[test]
    fn hint_position_is_after_the_name() {
        // `local count` -> name ends at column 11 (after "count")
        let h = hints("local count = 0;\n");
        assert_eq!(h[0].position.line, 0);
        assert_eq!(h[0].position.character, 11);
    }

    // ---- E6: inline computed-value inlay hints ----

    use crate::eval::config::TickPolicy;
    use crate::eval::engine::Provenance;
    use crate::eval::{Trace, Value};

    /// A project fixture with a value-bearing channel (`Root.Demo.Output`) whose
    /// owning function maps to `Demo.Update.m1scr`, so a bare `Output` write in
    /// that script resolves to `Root.Demo.Output`.
    fn eval_project() -> (tempfile::TempDir, crate::project_store::ProjectStore) {
        use crate::project_store::ProjectStore;
        use std::io::Write;
        let tmp = tempfile::tempdir().unwrap();
        std::fs::File::create(tmp.path().join("Project.m1prj"))
            .unwrap()
            .write_all(
                br#"<?xml version="1.0"?>
<Project>
  <Component Classname="BuiltIn.GroupCompound" Name="Root"/>
  <Component Classname="BuiltIn.GroupCompound" Name="Root.Demo"/>
  <Component Classname="BuiltIn.Channel" Name="Root.Demo.Output"><Props Type="f32" Qty="V"/></Component>
  <Component Classname="BuiltIn.FuncUser" Name="Root.Demo.Update" Filename="Demo.Update.m1scr"/>
</Project>"#,
            )
            .unwrap();
        let store = ProjectStore::new();
        store.discover_and_load(tmp.path()).unwrap();
        (tmp, store)
    }

    /// A one-channel trace at a single tick.
    fn trace_for(path: &str, value: Value) -> Trace {
        let mut tr = Trace::new();
        tr.push_tick(0.02);
        tr.record_channel(path, value);
        tr
    }

    /// Inlay hints with an eval context over the `Demo.Update.m1scr` script.
    fn eval_hints(
        store: &crate::project_store::ProjectStore,
        src: &str,
        ctx: Option<EvalInlayContext<'_>>,
    ) -> Vec<InlayHint> {
        let cst = m1_core::parse(src);
        let li = LineIndex::new(src);
        let full = Range::new(Position::new(0, 0), Position::new(10_000, 0));
        store.with_project(|p| {
            inlay_hints_with_eval(
                cst.root(),
                full,
                &li,
                PositionEncoding::Utf16,
                p.map(|lp| &lp.project),
                Some("Demo.Update.m1scr"),
                ctx,
            )
        })
    }

    /// With a scenario trace and value-inlays on, an assignment target gets a
    /// trailing `= value` hint.
    #[test]
    fn value_hint_on_assignment_target_with_scenario() {
        let (_tmp, store) = eval_project();
        let trace = trace_for("Root.Demo.Output", Value::Float(50.0));
        let prov = Provenance::Scenario(std::path::PathBuf::from("idle.toml"));
        let hs = eval_hints(
            &store,
            "Output = 1;\n",
            Some(EvalInlayContext {
                trace: &trace,
                provenance: &prov,
                tick: TickPolicy::Last,
            }),
        );
        let labels: Vec<String> = hs.iter().map(label).collect();
        assert!(
            labels.iter().any(|l| l == "= 50"),
            "expected a `= 50` value hint on the assignment target: {labels:?}"
        );
        // A configured scenario carries no muted marker.
        assert!(
            !labels.iter().any(|l| l.contains('?')),
            "scenario value must not be muted: {labels:?}"
        );
    }

    /// The default path (no eval context) emits only the existing type/unit/param
    /// hints — never a value hint.
    #[test]
    fn no_value_hints_without_eval_context() {
        let (_tmp, store) = eval_project();
        let hs = eval_hints(&store, "Output = 1;\n", None);
        let labels: Vec<String> = hs.iter().map(label).collect();
        assert!(
            !labels.iter().any(|l| l.starts_with("= ")),
            "no value hints when eval is off: {labels:?}"
        );
    }

    /// The plain `inlay_hints` entry point never emits value hints — it is the
    /// pre-eval surface, byte-identical to before.
    #[test]
    fn plain_inlay_hints_never_emits_value_hints() {
        let (_tmp, store) = eval_project();
        let cst = m1_core::parse("Output = 1;\n");
        let li = LineIndex::new("Output = 1;\n");
        let full = Range::new(Position::new(0, 0), Position::new(10_000, 0));
        let hs = store.with_project(|p| {
            inlay_hints(
                cst.root(),
                full,
                &li,
                PositionEncoding::Utf16,
                p.map(|lp| &lp.project),
                Some("Demo.Update.m1scr"),
            )
        });
        let labels: Vec<String> = hs.iter().map(label).collect();
        assert!(
            !labels.iter().any(|l| l.starts_with("= ")),
            "the plain entry point never adds value hints: {labels:?}"
        );
    }

    /// An offline-default value renders a muted marker so an inline number is
    /// never mistaken for a measured one.
    #[test]
    fn offline_default_value_hint_is_muted() {
        let (_tmp, store) = eval_project();
        let trace = trace_for("Root.Demo.Output", Value::Float(50.0));
        let prov = Provenance::OfflineDefault;
        let hs = eval_hints(
            &store,
            "Output = 1;\n",
            Some(EvalInlayContext {
                trace: &trace,
                provenance: &prov,
                tick: TickPolicy::Last,
            }),
        );
        let labels: Vec<String> = hs.iter().map(label).collect();
        assert!(
            labels.iter().any(|l| l == "= 50?"),
            "offline-default value must render the muted `= 50?` form: {labels:?}"
        );
        // The muted hint carries a tooltip explaining the marker.
        let muted = hs
            .iter()
            .find(|h| label(h) == "= 50?")
            .expect("muted hint present");
        assert!(
            muted.tooltip.is_some(),
            "muted value hint should carry an explanatory tooltip"
        );
    }

    /// A value hint also lands on a channel *read* (a `MemberExpression`), not
    /// only an assignment target.
    #[test]
    fn value_hint_on_channel_read() {
        let (_tmp, store) = eval_project();
        let trace = trace_for("Root.Demo.Output", Value::Float(50.0));
        let prov = Provenance::Scenario(std::path::PathBuf::from("idle.toml"));
        let hs = eval_hints(
            &store,
            "x = Demo.Output + 1;\n",
            Some(EvalInlayContext {
                trace: &trace,
                provenance: &prov,
                tick: TickPolicy::Last,
            }),
        );
        let labels: Vec<String> = hs.iter().map(label).collect();
        assert!(
            labels.iter().any(|l| l == "= 50"),
            "expected a `= 50` value hint at the channel read: {labels:?}"
        );
    }

    /// A symbol with no column in the trace (here a different channel) gets no
    /// value hint — honest, not an error.
    #[test]
    fn no_value_hint_when_channel_absent_from_trace() {
        let (_tmp, store) = eval_project();
        // The trace only carries some *other* channel, not Root.Demo.Output.
        let trace = trace_for("Root.Demo.Elsewhere", Value::Float(50.0));
        let prov = Provenance::Scenario(std::path::PathBuf::from("idle.toml"));
        let hs = eval_hints(
            &store,
            "Output = 1;\n",
            Some(EvalInlayContext {
                trace: &trace,
                provenance: &prov,
                tick: TickPolicy::Last,
            }),
        );
        let labels: Vec<String> = hs.iter().map(label).collect();
        assert!(
            !labels.iter().any(|l| l.starts_with("= ")),
            "no value hint for a channel absent from the trace: {labels:?}"
        );
    }

    /// The existing unit/type hints are unchanged when an eval context is added —
    /// value hints are purely additive.
    #[test]
    fn value_hints_are_additive_to_existing_hints() {
        let (_tmp, store) = eval_project();
        let trace = trace_for("Root.Demo.Output", Value::Float(50.0));
        let prov = Provenance::Scenario(std::path::PathBuf::from("idle.toml"));
        // `local v = Demo.Output;` produces a `: Float` type hint and a `[V]` unit
        // hint already; the eval context adds a `= 50` value hint on the read.
        let hs = eval_hints(
            &store,
            "local v = Demo.Output;\n",
            Some(EvalInlayContext {
                trace: &trace,
                provenance: &prov,
                tick: TickPolicy::Last,
            }),
        );
        let labels: Vec<String> = hs.iter().map(label).collect();
        assert!(
            labels.iter().any(|l| l.starts_with(": ")),
            "the local-type hint is still present: {labels:?}"
        );
        assert!(
            labels.iter().any(|l| l.contains("[V]")),
            "the unit hint is still present: {labels:?}"
        );
        assert!(
            labels.iter().any(|l| l == "= 50"),
            "the value hint is added: {labels:?}"
        );
    }
}
