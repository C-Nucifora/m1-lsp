//! textDocument/signatureHelp for library function calls: show the overload
//! signature(s) and highlight the active argument as you type inside `( … )`.
use crate::features::locate::{build_scope, node_at_byte};
use m1_core::{Field, Kind, Node};
use m1_typecheck::intrinsics::Overload;
use m1_typecheck::project::Project;
use m1_typecheck::resolve::{Resolution, resolve};
use tower_lsp::lsp_types::{
    ParameterInformation, ParameterLabel, SignatureHelp, SignatureInformation,
};

/// The nearest enclosing call whose argument list contains `byte`.
fn enclosing_call(root: Node, byte: usize) -> Option<Node> {
    let mut node = node_at_byte(root, byte)?;
    loop {
        if node.kind() == Kind::CallExpression
            && let Some(args) = node.child_by_field(Field::Arguments)
        {
            let r = args.byte_range();
            if byte >= r.start && byte <= r.end {
                return Some(node);
            }
        }
        node = node.parent()?;
    }
}

/// 0-based index of the argument the cursor is in: the number of top-level
/// commas in the argument list before `byte`.
fn active_arg(args: Node, byte: usize) -> usize {
    args.children()
        .iter()
        .filter(|c| c.kind_str() == "," && c.byte_range().end <= byte)
        .count()
}

fn sig_info(path: &str, ov: &Overload) -> SignatureInformation {
    let params: Vec<ParameterInformation> = ov
        .params
        .iter()
        .map(|p| ParameterInformation {
            label: ParameterLabel::Simple(format!("{}: {}", p.name, p.ty)),
            documentation: None,
        })
        .collect();
    let label = format!(
        "{path}({}) -> {}",
        ov.params
            .iter()
            .map(|p| format!("{}: {}", p.name, p.ty))
            .collect::<Vec<_>>()
            .join(", "),
        ov.returns
    );
    let doc = if ov.doc.is_empty() {
        None
    } else {
        Some(tower_lsp::lsp_types::Documentation::String(ov.doc.clone()))
    };
    SignatureInformation {
        label,
        documentation: doc,
        parameters: Some(params),
        active_parameter: None,
    }
}

/// Pick the overload that best fits `active`: the first whose arity covers it,
/// else the widest overload.
fn pick_by_arity(overloads: &[&Overload], active: usize) -> usize {
    overloads
        .iter()
        .position(|o| o.params.len() > active)
        .unwrap_or_else(|| {
            overloads
                .iter()
                .enumerate()
                .max_by_key(|(_, o)| o.params.len())
                .map(|(i, _)| i)
                .unwrap_or(0)
        })
}

pub fn signature_help(
    root: Node,
    byte: usize,
    project: Option<&Project>,
    file_name: Option<&str>,
) -> Option<SignatureHelp> {
    let call = enclosing_call(root, byte)?;
    let path = call.child_by_field(Field::Function)?.text().to_string();
    let scope = build_scope(root, project, file_name);
    let Resolution::BuiltinFn(overloads) = resolve(&path, &scope) else {
        return None; // v1: signature help only for the library functions
    };
    let active = call
        .child_by_field(Field::Arguments)
        .map(|args| active_arg(args, byte))
        .unwrap_or(0);
    let signatures = overloads.iter().map(|ov| sig_info(&path, ov)).collect();
    Some(SignatureHelp {
        signatures,
        active_signature: Some(pick_by_arity(&overloads, active) as u32),
        active_parameter: Some(active as u32),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn help(src: &str, cursor_after: &str) -> Option<SignatureHelp> {
        let cst = m1_core::parse(src);
        let byte = src.find(cursor_after).unwrap() + cursor_after.len();
        signature_help(cst.root(), byte, None, None)
    }

    #[test]
    fn library_call_shows_overload_and_active_arg() {
        // cursor right after the comma -> second argument (index 1).
        let h = help("x = Calculate.Max(a, b);\n", "Calculate.Max(a,").unwrap();
        assert!(!h.signatures.is_empty());
        assert!(h.signatures[0].label.contains("Calculate.Max("));
        assert!(h.signatures[0].label.contains("->"));
        assert_eq!(h.active_parameter, Some(1));
    }

    #[test]
    fn first_arg_is_index_zero() {
        let h = help("x = Calculate.Max(a, b);\n", "Calculate.Max(").unwrap();
        assert_eq!(h.active_parameter, Some(0));
    }

    #[test]
    fn no_help_outside_a_library_call() {
        assert!(help("local x = 1;\n", "local x =").is_none());
    }
}
