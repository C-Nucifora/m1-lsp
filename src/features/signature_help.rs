//! textDocument/signatureHelp: show the signature(s) of the call the cursor is
//! inside and highlight the active argument as you type within `( … )`.
//!
//! Two sources of signatures:
//!  * **Library functions** — the intrinsics' overload set (parameter names,
//!    types, return type, doc).
//!  * **User-defined functions / methods** (#30) — M1 user functions declare no
//!    parameters in the project model or the script grammar (they read/write
//!    channels directly), so their signature is `Name() -> ReturnType`: there
//!    are simply no parameters to list, but the callable is no longer silent.
use crate::features::locate::{build_scope, node_at_byte};
use m1_core::{Field, Kind, Node};
use m1_typecheck::intrinsics::Overload;
use m1_typecheck::project::Project;
use m1_typecheck::resolve::{Resolution, resolve};
use m1_typecheck::symbols::{Symbol, SymbolKind};
use m1_typecheck::types::ValueType;
use tower_lsp::lsp_types::{
    Documentation, ParameterInformation, ParameterLabel, SignatureHelp, SignatureInformation,
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
    match resolve(&path, &scope) {
        // Library functions: full overload set, active argument tracked.
        Resolution::BuiltinFn(overloads) => {
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
        // User-defined functions/methods: name + return type, no parameters (#30).
        Resolution::Symbol(sym)
            if matches!(sym.kind, SymbolKind::Function | SymbolKind::Method) =>
        {
            Some(user_fn_help(sym))
        }
        // Project-object methods (`Channel.Set`, `Table.Lookup`, `X.AsInteger`, …):
        // the call resolves opaquely, but the last path segment names a method the
        // intrinsics model with parameter types — surface its overload(s) (#145).
        _ => {
            let method = path.rsplit_once('.').map_or(path.as_str(), |(_, m)| m);
            let overloads = m1_typecheck::intrinsics::get().object_method(method);
            if overloads.is_empty() {
                return None;
            }
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
    }
}

/// The displayed return type for a known value type, or `None` when unknown (so
/// the signature reads `Name()` rather than `Name() -> Unknown`).
fn return_type_str(t: ValueType) -> Option<&'static str> {
    match t {
        ValueType::Boolean => Some("Boolean"),
        ValueType::Integer => Some("Integer"),
        ValueType::Unsigned => Some("Unsigned"),
        ValueType::Float => Some("Float"),
        ValueType::Enum(_) => Some("Enum"),
        ValueType::String => Some("String"),
        ValueType::Unknown => None,
    }
}

/// Signature help for a user-defined function/method. M1 user functions take no
/// declared parameters, so this surfaces the name and (when known) return type.
fn user_fn_help(sym: &Symbol) -> SignatureHelp {
    let kind_word = if sym.kind == SymbolKind::Method {
        "method"
    } else {
        "function"
    };
    let label = match return_type_str(sym.value_type) {
        Some(ret) => format!("{}() -> {ret}", sym.path),
        None => format!("{}()", sym.path),
    };
    SignatureHelp {
        signatures: vec![SignatureInformation {
            label,
            documentation: Some(Documentation::String(format!(
                "user-defined {kind_word} — M1 user functions take no declared parameters"
            ))),
            parameters: Some(vec![]),
            active_parameter: None,
        }],
        active_signature: Some(0),
        active_parameter: None,
    }
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

    #[test]
    fn object_method_call_shows_signature() {
        // `.Lookup(` is a project-object method modelled by the intrinsics; it
        // should get signature help even with no project loaded (#145).
        let h = help("x = Engine.Map.Lookup(rpm, load);\n", "Engine.Map.Lookup(").unwrap();
        assert!(
            h.signatures[0].label.contains("Lookup("),
            "got {}",
            h.signatures[0].label
        );
        assert_eq!(h.active_parameter, Some(0));
    }

    #[test]
    fn unknown_member_call_still_has_no_help() {
        // A made-up method name is not a modelled object method -> no help.
        assert!(help("x = Foo.Bar.Nonexistent(a);\n", "Nonexistent(").is_none());
    }

    #[test]
    fn user_function_call_shows_name_with_no_parameters() {
        use std::io::Write;
        let tmp = tempfile::tempdir().unwrap();
        let prj = tmp.path().join("Project.m1prj");
        std::fs::File::create(&prj)
            .unwrap()
            .write_all(
                br#"<?xml version="1.0"?>
<Project>
  <Component Classname="BuiltIn.GroupCompound" Name="Root"/>
  <Component Classname="BuiltIn.GroupCompound" Name="Root.CAN"/>
  <Component Classname="BuiltIn.FuncUser" Name="Root.CAN.Transcieve" Filename="CAN.Transcieve.m1scr"/>
</Project>"#,
            )
            .unwrap();
        let project = m1_typecheck::Project::load(&prj).unwrap();

        let src = "x = Root.CAN.Transcieve();\n";
        let cst = m1_core::parse(src);
        let byte = src.find('(').unwrap() + 1; // inside the (empty) argument list
        let h = signature_help(cst.root(), byte, Some(&project), Some("Other.m1scr")).unwrap();
        assert_eq!(h.signatures.len(), 1);
        assert!(
            h.signatures[0].label.contains("Root.CAN.Transcieve("),
            "got: {}",
            h.signatures[0].label
        );
        // No parameters, and none is "active".
        assert_eq!(h.signatures[0].parameters.as_ref().unwrap().len(), 0);
        assert_eq!(h.active_parameter, None);
    }
}
