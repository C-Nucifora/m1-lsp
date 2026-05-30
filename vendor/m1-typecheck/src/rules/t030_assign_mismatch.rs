use crate::diagnostics::{make, TypeCode, TypeDiagnostic};
use crate::resolve::{resolve, Resolution, Scope};
use crate::symbols::SymbolKind;
use crate::typer::{path_text, type_of};
use crate::types::ValueType;
use m1_core::{Kind, Node, Severity};

pub struct Rule;

impl super::Rule for Rule {
    fn check_node(&self, node: &Node, scope: &Scope, out: &mut Vec<TypeDiagnostic>) {
        if node.kind() != Kind::AssignmentStatement {
            return;
        }
        // Only plain `=` (not `+=` etc., which imply the existing type).
        if !node.children().iter().any(|c| c.kind() == Kind::Assign) {
            return;
        }
        let kids = node.named_children();
        let (Some(target), Some(value)) = (
            kids.iter().find(|c| is_expr(c.kind())),
            kids.iter().rev().find(|c| is_expr(c.kind())),
        ) else {
            return;
        };
        if std::ptr::eq(target, value) {
            return; // single child; nothing to compare
        }
        // Target must resolve to a Channel/Parameter with a known type.
        let target_ty = match resolve(&path_text(*target), scope) {
            Resolution::Symbol(s)
                if matches!(s.kind, SymbolKind::Channel | SymbolKind::Parameter) =>
            {
                s.value_type
            }
            _ => return,
        };
        let value_ty = type_of(*value, scope);
        if !target_ty.is_known() || !value_ty.is_known() {
            return; // silent under Unknown
        }
        if !compatible(target_ty, value_ty) {
            out.push(make(
                TypeCode::T030,
                node,
                Severity::Warning,
                format!("assigning {value_ty:?} to a target of type {target_ty:?}"),
            ));
        }
    }
}

/// Assignment compatibility (conservative).
fn compatible(target: ValueType, value: ValueType) -> bool {
    use ValueType::*;
    match (target, value) {
        (a, b) if a == b => true,
        // Integer/Unsigned interchangeable.
        (Integer, Unsigned) | (Unsigned, Integer) => true,
        // Integer channel accepts integral values; float channel accepts numerics.
        (Float, Integer) | (Float, Unsigned) => true,
        // Enum target only accepts the *same* enum.
        (Enum(a), Enum(b)) => a == b,
        _ => false, // e.g. Float -> Integer channel, Enum vs numeric, Boolean vs numeric
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
