//! T062 `deprecated-overload`: a library call that resolves to a deprecated
//! overload (e.g. `Serial.GetTransmitHandle` -> `Serial.GetHandle`). Emitted as
//! a hint so editors can surface the replacement without blocking.
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
        let Resolution::BuiltinFn(overloads) = resolve(&path, scope) else {
            return;
        };
        // Flag only when every resolved overload is deprecated (no live signature
        // shares the name).
        if overloads.is_empty() || !overloads.iter().all(|o| o.deprecated) {
            return;
        }
        out.push(make(
            TypeCode::T062,
            node,
            Severity::Hint,
            format!("`{path}` is deprecated; check the manual for its replacement"),
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
    fn deprecated_overload_is_hinted() {
        let src = "x = Serial.GetTransmitHandle(1);\n";
        assert!(codes(src).contains(&TypeCode::T062));
    }

    #[test]
    fn live_overload_is_not_hinted() {
        let src = "x = Calculate.Max(a, b);\n";
        assert!(!codes(src).contains(&TypeCode::T062));
    }
}
