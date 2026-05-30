use crate::diagnostics::{make, TypeCode, TypeDiagnostic};
use crate::resolve::{resolve, Resolution, Scope};
use m1_core::{Kind, Node, Severity};

pub struct Rule;

impl super::Rule for Rule {
    fn check_node(&self, node: &Node, scope: &Scope, out: &mut Vec<TypeDiagnostic>) {
        // Only check a member-expression or bare identifier that is NOT part of a
        // larger member-expression (avoid double-checking every prefix). We
        // evaluate each full path once at its outermost node.
        if !matches!(node.kind(), Kind::Identifier | Kind::MemberExpression) {
            return;
        }
        if let Some(parent) = node.parent() {
            if parent.kind() == Kind::MemberExpression {
                return; // part of a longer path; the outer node carries the full path
            }
            // Skip the type name inside `<Type>` annotations.
            if parent.kind() == Kind::TypeAnnotation {
                return;
            }
        }
        if matches!(resolve(node.text(), scope), Resolution::Unresolved) {
            out.push(make(
                TypeCode::T001,
                node,
                Severity::Warning,
                format!("unresolved reference `{}`", node.text()),
            ));
        }
    }
}
