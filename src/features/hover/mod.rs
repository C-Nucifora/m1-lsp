//! textDocument/hover: describe the symbol/local/opaque under the cursor.
//!
//! This module is the composition seam: [`hover_with_eval`] resolves the segment
//! under the cursor and delegates the actual rendering to focused submodules —
//! [`symbol`] (project symbols, types, units, table/DBC layout), [`enums`]
//! (enum literals), [`intrinsics`] (library objects/functions/methods), and
//! [`keywords`] (language/type/reference keyword docs). The evaluated-value
//! fragments come from [`crate::eval::render`].
mod enums;
mod intrinsics;
mod keywords;
mod symbol;

use crate::convert::range;
use crate::eval::Trace;
use crate::eval::config::TickPolicy;
use crate::eval::engine::Provenance;
use crate::eval::render::{eval_expr_fragment, eval_hover_fragment};
use crate::features::locate::{
    build_scope, node_at_byte, path_at_byte, segment_at_byte, segment_nodes,
};
use crate::line_index::{LineIndex, PositionEncoding};
use m1_core::Kind;
use m1_typecheck::project::Project;
use m1_typecheck::resolve::{Resolution, resolve};
use tower_lsp::lsp_types::{Hover, HoverContents, MarkupContent, MarkupKind};

use enums::{enum_literal_head_markdown, enum_member_markdown, enum_type_markdown};
use intrinsics::{builtin_fn_markdown, builtin_object_markdown, object_method_markdown};
use keywords::{language_keyword_doc, primitive_type_doc, reference_keyword_doc};

// Re-exported for the sibling features that render the same symbol fragments:
// `completion` (symbol_markdown / value_type_str) and `inlay` (value_type_str).
pub(crate) use symbol::{symbol_markdown, value_type_str};

/// The cached-evaluation view passed into [`hover_with_eval`]: a borrowed
/// [`Trace`], where it came from, and which tick to read. Bundled so the eval
/// inputs travel as one optional argument — `None` means "no eval available",
/// which reproduces the pre-eval hover exactly.
#[derive(Clone, Copy)]
pub struct EvalContext<'a> {
    /// The cached trace whose channel/expression columns hold the evaluated values.
    pub trace: &'a Trace,
    /// Where the trace came from, so the value line can be honest (offline
    /// default vs. configured scenario/log).
    pub provenance: &'a Provenance,
    /// Which tick of the trace a value is read from.
    pub tick: TickPolicy,
    /// Whether expression-level hover (E5) may key into [`Trace::exprs`] by byte
    /// offset. [`Trace::exprs`] offsets are the evaluator's view of the **saved**
    /// script, so they only line up with the open buffer when it is
    /// unmodified-since-load; the backend sets this `false` once the buffer is
    /// edited (a known limitation, documented in the eval plan). Channel hover
    /// (E4) is path-keyed and unaffected by this flag.
    pub expr_offsets_valid: bool,
}

/// textDocument/hover entry point — unchanged behaviour, no evaluated values.
///
/// This is the long-standing signature every existing call site uses; it simply
/// delegates to [`hover_with_eval`] with no [`EvalContext`], so its output is
/// byte-identical to before the eval integration. The backend uses
/// [`hover_with_eval`] when a cached trace is available.
pub fn hover(
    root: m1_core::Node,
    byte: usize,
    project: Option<&Project>,
    file_name: Option<&str>,
    li: &LineIndex,
    enc: PositionEncoding,
) -> Option<Hover> {
    hover_with_eval(root, byte, project, file_name, li, enc, None)
}

