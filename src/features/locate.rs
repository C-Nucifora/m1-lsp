//! Cursor → CST node → dotted path, plus the m1-typecheck Scope builder.
use m1_core::{Kind, Node};
use m1_typecheck::project::Project;
use m1_typecheck::resolve::Scope;
use m1_typecheck::types::{type_from_hungarian, ValueType};
use std::collections::HashMap;

/// The deepest node whose byte range contains `byte`.
pub fn node_at_byte(root: Node, byte: usize) -> Option<Node> {
    let r = root.byte_range();
    if !(r.start..=r.end).contains(&byte) {
        return None;
    }
    let mut current = root;
    'descend: loop {
        for child in current.children() {
            let cr = child.byte_range();
            if (cr.start..cr.end).contains(&byte) || (cr.start == cr.end && cr.start == byte) {
                current = child;
                continue 'descend;
            }
        }
        return Some(current);
    }
}

/// The enclosing identifier/member-expression node and its full dotted path text.
pub fn path_at_byte(root: Node, byte: usize) -> Option<(Node, String)> {
    let node = node_at_byte(root, byte)?;
    if !matches!(node.kind(), Kind::Identifier | Kind::MemberExpression) {
        return None;
    }
    // Climb out of nested member expressions to the outermost one.
    let mut top = node;
    while let Some(parent) = top.parent() {
        if parent.kind() == Kind::MemberExpression {
            top = parent;
        } else {
            break;
        }
    }
    Some((top, top.text().to_string()))
}

/// Collect locals (name -> inferred type) from the CST, mirroring m1-typecheck.
pub fn collect_locals(root: Node) -> HashMap<String, ValueType> {
    let mut locals = HashMap::new();
    fn walk(n: Node, locals: &mut HashMap<String, ValueType>) {
        if n.kind() == Kind::LocalDeclaration {
            if let Some(name) = n
                .named_children()
                .into_iter()
                .find(|c| c.kind() == Kind::Identifier)
            {
                let t = type_from_hungarian(name.text()).unwrap_or_else(|| {
                    // Fall back to typing the initializer expression.
                    let initializer = n
                        .named_children()
                        .into_iter()
                        .find(|c| c.kind() != Kind::Identifier && c.kind() != Kind::TypeAnnotation);
                    if let Some(init) = initializer {
                        let empty_scope = m1_typecheck::resolve::Scope {
                            locals: HashMap::new(),
                            group: None,
                            project: None,
                        };
                        m1_typecheck::typer::type_of(init, &empty_scope)
                    } else {
                        ValueType::Unknown
                    }
                });
                locals.insert(name.text().to_string(), t);
            }
        }
        for c in n.children() {
            walk(c, locals);
        }
    }
    walk(root, &mut locals);
    locals
}

/// Build the resolution scope for `src` in the context of `project` (if any) and
/// the script's `file_name` (for group-relative resolution).
pub fn build_scope<'p>(
    root: Node,
    project: Option<&'p Project>,
    file_name: Option<&str>,
) -> Scope<'p> {
    let group = match (project, file_name) {
        (Some(p), Some(f)) => p.group_for_script(f),
        _ => None,
    };
    Scope {
        locals: collect_locals(root),
        group,
        project,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn locates_bare_identifier() {
        let src = "Ratio = 2;\n";
        let cst = m1_core::parse(src);
        let (_, path) = path_at_byte(cst.root(), 2).unwrap();
        assert_eq!(path, "Ratio");
    }

    #[test]
    fn locates_member_path() {
        let src = "Vund Klee.Trilby Glonk = 1;\n";
        let cst = m1_core::parse(src);
        // cursor inside the "Trilby Glonk" tail
        let byte = src.find("Trilby").unwrap();
        let (_, path) = path_at_byte(cst.root(), byte).unwrap();
        assert_eq!(path, "Vund Klee.Trilby Glonk");
    }

    #[test]
    fn whitespace_has_no_path() {
        let src = "x = 1;\n";
        let cst = m1_core::parse(src);
        // byte at the space after '='
        let byte = src.find("= 1").unwrap() + 1;
        assert!(path_at_byte(cst.root(), byte).is_none());
    }

    #[test]
    fn collects_typed_locals() {
        let src = "local fGain = 1.0;\nlocal iCount = 0;\n";
        let cst = m1_core::parse(src);
        let locals = collect_locals(cst.root());
        assert_eq!(locals.get("fGain"), Some(&ValueType::Float));
        assert_eq!(locals.get("iCount"), Some(&ValueType::Integer));
    }

    #[test]
    fn infers_type_from_initializer_when_no_prefix() {
        let src = "local count = 0;\nlocal ratio = 1.5;\nlocal flag = true;\n";
        let cst = m1_core::parse(src);
        let locals = collect_locals(cst.root());
        assert_eq!(locals.get("count"),  Some(&ValueType::Integer));
        assert_eq!(locals.get("ratio"),  Some(&ValueType::Float));
        assert_eq!(locals.get("flag"),   Some(&ValueType::Boolean));
    }

    #[test]
    fn hungarian_prefix_beats_initializer() {
        // fCount has Hungarian Float prefix; initializer says Integer — prefix wins.
        let src = "local fCount = 0;\n";
        let cst = m1_core::parse(src);
        let locals = collect_locals(cst.root());
        assert_eq!(locals.get("fCount"), Some(&ValueType::Float));
    }
}
