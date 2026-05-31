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
    n: Node,
    range: &Range,
    li: &LineIndex,
    enc: PositionEncoding,
    out: &mut Vec<InlayHint>,
) {
    if n.kind() == Kind::LocalDeclaration {
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
    for c in n.children() {
        collect(c, range, li, enc, out);
    }
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
    fn hungarian_prefix_hint() {
        let h = hints("local fGain = 0;\n");
        assert_eq!(label(&h[0]), ": Float");
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
