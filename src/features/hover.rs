//! textDocument/hover: describe the symbol/local/opaque under the cursor.
use crate::convert::range;
use crate::features::locate::{build_scope, path_at_byte};
use crate::line_index::{LineIndex, PositionEncoding};
use m1_typecheck::project::Project;
use m1_typecheck::resolve::{Resolution, resolve};
use m1_typecheck::symbols::{Symbol, SymbolKind};
use m1_typecheck::types::ValueType;
use tower_lsp::lsp_types::{Hover, HoverContents, MarkupContent, MarkupKind};

fn value_type_str(t: ValueType) -> &'static str {
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

fn kind_str(k: SymbolKind) -> &'static str {
    match k {
        SymbolKind::Channel => "channel",
        SymbolKind::Parameter => "parameter",
        SymbolKind::Constant => "constant",
        SymbolKind::Function => "function",
        SymbolKind::Method => "method",
        SymbolKind::Table => "table",
        SymbolKind::Group => "group",
        SymbolKind::Reference => "reference",
        SymbolKind::Object => "object",
        SymbolKind::Other => "symbol",
    }
}

fn symbol_markdown(sym: &Symbol) -> String {
    let mut s = format!("**{}** `{}`\n\n", sym.path, kind_str(sym.kind));
    // For objects, show the package class instead of a (meaningless) value type.
    if sym.kind == SymbolKind::Object {
        match &sym.class {
            Some(class) => s.push_str(&format!("class: `{class}`")),
            None => s.push_str("object"),
        }
        return s;
    }
    s.push_str(&format!("type: `{}`", value_type_str(sym.value_type)));
    if let Some(unit) = &sym.unit {
        s.push_str(&format!("  ·  unit: `{unit}`"));
    }
    s
}

use m1_typecheck::intrinsics::Overload;

/// `(p1: T1, p2: T2) -> Ret` for one overload signature.
fn signature(path: &str, ov: &Overload) -> String {
    let params: Vec<String> = ov
        .params
        .iter()
        .map(|p| format!("{}: {}", p.name, p.ty))
        .collect();
    format!("{path}({}) -> {}", params.join(", "), ov.returns)
}

fn builtin_object_markdown(name: &str) -> String {
    let doc = m1_typecheck::intrinsics::get()
        .library_object(name)
        .map(|o| o.doc.as_str())
        .unwrap_or("");
    format!("**{name}** `library object`\n\n{doc}")
}

fn builtin_fn_markdown(path: &str, overloads: &[&Overload]) -> String {
    let mut s = format!("**{path}** `library function`\n\n");
    for ov in overloads {
        s.push_str(&format!("```\n{}\n```\n", signature(path, ov)));
    }
    if let Some(first) = overloads.first() {
        if !first.doc.is_empty() {
            s.push_str(&format!("\n{}\n", first.doc));
        }
        if first.stateful {
            s.push_str(
                "\n⚠ **stateful** — call it on every execution; never inside an `if`/`when` or a comparison.",
            );
        }
        if first.deprecated {
            s.push_str("\n⚠ **deprecated**");
        }
    }
    s
}

/// Render hover for the path at `byte`. `project`/`file_name` drive resolution.
pub fn hover(
    root: m1_core::Node,
    byte: usize,
    project: Option<&Project>,
    file_name: Option<&str>,
    li: &LineIndex,
    enc: PositionEncoding,
) -> Option<Hover> {
    let (node, path) = path_at_byte(root, byte)?;
    let scope = build_scope(root, project, file_name);
    let md = match resolve(&path, &scope) {
        Resolution::Local(t) => format!("**{path}** `local`\n\ntype: `{}`", value_type_str(t)),
        Resolution::Symbol(sym) => symbol_markdown(sym),
        Resolution::BuiltinObject(name) => builtin_object_markdown(name),
        Resolution::BuiltinFn(overloads) => builtin_fn_markdown(&path, &overloads),
        Resolution::Opaque => format!("**{path}**\n\nbuilt-in symbol — type not modelled"),
        Resolution::Unresolved => return None,
    };
    Some(Hover {
        contents: HoverContents::Markup(MarkupContent {
            kind: MarkupKind::Markdown,
            value: md,
        }),
        range: Some(range(&node.byte_range(), li, enc)),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