/// textDocument/hover with an optional cached-evaluation view (E4).
///
/// Identical to [`hover`] in every branch except the project-symbol one: when an
/// [`EvalContext`] is supplied and the resolved symbol has a column in the cached
/// trace, an evaluated-value fragment (`value: \`50\` (@ t=…)`, with honest
/// provenance suffixes) is appended after the existing type/symbol markdown. With
/// `eval == None` — or for a symbol with no trace column (group/function/table) —
/// the output is exactly what [`hover`] produces.
pub fn hover_with_eval(
    root: m1_core::Node,
    byte: usize,
    project: Option<&Project>,
    file_name: Option<&str>,
    li: &LineIndex,
    enc: PositionEncoding,
    eval: Option<EvalContext<'_>>,
) -> Option<Hover> {
    // Language-keyword/construct docs (#166). Keyword tokens (`if`, `when`,
    // `expand`, `local`, …) are not part of a dotted path, so `path_at_byte`
    // below would miss them; handle them up front.
    if let Some(node) = node_at_byte(root, byte)
        && let Some(doc) = language_keyword_doc(node.kind())
    {
        return Some(Hover {
            contents: HoverContents::Markup(MarkupContent {
                kind: MarkupKind::Markdown,
                value: doc.to_string(),
            }),
            range: Some(range(&node.byte_range(), li, enc)),
        });
    }

    let (top, _full) = path_at_byte(root, byte)?;
    let segs = segment_nodes(top);
    if segs.is_empty() {
        return None;
    }
    let i = segment_at_byte(top, byte).unwrap_or(segs.len() - 1);
    let scope = build_scope(root, project, file_name);

    // The dotted prefix up to and including the segment under the cursor.
    let prefix = segs[..=i]
        .iter()
        .map(|n| n.text())
        .collect::<Vec<_>>()
        .join(".");
    let seg = segs[i];
    let seg_text = seg.text();

    // Enum-literal head (`ASSI` in `ASSI.Driving`) must win over plain resolution:
    // the enum type often shares its name with a group/channel, so resolving the
    // segment alone would describe that shadowing symbol. Decided by the following
    // member segment, so it never misfires on a real group-relative path.
    // Primitive type name inside a `<…>` annotation (#164): `local <Integer>`.
    // The type name resolves as an opaque project path and would hover as "type
    // not modelled"; describe the primitive instead. A non-primitive annotation
    // (an enum type) returns None here and falls through to the enum handling.
    // Tracks whether the channel-value enrichment (E4) already appended a `value:`
    // line, so the expression-level fallback (E5) below does not add a second one
    // when a segment resolves to a channel that also has a column.
    let mut channel_value_shown = false;
    let md = if seg.parent().map(|p| p.kind()) == Some(Kind::TypeAnnotation)
        && let Some(doc) = primitive_type_doc(seg_text)
    {
        doc.to_string()
    }
    // Reference/scope keyword at the head of the reference (#167): `Root`,
    // `Parent`, `This`, `In`, `Out`, `Library`. These resolve to unhelpful or
    // misleading hovers (Root → "group / Unknown", Parent → the parent group's
    // own hover), so when the cursor is on the anchor itself, describe the
    // keyword's meaning instead.
    else if i == 0
        && let Some(doc) = reference_keyword_doc(seg_text)
    {
        doc.to_string()
    } else if let Some(md) = enum_literal_head_markdown(i, &segs, project, &scope) {
        md
    } else {
        match resolve(&prefix, &scope) {
            Resolution::Local(t) => {
                format!("**{prefix}** `local`\n\ntype: `{}`", value_type_str(t))
            }
            Resolution::Symbol(sym) => {
                // Type/symbol info is unchanged and always shown first; the
                // evaluated value (when a trace is available and the symbol has a
                // column under its canonical path) is appended after it.
                let mut md = symbol_markdown(sym, project);
                if let Some(ctx) = eval
                    && let Some(frag) =
                        eval_hover_fragment(&sym.path, ctx.trace, ctx.provenance, ctx.tick)
                {
                    md.push_str(&frag);
                    channel_value_shown = true;
                }
                md
            }
            Resolution::BuiltinObject(name) => builtin_object_markdown(name),
            Resolution::BuiltinFn(overloads) => builtin_fn_markdown(&prefix, &overloads),
            Resolution::Opaque | Resolution::Unresolved => {
                // A trailing accessor (`object.AsInteger`) doesn't resolve to a
                // project symbol, but the object on its left does. Describe the
                // built-in method itself, with the manual's docs.
                let methods = m1_typecheck::intrinsics::get().object_method(seg_text);
                let object_resolves = i > 0
                    && matches!(
                        resolve(
                            &segs[..i]
                                .iter()
                                .map(|n| n.text())
                                .collect::<Vec<_>>()
                                .join("."),
                            &scope
                        ),
                        Resolution::Symbol(_)
                            | Resolution::Opaque
                            | Resolution::BuiltinObject(_)
                            | Resolution::Local(_)
                    );
                if object_resolves && !methods.is_empty() {
                    object_method_markdown(seg_text, &methods)
                } else if let Some(md) = enum_member_markdown(seg, i, &segs, project) {
                    // An enum-member token (`Drive State.Off`, or a bare `Off`) — the
                    // project model defines the enum + member, so describe it rather
                    // than fall through to "type not modelled" (#127).
                    md
                } else if let Some(md) = enum_type_markdown(seg_text, project) {
                    // The head of an `EnumName.Member` literal (e.g. `ASSI` in
                    // `ASSI.Driving`): the enum type itself. Describe the enum rather
                    // than fall through to "type not modelled".
                    md
                } else if matches!(resolve(&prefix, &scope), Resolution::Opaque) {
                    format!("**{prefix}**\n\nbuilt-in symbol — type not modelled")
                } else {
                    return None;
                }
            }
        }
    };

    // Expression-level hover (E5): when the hovered segment is an expression
    // occurrence rather than a channel that already carried a value, look up its
    // per-node value in `Trace::exprs`, keyed by `(script_name, byte_offset)`. The
    // sink is sparse — a segment the run never evaluated simply yields no value
    // line (honest, not an error), leaving the rest of the hover unchanged.
    //
    // Gated on `expr_offsets_valid`: the offsets are the evaluator's view of the
    // *saved* script, so they only line up with the open buffer when it is
    // unmodified-since-load (a known limitation). Channel hover above is path-keyed
    // and unaffected.
    let mut md = md;
    if let Some(ctx) = eval
        && ctx.expr_offsets_valid
        && !channel_value_shown
        && let Some(name) = file_name
        && let Some(frag) = eval_expr_fragment(
            name,
            seg.byte_range().start,
            ctx.trace,
            ctx.provenance,
            ctx.tick,
        )
    {
        md.push_str(&frag);
    }

    Some(Hover {
        contents: HoverContents::Markup(MarkupContent {
            kind: MarkupKind::Markdown,
            value: md,
        }),
        // Highlight just the hovered segment, not the whole expression.
        range: Some(range(&seg.byte_range(), li, enc)),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Hover markdown at the first occurrence of `find`, with no project loaded.
    fn kw_hover(src: &str, find: &str) -> Option<String> {
        let cst = m1_core::parse(src);
        let li = LineIndex::new(src);
        let byte = src.find(find).unwrap();
        hover(cst.root(), byte, None, None, &li, PositionEncoding::Utf16).map(|h| {
            match h.contents {
                HoverContents::Markup(m) => m.value,
                _ => String::new(),
            }
        })
    }

    #[test]
    fn primitive_type_name_in_annotation_has_doc_not_unmodelled() {
        // #164: the type name inside `local <Integer>` resolved as an opaque path
        // and hovered as "built-in symbol — type not modelled". It is a primitive
        // type; describe it.
        let md = kw_hover("local <Integer> myValue = 0;\n", "Integer").unwrap();
        assert!(md.contains("primitive type"), "got: {md}");
        assert!(
            !md.contains("not modelled"),
            "should not show the unmodelled fallback: {md}"
        );
    }

    #[test]
    fn multiword_primitive_type_name_has_doc() {
        let md = kw_hover("local <Floating Point> r = 0.0;\n", "Floating Point").unwrap();
        assert!(md.to_lowercase().contains("floating point"), "got: {md}");
        assert!(md.contains("primitive type"), "got: {md}");
    }

    #[test]
    fn unsigned_integer_primitive_has_doc() {
        let md = kw_hover("local <Unsigned Integer> u = 0;\n", "Unsigned Integer").unwrap();
        assert!(md.contains("primitive type"), "got: {md}");
        assert!(md.to_lowercase().contains("non-negative"), "got: {md}");
    }

    #[test]
    fn language_keyword_local_has_doc() {
        // #166: a keyword token is not part of a path, so hover used to return null.
        let md = kw_hover("local x = 1;\n", "local").expect("local should have a doc");
        assert!(md.contains("local variable"), "got: {md}");
        assert!(md.to_lowercase().contains("function"), "got: {md}");
    }

    #[test]
    fn language_keyword_when_explains_enum_match() {
        let md = kw_hover("when (Mode)\n{\nis (Red)\n{\n}\n}\n", "when").unwrap();
        assert!(md.contains("when"), "got: {md}");
        assert!(md.to_lowercase().contains("match"), "got: {md}");
    }

    #[test]
    fn language_keyword_expand_explains_compile_time_unroll() {
        let md = kw_hover("expand (i = 1 to 3)\n{\n}\n", "expand").unwrap();
        assert!(md.to_lowercase().contains("compile"), "got: {md}");
    }

    #[test]
    fn language_keyword_static_explains_persistence() {
        let md = kw_hover("static local x = 1;\n", "static").unwrap();
        assert!(
            md.to_lowercase().contains("across executions") || md.to_lowercase().contains("retain"),
            "got: {md}"
        );
    }

    #[test]
    fn reference_keyword_root_has_doc() {
        // #167: `Root` used to show only "group / type: Unknown".
        let md = kw_hover("Root.Demo.X = 0;\n", "Root").unwrap();
        assert!(md.to_lowercase().contains("root group"), "got: {md}");
    }

    #[test]
    fn reference_keyword_in_explains_input_args() {
        let md = kw_hover("In.Widget Count = 0;\n", "In").unwrap();
        assert!(md.to_lowercase().contains("input"), "got: {md}");
    }

    #[test]
    fn reference_keyword_parent_explains_container() {
        let md = kw_hover("Parent.X = 0;\n", "Parent").unwrap();
        assert!(
            md.to_lowercase().contains("containing") || md.to_lowercase().contains("parent"),
            "got: {md}"
        );
    }

    #[test]
    fn reference_keyword_doc_only_at_head_not_on_trailing_member() {
        // Hovering `Demo` (a real-ish group segment, not the anchor) must NOT
        // produce the Root keyword doc — the ref-keyword doc is head-only.
        let md = kw_hover("Root.Demo.X = 0;\n", "Demo");
        assert!(
            md.as_deref()
                .map(|m| !m.contains("root group"))
                .unwrap_or(true),
            "trailing segment wrongly got the Root doc: {md:?}"
        );
    }

    #[test]
    fn hovers_local_with_inferred_type() {
        let src = "local fGain = 1.0;\nfGain = fGain + 1.0;\n";
        let cst = m1_core::parse(src);
        let li = LineIndex::new(src);
        let byte = src.rfind("fGain").unwrap();
        let h = hover(cst.root(), byte, None, None, &li, PositionEncoding::Utf16).unwrap();
        if let HoverContents::Markup(m) = h.contents {
            assert!(m.value.contains("local"));
            assert!(m.value.contains("Float"));
        } else {
            panic!("expected markup");
        }
    }

    #[test]
    fn hover_names_the_enum_type() {
        use std::io::Write;
        let tmp = tempfile::tempdir().unwrap();
        let prj = tmp.path().join("Project.m1prj");
        std::fs::File::create(&prj)
            .unwrap()
            .write_all(
                br#"<?xml version="1.0"?>
<Project>
  <DataTypes>
    <Type Name="Drive State" Storage="enum" Default="Idle">
      <Enum Name="Idle" ContainerOrder="1"/>
      <Enum Name="Off" ContainerOrder="0"/>
      <Enum Name="Running" ContainerOrder="2"/>
    </Type>
  </DataTypes>
  <Component Classname="BuiltIn.GroupCompound" Name="Root"/>
  <Component Classname="BuiltIn.GroupCompound" Name="Root.Control"/>
  <Component Classname="BuiltIn.Channel" Name="Root.Control.State"><Props Type="::This.Drive State"/></Component>
</Project>"#,
            )
            .unwrap();
        let project = m1_typecheck::Project::load(&prj).unwrap();
        let src = "Control.State = 1;\n";
        let cst = m1_core::parse(src);
        let li = LineIndex::new(src);
        let byte = src.find("State").unwrap();
        let h = hover(
            cst.root(),
            byte,
            Some(&project),
            Some("X.m1scr"),
            &li,
            PositionEncoding::Utf16,
        )
        .unwrap();
        if let HoverContents::Markup(m) = h.contents {
            assert!(
                m.value.contains("Drive State"),
                "hover should name the enum type, got: {}",
                m.value
            );
            // Lists every valid value, in ContainerOrder, with the default marked.
            assert!(m.value.contains("values:"), "got: {}", m.value);
            assert!(m.value.contains("`Off`"), "got: {}", m.value);
            assert!(m.value.contains("`Idle` (default)"), "got: {}", m.value);
            assert!(m.value.contains("`Running`"), "got: {}", m.value);
            let off = m.value.find("`Off`").unwrap();
            let idle = m.value.find("`Idle`").unwrap();
            let running = m.value.find("`Running`").unwrap();
            assert!(
                off < idle && idle < running,
                "values not in ContainerOrder: {}",
                m.value
            );
        } else {
            panic!("expected markup");
        }
    }

    #[test]
    fn opaque_hover_does_not_say_type_unknown() {
        // "Output" has no project context — resolves as Opaque.
        let src = "Output.Value = 1;\n";
        let cst = m1_core::parse(src);
        let li = LineIndex::new(src);
        let byte = src.find("Output").unwrap();
        let h = hover(cst.root(), byte, None, None, &li, PositionEncoding::Utf16).unwrap();
        if let HoverContents::Markup(m) = h.contents {
            assert!(
                !m.value.contains("type unknown"),
                "hover should not say 'type unknown' for opaque symbols: {}",
                m.value
            );
        } else {
            panic!("expected markup");
        }
    }

    #[test]
    fn library_function_hover_shows_signature() {
        let src = "x = Calculate.Max(a, b);\n";
        let cst = m1_core::parse(src);
        let li = LineIndex::new(src);
        let byte = src.find("Max").unwrap();
        let h = hover(cst.root(), byte, None, None, &li, PositionEncoding::Utf16).unwrap();
        if let HoverContents::Markup(m) = h.contents {
            assert!(m.value.contains("library function"), "{}", m.value);
            assert!(m.value.contains("Calculate.Max"), "{}", m.value);
            assert!(m.value.contains("->"), "{}", m.value);
        } else {
            panic!("expected markup");
        }
    }

    /// A project mirroring the EV-M1 sample line
    /// `Control.Drive State.AsInteger()`: a `Control` group, a `Drive State`
    /// channel under it typed as the `Drive State` enum, and the enum's members.
    fn drive_state_project() -> (tempfile::TempDir, Project) {
        use std::io::Write;
        let tmp = tempfile::tempdir().unwrap();
        let prj = tmp.path().join("Project.m1prj");
        std::fs::File::create(&prj)
            .unwrap()
            .write_all(
                br#"<?xml version="1.0"?>
<Project>
  <DataTypes>
    <Type Name="Drive State" Storage="enum" Default="Idle">
      <Enum Name="Idle" ContainerOrder="1"/>
      <Enum Name="Off" ContainerOrder="0"/>
    </Type>
  </DataTypes>
  <Component Classname="BuiltIn.GroupCompound" Name="Root"/>
  <Component Classname="BuiltIn.GroupCompound" Name="Root.Control"/>
  <Component Classname="BuiltIn.Channel" Name="Root.Control.Drive State"><Props Type="::This.Drive State"/></Component>
</Project>"#,
            )
            .unwrap();
        let project = m1_typecheck::Project::load(&prj).unwrap();
        (tmp, project)
    }

    fn hover_value_at(project: &Project, src: &str, find: &str, occurrence: usize) -> String {
        let cst = m1_core::parse(src);
        let li = LineIndex::new(src);
        // Byte of the `occurrence`-th match of `find` (0-based).
        let byte = src.match_indices(find).nth(occurrence).unwrap().0;
        let h = hover(
            cst.root(),
            byte,
            Some(project),
            Some("Control.Update.m1scr"),
            &li,
            PositionEncoding::Utf16,
        )
        .unwrap_or_else(|| panic!("no hover for `{find}`#{occurrence}"));
        match h.contents {
            HoverContents::Markup(m) => m.value,
            _ => panic!("expected markup"),
        }
    }

    #[test]
    fn hover_resolves_each_segment_of_a_dotted_accessor_separately() {
        // `Control.Drive State.AsInteger()` — hovering each segment must describe
        // that segment, not the whole expression: a group, an enum channel, a
        // built-in method.
        let (_tmp, project) = drive_state_project();
        let src = "if (Control.Drive State.AsInteger() > 0)\n{\n}\n";

        // 1) `Control` → the top-level group.
        let on_control = hover_value_at(&project, src, "Control", 0);
        assert!(on_control.contains("group"), "Control hover: {on_control}");
        assert!(
            !on_control.contains("AsInteger"),
            "Control hover must not describe the whole path: {on_control}"
        );

        // 2) `Drive State` (the channel) → the custom enum type + its values.
        let on_enum = hover_value_at(&project, src, "Drive State", 0);
        assert!(
            on_enum.contains("Enum") && on_enum.contains("Drive State"),
            "Drive State hover should name the enum: {on_enum}"
        );
        assert!(
            !on_enum.contains("AsInteger"),
            "Drive State hover must not describe the method: {on_enum}"
        );

        // 3) `AsInteger` → the built-in enum accessor method, with its docs.
        let on_method = hover_value_at(&project, src, "AsInteger", 0);
        assert!(
            on_method.contains("AsInteger"),
            "AsInteger hover should name the method: {on_method}"
        );
        assert!(
            on_method.to_lowercase().contains("method"),
            "AsInteger hover should label it a method: {on_method}"
        );
        assert!(
            on_method.contains("Integer representation"),
            "AsInteger hover should show its doc: {on_method}"
        );
    }

    #[test]
    fn hover_on_enum_member_renders_enum_member_value() {
        // `Drive State.Off` — hovering the trailing member `Off` must describe it
        // as the enum member it is (enum name, member, integer value), not fall
        // back to "built-in symbol — type not modelled" (#127). The `Drive State`
        // enum here declares `Off` (ContainerOrder 0) and `Idle` (1).
        let (_tmp, project) = drive_state_project();
        let src = "Local State = Drive State.Off;\n";
        let on_member = hover_value_at(&project, src, "Off", 0);
        assert!(
            on_member.contains("Drive State") && on_member.contains("Off"),
            "enum-member hover should name the enum and member: {on_member}"
        );
        assert!(
            on_member.to_lowercase().contains("enum member"),
            "enum-member hover should label it an enum member: {on_member}"
        );
        assert!(
            on_member.contains("= 0"),
            "enum-member hover should show the member's value: {on_member}"
        );
        assert!(
            !on_member.contains("type not modelled"),
            "enum-member hover must not fall back to the not-modelled message: {on_member}"
        );
    }

    /// A *case-variant* enum head — AV-M1's `… eq universal Switch State.On`
    /// (lowercase `u`) — must hover like the canonical spelling: M1 Build
    /// resolves names case-insensitively, and m1-typecheck#183 made
    /// `enum_by_name` follow suit. Both the head card and the member card must
    /// resolve; neither may fall back to "type not modelled".
    #[test]
    fn hover_on_case_variant_enum_head_still_names_the_enum() {
        use std::io::Write;
        let tmp = tempfile::tempdir().unwrap();
        let prj = tmp.path().join("Project.m1prj");
        std::fs::File::create(&prj)
            .unwrap()
            .write_all(
                br#"<?xml version="1.0"?>
<Project>
  <DataTypes>
    <Type Name="Gear State" Storage="enum" Default="Neutral">
      <Enum Name="Neutral" ContainerOrder="0"/>
      <Enum Name="Driving" ContainerOrder="1"/>
    </Type>
  </DataTypes>
  <Component Classname="BuiltIn.GroupCompound" Name="Root"/>
  <Component Classname="BuiltIn.GroupCompound" Name="Root.Control"/>
  <Component Classname="BuiltIn.Channel" Name="Root.Control.Status"><Props Type="::This.Gear State"/></Component>
</Project>"#,
            )
            .unwrap();
        let project = m1_typecheck::Project::load(&prj).unwrap();
        let src = "Status = gear state.Driving;\n";
        let on_head = hover_value_at(&project, src, "gear state", 0);
        assert!(
            on_head.contains("Gear State") && on_head.to_lowercase().contains("enum"),
            "case-variant head hover should name the canonical enum: {on_head}"
        );
        assert!(
            on_head.contains("Driving") && on_head.contains("Neutral"),
            "case-variant head hover should list the members: {on_head}"
        );
        let on_member = hover_value_at(&project, src, "Driving", 0);
        assert!(
            on_member.contains("Gear State.Driving") && !on_member.contains("type not modelled"),
            "member behind a case-variant head must resolve: {on_member}"
        );
    }

    /// `Status = ASSI.Driving;` — the *head* of an `EnumName.Member` literal is
    /// the enum type itself. Hovering it must describe the enum (name + values),
    /// not fall back to "type not modelled". The enum name is not a channel, so it
    /// only ever resolves Opaque; this is the AV-M1 `ASSI.Driving` case. The member
    /// `Driving` and the enum-typed LHS already hover correctly — only the head was
    /// broken.
    #[test]
    fn hover_on_enum_type_head_names_the_enum() {
        use std::io::Write;
        let tmp = tempfile::tempdir().unwrap();
        let prj = tmp.path().join("Project.m1prj");
        std::fs::File::create(&prj)
            .unwrap()
            .write_all(
                br#"<?xml version="1.0"?>
<Project>
  <DataTypes>
    <Type Name="Gear State" Storage="enum" Default="Neutral">
      <Enum Name="Neutral" ContainerOrder="0"/>
      <Enum Name="Driving" ContainerOrder="1"/>
    </Type>
  </DataTypes>
  <Component Classname="BuiltIn.GroupCompound" Name="Root"/>
  <Component Classname="BuiltIn.GroupCompound" Name="Root.Control"/>
  <Component Classname="BuiltIn.Channel" Name="Root.Control.Status"><Props Type="::This.Gear State"/></Component>
</Project>"#,
            )
            .unwrap();
        let project = m1_typecheck::Project::load(&prj).unwrap();
        let src = "Status = Gear State.Driving;\n";
        // Hover the enum-type head `Gear State`, not the `Driving` member.
        let on_head = hover_value_at(&project, src, "Gear State", 0);
        assert!(
            on_head.contains("Gear State") && on_head.to_lowercase().contains("enum"),
            "enum-type head hover should name the enum type: {on_head}"
        );
        assert!(
            on_head.contains("Driving") && on_head.contains("Neutral"),
            "enum-type head hover should list the members: {on_head}"
        );
        assert!(
            !on_head.contains("type not modelled"),
            "enum-type head must not fall back to the not-modelled message: {on_head}"
        );
    }

    /// The real AV-M1 case: the enum type `ASSI` shares its name with its
    /// enclosing group `Root.Control.AV.ASSI`. Hovering `ASSI` in `ASSI.Driving`
    /// must describe the *enum* (because the next segment `Driving` is one of its
    /// members), not the shadowing group — which group-relative resolution would
    /// otherwise return as `group / type: Unknown`.
    #[test]
    fn hover_on_enum_head_that_shadows_a_group_names_the_enum() {
        use std::io::Write;
        let tmp = tempfile::tempdir().unwrap();
        let prj = tmp.path().join("Project.m1prj");
        std::fs::File::create(&prj)
            .unwrap()
            .write_all(
                br#"<?xml version="1.0"?>
<Project>
  <DataTypes>
    <Type Name="ASSI" Storage="enum" Default="Off">
      <Enum Name="Off" ContainerOrder="0"/>
      <Enum Name="Driving" ContainerOrder="1"/>
    </Type>
  </DataTypes>
  <Component Classname="BuiltIn.GroupCompound" Name="Root"/>
  <Component Classname="BuiltIn.GroupCompound" Name="Root.Control"/>
  <Component Classname="BuiltIn.GroupCompound" Name="Root.Control.ASSI"/>
  <Component Classname="BuiltIn.Channel" Name="Root.Control.ASSI.Status"><Props Type="::This.ASSI"/></Component>
  <Component Classname="BuiltIn.MethodUser" Name="Root.Control.ASSI.Update"/>
</Project>"#,
            )
            .unwrap();
        let project = m1_typecheck::Project::load(&prj).unwrap();
        let src = "Status = ASSI.Driving;\n";
        let cst = m1_core::parse(src);
        let li = LineIndex::new(src);
        let byte = src.find("ASSI").unwrap();
        // The script lives in the `Root.Control.ASSI` group — so a bare `ASSI`
        // resolves group-relatively to that group unless we recognise the literal.
        let h = hover(
            cst.root(),
            byte,
            Some(&project),
            Some("Control.ASSI.Update.m1scr"),
            &li,
            PositionEncoding::Utf16,
        )
        .unwrap();
        let HoverContents::Markup(m) = h.contents else {
            panic!("expected markup")
        };
        assert!(
            m.value.to_lowercase().contains("enum") && m.value.contains("Driving"),
            "ASSI head should describe the enum, not the shadowing group: {}",
            m.value
        );
        assert!(
            !m.value.contains("group"),
            "ASSI head must not resolve to the shadowing group: {}",
            m.value
        );
    }

    /// #163: with the enum/group name collision, a *misspelled* member must still
    /// produce the enum hover (with the valid member list) rather than falling
    /// back to the shadowing group — that is the most useful moment to show it.
    #[test]
    fn hover_on_enum_head_with_typoed_member_still_names_the_enum() {
        use std::io::Write;
        let tmp = tempfile::tempdir().unwrap();
        let prj = tmp.path().join("Project.m1prj");
        std::fs::File::create(&prj)
            .unwrap()
            .write_all(
                br#"<?xml version="1.0"?>
<Project>
  <DataTypes>
    <Type Name="ASSI" Storage="enum" Default="Off">
      <Enum Name="Off" ContainerOrder="0"/>
      <Enum Name="Driving" ContainerOrder="1"/>
    </Type>
  </DataTypes>
  <Component Classname="BuiltIn.GroupCompound" Name="Root"/>
  <Component Classname="BuiltIn.GroupCompound" Name="Root.Control"/>
  <Component Classname="BuiltIn.GroupCompound" Name="Root.Control.ASSI"/>
  <Component Classname="BuiltIn.Channel" Name="Root.Control.ASSI.Status"><Props Type="::This.ASSI"/></Component>
  <Component Classname="BuiltIn.MethodUser" Name="Root.Control.ASSI.Update"/>
</Project>"#,
            )
            .unwrap();
        let project = m1_typecheck::Project::load(&prj).unwrap();
        // `Drivng` is a typo of the member `Driving` — not a member, and
        // `ASSI.Drivng` does not resolve to any symbol.
        let src = "Status = ASSI.Drivng;\n";
        let cst = m1_core::parse(src);
        let li = LineIndex::new(src);
        let byte = src.find("ASSI").unwrap();
        let h = hover(
            cst.root(),
            byte,
            Some(&project),
            Some("Control.ASSI.Update.m1scr"),
            &li,
            PositionEncoding::Utf16,
        )
        .unwrap();
        let HoverContents::Markup(m) = h.contents else {
            panic!("expected markup")
        };
        assert!(
            m.value.to_lowercase().contains("enum") && m.value.contains("Driving"),
            "typoed-member head should still describe the enum + members: {}",
            m.value
        );
        assert!(
            m.value.contains("not a member"),
            "should flag the bad member: {}",
            m.value
        );
        assert!(
            !m.value.contains("`group`"),
            "must not fall back to the shadowing group: {}",
            m.value
        );
    }

    /// A genuine group-relative path that shares an enum name (`ASSI.Status`, where
    /// `Status` is a real child channel, not an enum member) must NOT be hijacked
    /// by the enum hover — it should resolve to the channel.
    #[test]
    fn hover_on_group_name_with_real_child_is_not_hijacked_by_enum() {
        use std::io::Write;
        let tmp = tempfile::tempdir().unwrap();
        let prj = tmp.path().join("Project.m1prj");
        std::fs::File::create(&prj)
            .unwrap()
            .write_all(
                br#"<?xml version="1.0"?>
<Project>
  <DataTypes>
    <Type Name="ASSI" Storage="enum" Default="Off">
      <Enum Name="Off" ContainerOrder="0"/>
      <Enum Name="Driving" ContainerOrder="1"/>
    </Type>
  </DataTypes>
  <Component Classname="BuiltIn.GroupCompound" Name="Root"/>
  <Component Classname="BuiltIn.GroupCompound" Name="Root.Control"/>
  <Component Classname="BuiltIn.GroupCompound" Name="Root.Control.ASSI"/>
  <Component Classname="BuiltIn.Channel" Name="Root.Control.ASSI.Status"><Props Type="u8"/></Component>
  <Component Classname="BuiltIn.MethodUser" Name="Root.Control.ASSI.Update"/>
</Project>"#,
            )
            .unwrap();
        let project = m1_typecheck::Project::load(&prj).unwrap();
        // `ASSI.Status` is a real path (the channel) — `Status` is not an enum
        // member, so the enum hover must not hijack it.
        let src = "x = Control.ASSI.Status;\n";
        let cst = m1_core::parse(src);
        let li = LineIndex::new(src);
        let byte = src.find("Status").unwrap();
        let h = hover(
            cst.root(),
            byte,
            Some(&project),
            Some("Other.Update.m1scr"),
            &li,
            PositionEncoding::Utf16,
        )
        .unwrap();
        let HoverContents::Markup(m) = h.contents else {
            panic!("expected markup")
        };
        assert!(
            !m.value.contains("not a member"),
            "a real group-relative path must not get the enum note: {}",
            m.value
        );
    }

    #[test]
    fn calibration_only_function_hover_is_labelled() {
        // Math.* are calibration-method-only; hover should resolve them but flag
        // that they're not valid in ECU scripts.
        let src = "x = Math.Sqrt(a);\n";
        let cst = m1_core::parse(src);
        let li = LineIndex::new(src);
        let byte = src.find("Sqrt").unwrap();
        let h = hover(cst.root(), byte, None, None, &li, PositionEncoding::Utf16).unwrap();
        if let HoverContents::Markup(m) = h.contents {
            assert!(m.value.contains("Math.Sqrt"), "{}", m.value);
            assert!(
                m.value.to_lowercase().contains("calibration"),
                "should label calibration-only: {}",
                m.value
            );
        } else {
            panic!("expected markup");
        }
    }

    // ---- E4: hover-to-evaluate ----

    /// A project fixture with a value-bearing channel (`Root.Demo.Output`), a
    /// group (`Root.Demo`), a parameter (`Root.Demo.Gain`), a table
    /// (`Root.Demo.Map`) and a user function (`Root.Demo.Update`) — enough to
    /// cover "value-bearing vs. not" for the eval fragment.
    fn eval_project() -> (tempfile::TempDir, Project) {
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
  <Component Classname="BuiltIn.Channel" Name="Root.Demo.Output"><Props Type="f32"/></Component>
  <Component Classname="BuiltIn.Parameter" Name="Root.Demo.Gain"><Props Type="f32"/></Component>
  <Component Classname="BuiltIn.Table" Name="Root.Demo.Map"><Props Type="f32"/></Component>
  <Component Classname="BuiltIn.FuncUser" Name="Root.Demo.Update" Filename="Demo.Update.m1scr"/>
</Project>"#,
            )
            .unwrap();
        let project = m1_typecheck::Project::load(&prj).unwrap();
        (tmp, project)
    }

    /// A one-channel trace at a single tick. `external` flags the channel as a
    /// Tier-3 / scenario-fed input.
    fn trace_for(path: &str, value: crate::eval::Value, external: bool) -> Trace {
        let mut tr = Trace::new();
        tr.push_tick(0.02);
        tr.record_channel(path, value);
        if external {
            tr.mark_external(path);
        }
        tr
    }

    /// Hover markdown for `find` in `src` against `project`, with an optional
    /// eval context. Mirrors the shape of `hover_value_at` but exercises the
    /// eval-aware entry point.
    fn eval_hover_md(
        project: &Project,
        src: &str,
        find: &str,
        eval: Option<EvalContext<'_>>,
    ) -> String {
        let cst = m1_core::parse(src);
        let li = LineIndex::new(src);
        let byte = src.find(find).unwrap();
        let h = hover_with_eval(
            cst.root(),
            byte,
            Some(project),
            Some("Demo.Update.m1scr"),
            &li,
            PositionEncoding::Utf16,
            eval,
        )
        .unwrap_or_else(|| panic!("no hover for `{find}`"));
        match h.contents {
            HoverContents::Markup(m) => m.value,
            _ => panic!("expected markup"),
        }
    }

    #[test]
    fn scenario_hover_shows_value_alongside_type() {
        let (_tmp, project) = eval_project();
        let trace = trace_for("Root.Demo.Output", crate::eval::Value::Float(50.0), false);
        let prov = Provenance::Scenario(std::path::PathBuf::from("idle.toml"));
        let md = eval_hover_md(
            &project,
            "Output = 1;\n",
            "Output",
            Some(EvalContext {
                trace: &trace,
                provenance: &prov,
                tick: TickPolicy::Last,
                expr_offsets_valid: false,
            }),
        );
        // The existing type/symbol info is still present and shown first.
        assert!(md.contains("`channel`"), "type/symbol info kept: {md}");
        assert!(md.contains("type: `Float`"), "type info kept: {md}");
        // The evaluated value is appended after it.
        assert!(md.contains("value: `50`"), "value line present: {md}");
        // A configured scenario carries no honesty suffix.
        assert!(!md.contains("offline default"), "no offline label: {md}");
        assert!(!md.contains("externally driven"), "not external: {md}");
        // Type comes before value.
        assert!(
            md.find("type: `Float`").unwrap() < md.find("value: `50`").unwrap(),
            "type shown before value: {md}"
        );
    }

    #[test]
    fn offline_default_hover_shows_value_with_label() {
        let (_tmp, project) = eval_project();
        let trace = trace_for("Root.Demo.Output", crate::eval::Value::Float(50.0), false);
        let prov = Provenance::OfflineDefault;
        let md = eval_hover_md(
            &project,
            "Output = 1;\n",
            "Output",
            Some(EvalContext {
                trace: &trace,
                provenance: &prov,
                tick: TickPolicy::Last,
                expr_offsets_valid: false,
            }),
        );
        assert!(md.contains("value: `50`"), "value shown: {md}");
        assert!(
            md.contains("(experimental offline default — no scenario)"),
            "offline default labelled: {md}"
        );
    }

    #[test]
    fn external_channel_hover_is_labelled() {
        let (_tmp, project) = eval_project();
        let trace = trace_for("Root.Demo.Output", crate::eval::Value::Float(50.0), true);
        let prov = Provenance::Scenario(std::path::PathBuf::from("idle.toml"));
        let md = eval_hover_md(
            &project,
            "Output = 1;\n",
            "Output",
            Some(EvalContext {
                trace: &trace,
                provenance: &prov,
                tick: TickPolicy::Last,
                expr_offsets_valid: false,
            }),
        );
        assert!(
            md.contains("(externally driven)"),
            "external channel labelled: {md}"
        );
    }

    #[test]
    fn eval_off_hover_equals_pre_eval_baseline() {
        // Regression guard: with no EvalContext, the eval-aware path produces the
        // exact markdown the long-standing `hover` entry point does.
        let (_tmp, project) = eval_project();
        let src = "Output = 1;\n";
        let baseline = eval_hover_md(&project, src, "Output", None);
        let cst = m1_core::parse(src);
        let li = LineIndex::new(src);
        let byte = src.find("Output").unwrap();
        let via_plain = hover(
            cst.root(),
            byte,
            Some(&project),
            Some("Demo.Update.m1scr"),
            &li,
            PositionEncoding::Utf16,
        )
        .unwrap();
        let plain_md = match via_plain.contents {
            HoverContents::Markup(m) => m.value,
            _ => panic!("expected markup"),
        };
        assert_eq!(baseline, plain_md, "eval-off path equals plain hover");
        assert!(
            !baseline.contains("value:"),
            "no value line when off: {baseline}"
        );
    }

    #[test]
    fn non_value_symbols_get_no_value_line() {
        // A group, a function, and a table have no channel column in the trace,
        // so even with a trace available no `value:` line is added.
        let (_tmp, project) = eval_project();
        // The trace only has the Output channel; nothing for Demo/Update/Map.
        let trace = trace_for("Root.Demo.Output", crate::eval::Value::Float(50.0), false);
        let prov = Provenance::OfflineDefault;
        let ctx = EvalContext {
            trace: &trace,
            provenance: &prov,
            tick: TickPolicy::Last,
            expr_offsets_valid: false,
        };
        // Group `Demo`.
        let group_md = eval_hover_md(&project, "Demo.Output = 1;\n", "Demo", Some(ctx));
        assert!(group_md.contains("`group`"), "is a group: {group_md}");
        assert!(
            !group_md.contains("value:"),
            "group has no value: {group_md}"
        );
        // Table `Map`.
        let table_md = eval_hover_md(&project, "x = Demo.Map;\n", "Map", Some(ctx));
        assert!(
            !table_md.contains("value:"),
            "table has no value: {table_md}"
        );
        // Parameter `Gain` — also absent from the trace, so no value line.
        let param_md = eval_hover_md(&project, "x = Demo.Gain;\n", "Gain", Some(ctx));
        assert!(
            !param_md.contains("value:"),
            "param has no column: {param_md}"
        );
    }

    // ---- E5: expression-level hover (per-node values) ----

    /// A trace with one expression column at `(file, offset)` over a single tick,
    /// keyed exactly as the runner records expression sites.
    fn expr_trace_for(file: &str, offset: usize, value: crate::eval::Value) -> Trace {
        let mut tr = Trace::new();
        tr.push_tick(0.02);
        tr.record_expr((file.to_string(), offset), value);
        tr
    }

    #[test]
    fn expr_occurrence_with_recorded_value_shows_it() {
        // A `local` reference resolves as a local (not a channel symbol), so the
        // E4 channel path never fires; the E5 expr lookup keyed on this segment's
        // byte offset supplies the value instead.
        let (_tmp, project) = eval_project();
        let src = "local x = 0;\nOutput = x;\n";
        // Hover the `x` *use* on line 2 (the second occurrence of "x").
        let use_byte = src.match_indices('x').nth(1).unwrap().0;
        let trace = expr_trace_for("Demo.Update.m1scr", use_byte, crate::eval::Value::Int(7));
        let prov = Provenance::Scenario(std::path::PathBuf::from("idle.toml"));

        let cst = m1_core::parse(src);
        let li = LineIndex::new(src);
        let md = match hover_with_eval(
            cst.root(),
            use_byte,
            Some(&project),
            Some("Demo.Update.m1scr"),
            &li,
            PositionEncoding::Utf16,
            Some(EvalContext {
                trace: &trace,
                provenance: &prov,
                tick: TickPolicy::Last,
                expr_offsets_valid: true,
            }),
        )
        .expect("hover present")
        .contents
        {
            HoverContents::Markup(m) => m.value,
            _ => panic!("expected markup"),
        };
        assert!(md.contains("value: `7`"), "expr value shown: {md}");
        assert!(md.contains("(@ t=0.02s)"), "expr tick time shown: {md}");
        // The local's own type/symbol info is still present and shown first.
        assert!(md.contains("`local`"), "local info kept: {md}");
        assert!(
            md.find("`local`").unwrap() < md.find("value: `7`").unwrap(),
            "symbol info before value: {md}"
        );
    }

    #[test]
    fn expr_occurrence_with_no_recorded_value_leaves_hover_unchanged() {
        // A sparse miss: the run recorded a *different* offset, so the hovered
        // segment gets no value line and the rest of the hover is byte-identical
        // to the eval-off baseline.
        let (_tmp, project) = eval_project();
        let src = "local x = 0;\nOutput = x;\n";
        let use_byte = src.match_indices('x').nth(1).unwrap().0;
        // Record a value at an unrelated offset only.
        let trace = expr_trace_for(
            "Demo.Update.m1scr",
            use_byte + 100,
            crate::eval::Value::Int(7),
        );
        let prov = Provenance::Scenario(std::path::PathBuf::from("idle.toml"));

        let cst = m1_core::parse(src);
        let li = LineIndex::new(src);
        let with_eval = match hover_with_eval(
            cst.root(),
            use_byte,
            Some(&project),
            Some("Demo.Update.m1scr"),
            &li,
            PositionEncoding::Utf16,
            Some(EvalContext {
                trace: &trace,
                provenance: &prov,
                tick: TickPolicy::Last,
                expr_offsets_valid: true,
            }),
        )
        .expect("hover present")
        .contents
        {
            HoverContents::Markup(m) => m.value,
            _ => panic!("expected markup"),
        };
        assert!(
            !with_eval.contains("value:"),
            "no value line on a miss: {with_eval}"
        );

        // Byte-identical to the plain (eval-off) hover.
        let baseline = match hover(
            cst.root(),
            use_byte,
            Some(&project),
            Some("Demo.Update.m1scr"),
            &li,
            PositionEncoding::Utf16,
        )
        .expect("hover present")
        .contents
        {
            HoverContents::Markup(m) => m.value,
            _ => panic!("expected markup"),
        };
        assert_eq!(with_eval, baseline, "a miss leaves the hover unchanged");
    }

    #[test]
    fn expr_hover_skipped_when_buffer_offsets_invalid() {
        // The saved-script offsets drift on an edited buffer, so with
        // `expr_offsets_valid: false` the expr lookup is skipped entirely even when
        // a column happens to exist at the segment's current offset.
        let (_tmp, project) = eval_project();
        let src = "local x = 0;\nOutput = x;\n";
        let use_byte = src.match_indices('x').nth(1).unwrap().0;
        let trace = expr_trace_for("Demo.Update.m1scr", use_byte, crate::eval::Value::Int(7));
        let prov = Provenance::Scenario(std::path::PathBuf::from("idle.toml"));

        let cst = m1_core::parse(src);
        let li = LineIndex::new(src);
        let md = match hover_with_eval(
            cst.root(),
            use_byte,
            Some(&project),
            Some("Demo.Update.m1scr"),
            &li,
            PositionEncoding::Utf16,
            Some(EvalContext {
                trace: &trace,
                provenance: &prov,
                tick: TickPolicy::Last,
                expr_offsets_valid: false,
            }),
        )
        .expect("hover present")
        .contents
        {
            HoverContents::Markup(m) => m.value,
            _ => panic!("expected markup"),
        };
        assert!(
            !md.contains("value:"),
            "expr value suppressed when offsets are invalid: {md}"
        );
    }

    #[test]
    fn channel_value_wins_over_expr_value_no_double_line() {
        // A segment that resolves to a channel with a column gets exactly one
        // value line (the E4 channel value), never a second from an expr column at
        // the same offset.
        let (_tmp, project) = eval_project();
        let src = "Output = 1;\n";
        let chan_byte = src.find("Output").unwrap();
        let mut trace = trace_for("Root.Demo.Output", crate::eval::Value::Float(50.0), false);
        // Also record an expr column at the same offset; it must be ignored.
        trace.record_expr(
            ("Demo.Update.m1scr".to_string(), chan_byte),
            crate::eval::Value::Int(7),
        );
        let prov = Provenance::Scenario(std::path::PathBuf::from("idle.toml"));

        let md = eval_hover_md(
            &project,
            src,
            "Output",
            Some(EvalContext {
                trace: &trace,
                provenance: &prov,
                tick: TickPolicy::Last,
                expr_offsets_valid: true,
            }),
        );
        assert!(md.contains("value: `50`"), "channel value shown: {md}");
        assert!(!md.contains("value: `7`"), "expr value not added: {md}");
        assert_eq!(
            md.matches("value:").count(),
            1,
            "exactly one value line: {md}"
        );
    }
}
