//! textDocument/inlayHint: an inline `: Type` after each `local` declaration that
//! has no explicit `<Type>` annotation and whose type is known. Reuses the same
//! inference as hover (`locate::local_decl_type`), so the two always agree.
use crate::features::locate::local_decl_type;
use crate::line_index::{LineIndex, PositionEncoding};
use m1_core::{Field, Kind, Node};
use m1_typecheck::types::ValueType;
use tower_lsp::lsp_types::{InlayHint, InlayHintKind, InlayHintLabel, Range};

fn value_type_str(t: &ValueType) -> &'static str {
    match t {
        ValueType::Boolean => "Boolean",
        ValueType::Integer => "Integer",
        ValueType::Unsigned => "Unsigned",
        ValueType::Float => "Float",
        ValueType::Enum(_) => "Enum",
        ValueType::String => "String",
        ValueType::Unknown => "Unknown",
    }
}

/// Inline type hints for `local` declarations within `range`.
pub fn inlay_hints(
    root: Node,
    range: Range,
    li: &LineIndex,
    enc: PositionEncoding,
) -> Vec<InlayHint> {
    let mut out = Vec::new();
    collect(root, &range, li, enc, &mut out);
    out
}

fn collect(
    root: Node,
    range: &Range,
    li: &LineIndex,
    enc: PositionEncoding,
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
            let t = local_decl_type(n);
            if !matches!(t, ValueType::Unknown) {
                let position = li.position(name.byte_range().end, enc);
                if position.line >= range.start.line && position.line <= range.end.line {
                    out.push(InlayHint {
                        position,
                        label: InlayHintLabel::String(format!(": {}", value_type_str(&t))),
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
        inlay_hints(cst.root(), full, &li, PositionEncoding::Utf16)
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
}
