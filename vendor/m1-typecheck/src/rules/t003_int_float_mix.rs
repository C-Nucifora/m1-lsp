use crate::diagnostics::{make, TypeCode, TypeDiagnostic};
use crate::resolve::Scope;
use crate::typer::type_of;
use crate::types::ValueType;
use m1_core::{Kind, Node, Severity};

pub struct Rule;

impl super::Rule for Rule {
    fn check_node(&self, node: &Node, scope: &Scope, out: &mut Vec<TypeDiagnostic>) {
        if node.kind() != Kind::BinaryExpression {
            return;
        }
        let is_arith = node.children().iter().any(|c| {
            matches!(
                c.kind(),
                Kind::Plus | Kind::Minus | Kind::Star | Kind::Slash | Kind::Percent
            )
        });
        if !is_arith {
            return;
        }
        let ops: Vec<_> = node
            .named_children()
            .into_iter()
            .filter(|c| is_expr(c.kind()))
            .collect();
        let (Some(l), Some(r)) = (ops.first(), ops.get(1)) else {
            return;
        };
        let (lt, rt) = (type_of(*l, scope), type_of(*r, scope));
        let mixed = (lt == ValueType::Float && rt.is_integral())
            || (rt == ValueType::Float && lt.is_integral());
        if mixed {
            out.push(make(
                TypeCode::T003,
                node,
                Severity::Warning,
                "mixing integer and float in arithmetic; use Convert.ToFloat/ToInteger".into(),
            ));
        }
    }
}

fn is_expr(k: Kind) -> bool {
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
            | Kind::Boolean
            | Kind::String
    )
}
