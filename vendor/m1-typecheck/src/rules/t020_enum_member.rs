use crate::diagnostics::{make, TypeCode, TypeDiagnostic};
use crate::resolve::Scope;
use crate::typer::path_text;
use m1_core::{Kind, Node, Severity};

pub struct Rule;

impl super::Rule for Rule {
    fn check_node(&self, node: &Node, scope: &Scope, out: &mut Vec<TypeDiagnostic>) {
        let Some(project) = scope.project else { return };
        let table = project.symbols();
        // `<EnumTypeName>.<Member>` with a head that names an enum type but a
        // member the enum does not declare.
        //
        // NOTE: the plan also specified an `LHS is (Member)` trigger, but the
        // tree-sitter-m1 grammar treats `is` as a syntax error (and the idiom
        // never appears in the m1-example corpus), so it is unreachable; only the
        // typed-member-path idiom is checked here.
        if node.kind() != Kind::MemberExpression {
            return;
        }
        if matches!(
            node.parent().map(|p| p.kind()),
            Some(Kind::MemberExpression)
        ) {
            return; // only the outermost path
        }
        let path = path_text(*node);
        if let Some((head, member)) = path.rsplit_once('.') {
            if let Some(id) = table.enum_by_name(head) {
                if !table.enum_has_member(id, member) {
                    out.push(make(
                        TypeCode::T020,
                        node,
                        Severity::Warning,
                        format!("`{member}` is not a member of enum `{head}`"),
                    ));
                }
            }
        }
    }
}
