//! T061 `integrated-only-call`: the integrated-only firmware objects (`BR2`,
//! `GPS`, `LTC`, `MDD`, `PDM`, `RS232Comms`, `VCS`) exist in firmware but are
//! built into hardware classes and must not be called directly. Warn on a direct
//! call — but only when the head does not resolve to a same-named project symbol
//! (a project may legitimately own a `GPS` channel tree).
use crate::diagnostics::{TypeCode, TypeDiagnostic, make};
use crate::resolve::{Resolution, Scope, resolve};
use crate::typer::path_text;
use m1_core::{Field, Kind, Node, Severity};

pub struct Rule;

impl super::Rule for Rule {
    fn check_node(&self, node: &Node, scope: &Scope, out: &mut Vec<TypeDiagnostic>) {
        if node.kind() != Kind::CallExpression {
            return;
        }
        let Some(callee) = node.child_by_field(Field::Function) else {
            return;
        };
        let path = path_text(callee);
        let head = path.split('.').next().unwrap_or(&path);
        if !crate::intrinsics::get().is_integrated_only(head) {
            return;
        }
        // Only the firmware object is integrated-only; a project object of the
        // same name resolves to a Symbol and is a legitimate reference.
        if !matches!(
            resolve(&path, scope),
            Resolution::Opaque | Resolution::Unresolved | Resolution::BuiltinFn(_)
        ) {
            return;
        }
        out.push(make(
            TypeCode::T061,
            node,
            Severity::Warning,
            format!(
                "`{head}` is an integrated-only object: it is built into a hardware \
                 class and must not be called directly"
            ),
        ));
    }
}

#[cfg(test)]
mod tests {
    use crate::diagnostics::TypeCode;
    use crate::rules::check_script_no_project;

    fn codes(src: &str) -> Vec<TypeCode> {
        check_script_no_project(src)
            .diagnostics
            .into_iter()
            .map(|d| d.code)
            .collect()
    }

    #[test]
    fn direct_call_to_integrated_object_is_flagged() {
        let src = "x = PDM.GetOutput(1);\n";
        assert!(codes(src).contains(&TypeCode::T061));
    }

    #[test]
    fn library_object_call_is_not_flagged() {
        let src = "x = Calculate.Max(a, b);\n";
        assert!(!codes(src).contains(&TypeCode::T061));
    }
}
