use crate::diagnostics::{make, TypeCode, TypeDiagnostic};
use crate::resolve::Scope;
use crate::typer::type_of;
use crate::types::type_from_hungarian;
use m1_core::{Kind, Node, Severity};

pub struct Rule;

impl super::Rule for Rule {
    fn check_node(&self, node: &Node, scope: &Scope, out: &mut Vec<TypeDiagnostic>) {
        if node.kind() != Kind::LocalDeclaration {
            return;
        }
        let children = node.named_children();
        // The name is the first Identifier child; the initialiser is the next
        // expression child after it (positional, robust to TypeAnnotation).
        let Some(name_pos) = children.iter().position(|c| c.kind() == Kind::Identifier) else {
            return;
        };
        let name_node = children[name_pos];
        let Some(declared) = type_from_hungarian(name_node.text()) else {
            return;
        };
        let Some(value) = children
            .iter()
            .skip(name_pos + 1)
            .find(|c| is_expr(c.kind()))
        else {
            return; // no initialiser expression
        };
        let actual = type_of(*value, scope);
        // Treat Integer/Unsigned as compatible with each other (both integral).
        let mismatch = actual.is_known()
            && declared != actual
            && !(declared.is_integral() && actual.is_integral());
        if mismatch {
            out.push(make(
                TypeCode::T011,
                &name_node,
                Severity::Warning,
                format!(
                    "local `{}` prefix implies {:?} but initialiser is {:?}",
                    name_node.text(),
                    declared,
                    actual
                ),
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
