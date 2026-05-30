use crate::diagnostics::{make, TypeCode, TypeDiagnostic};
use crate::resolve::Scope;
use crate::types::type_from_hungarian;
use m1_core::{Kind, Node, Severity};

pub struct Rule;

impl super::Rule for Rule {
    fn check_node(&self, node: &Node, _scope: &Scope, out: &mut Vec<TypeDiagnostic>) {
        if node.kind() != Kind::LocalDeclaration {
            return;
        }
        let Some(name_node) = node
            .named_children()
            .into_iter()
            .find(|c| c.kind() == Kind::Identifier)
        else {
            return;
        };
        let name = name_node.text();
        if type_from_hungarian(name).is_none() {
            out.push(make(
                TypeCode::T010,
                &name_node,
                Severity::Warning,
                format!("local `{name}` lacks a Hungarian type prefix (b/u/i/f + UpperCase)"),
            ));
        }
    }
}
