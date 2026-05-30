//! Best-effort expression typing over the m1-core CST.
use crate::resolve::{resolve, Resolution, Scope};
use crate::types::{numeric_join, type_of_number_literal, ValueType};
use m1_core::{Kind, Node};

/// Type the first expression found in a parsed source string. Test/utility entry.
pub fn type_of_source_expr(src: &str, scope: &Scope) -> ValueType {
    let cst = m1_core::parse(src);
    // Find the first expression-bearing statement and type its expression child.
    fn first_expr(n: Node<'_>) -> Option<Node<'_>> {
        if is_expr(n.kind()) {
            return Some(n);
        }
        for c in n.children() {
            if let Some(e) = first_expr(c) {
                return Some(e);
            }
        }
        None
    }
    match first_expr(cst.root()) {
        Some(e) => type_of(e, scope),
        None => ValueType::Unknown,
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

pub fn type_of(node: Node, scope: &Scope) -> ValueType {
    match node.kind() {
        Kind::Number => type_of_number_literal(node.text()),
        Kind::Boolean => ValueType::Boolean,
        Kind::String => ValueType::String,
        Kind::ParenthesizedExpression => child_expr(node)
            .map(|c| type_of(c, scope))
            .unwrap_or(ValueType::Unknown),
        Kind::Identifier | Kind::MemberExpression => {
            let path = path_text(node);
            // Typed enum-member path: `<EnumTypeName>.<Member>` -> Enum(id).
            if let Some(p) = scope.project {
                if let Some((head, member)) = path.rsplit_once('.') {
                    if let Some(id) = p.symbols().enum_by_name(head) {
                        if p.symbols().enum_has_member(id, member) {
                            return ValueType::Enum(id);
                        }
                    }
                }
            }
            match resolve(&path, scope) {
                Resolution::Local(t) => t,
                Resolution::Symbol(s) => s.value_type,
                Resolution::Opaque | Resolution::Unresolved => ValueType::Unknown,
            }
        }
        Kind::CallExpression => ValueType::Unknown, // v1: return types unknown
        Kind::UnaryExpression => {
            // `not`/`!` -> Boolean; `-x` -> type of x.
            let op_is_not = node
                .children()
                .iter()
                .any(|c| matches!(c.kind(), Kind::Not | Kind::Bang));
            if op_is_not {
                ValueType::Boolean
            } else {
                child_expr(node)
                    .map(|c| type_of(c, scope))
                    .unwrap_or(ValueType::Unknown)
            }
        }
        Kind::TernaryExpression => {
            let exprs: Vec<_> = node
                .named_children()
                .into_iter()
                .filter(|c| is_expr(c.kind()))
                .collect();
            // condition, consequence, alternative -> join the two branches
            match (exprs.get(1), exprs.get(2)) {
                (Some(a), Some(b)) => {
                    let (ta, tb) = (type_of(*a, scope), type_of(*b, scope));
                    if ta == tb {
                        ta
                    } else {
                        numeric_join(ta, tb)
                    }
                }
                _ => ValueType::Unknown,
            }
        }
        Kind::BinaryExpression => type_of_binary(node, scope),
        _ => ValueType::Unknown,
    }
}

fn type_of_binary(node: Node, scope: &Scope) -> ValueType {
    let operands: Vec<_> = node
        .named_children()
        .into_iter()
        .filter(|c| is_expr(c.kind()))
        .collect();
    let (l, r) = match (operands.first(), operands.get(1)) {
        (Some(l), Some(r)) => (type_of(*l, scope), type_of(*r, scope)),
        _ => return ValueType::Unknown,
    };
    match binary_op_kind(node) {
        OpClass::Comparison | OpClass::Logical => ValueType::Boolean,
        OpClass::Arithmetic => numeric_join(l, r),
        OpClass::Bitwise => l, // result follows the (integer) left operand
        OpClass::Unknown => ValueType::Unknown,
    }
}

enum OpClass {
    Arithmetic,
    Comparison,
    Logical,
    Bitwise,
    Unknown,
}

fn binary_op_kind(node: Node) -> OpClass {
    for c in node.children() {
        match c.kind() {
            Kind::Plus | Kind::Minus | Kind::Star | Kind::Slash | Kind::Percent => {
                return OpClass::Arithmetic
            }
            Kind::EqEq
            | Kind::BangEq
            | Kind::Eq
            | Kind::Neq
            | Kind::Lt
            | Kind::Gt
            | Kind::LtEq
            | Kind::GtEq => return OpClass::Comparison,
            Kind::And | Kind::Or | Kind::AmpAmp | Kind::PipePipe => return OpClass::Logical,
            Kind::Amp | Kind::Pipe | Kind::Caret | Kind::LtLt | Kind::GtGt => {
                return OpClass::Bitwise
            }
            _ => {}
        }
    }
    OpClass::Unknown
}

fn child_expr(node: Node) -> Option<Node> {
    node.named_children()
        .into_iter()
        .find(|c| is_expr(c.kind()))
}

/// The dotted source text of an identifier / member-expression path.
pub fn path_text(node: Node) -> String {
    node.text().to_string()
}
