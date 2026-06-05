//! Cursor → CST node → dotted path, plus the m1-typecheck Scope builder.
use m1_core::{Field, Kind, Node};
use m1_typecheck::project::Project;
use m1_typecheck::resolve::Scope;
use m1_typecheck::types::ValueType;
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

/// The identifier nodes of a dotted-path node, leftmost first. For
/// `Root.Engine.Speed` this is `[Root, Engine, Speed]`; for a bare `Speed` it is
/// `[Speed]`. Anchors are ordinary segments here (`[This, Speed]`).
pub fn segment_nodes(top: Node) -> Vec<Node> {
    // Descend the left (Object) spine iteratively rather than recursively, so a
    // deeply nested member chain can't overflow the call stack (#133). The
    // recursive form visited Object first then pushed Property, yielding segments
    // leftmost-first; we reproduce that by collecting each member's Property while
    // walking down, then reversing (and appending the leftmost base node, which
    // the recursion emitted first).
    let mut props = Vec::new(); // properties, innermost-first
    let mut base = None; // the leftmost non-member node, if any
    let mut n = top;
    loop {
        if n.kind() == Kind::MemberExpression {
            if let Some(prop) = n.child_by_field(Field::Property) {
                props.push(prop);
            }
            if let Some(obj) = n.child_by_field(Field::Object) {
                n = obj;
                continue;
            }
            // Member with no Object field: the recursion would have pushed only
            // its Property (already recorded) and stopped — no base node.
            break;
        } else {
            base = Some(n);
            break;
        }
    }
    let mut out = Vec::new();
    if let Some(b) = base {
        out.push(b);
    }
    out.extend(props.into_iter().rev());
    out
}

/// Index of the dotted-path segment whose text span contains `byte`.
pub fn segment_at_byte(top: Node, byte: usize) -> Option<usize> {
    segment_nodes(top).iter().position(|s| {
        let r = s.byte_range();
        byte >= r.start && byte <= r.end
    })
}

/// Infer the type of a single `local` declaration from its initializer
/// expression (literals/arithmetic only, via an empty scope so there are no
/// cross-local ordering hazards). Returns `Unknown` when there is no initializer
/// or its type cannot be determined.
pub fn local_decl_type(decl: Node) -> ValueType {
    let Some(_name) = decl
        .named_children()
        .into_iter()
        .find(|c| c.kind() == Kind::Identifier)
    else {
        return ValueType::Unknown;
    };
    let initializer = decl
        .named_children()
        .into_iter()
        .find(|c| c.kind() != Kind::Identifier && c.kind() != Kind::TypeAnnotation);
    if let Some(init) = initializer {
        let empty_scope = Scope {
            locals: HashMap::new(),
            group: None,
            project: None,
        };
        m1_typecheck::typer::type_of(init, &empty_scope)
    } else {
        ValueType::Unknown
    }
}

/// Collect locals (name -> inferred type) from the CST, mirroring m1-typecheck.
pub fn collect_locals(root: Node) -> HashMap<String, ValueType> {
    // Iterate the tree with m1-core's explicit work-stack pre-order iterator
    // rather than recursion, so a pathologically deep document can't overflow the
    // call stack (#133). Same pre-order visit, same result.
    let mut locals = HashMap::new();
    for n in root.descendants() {
        if n.kind() == Kind::LocalDeclaration
            && let Some(name) = n
                .named_children()
                .into_iter()
                .find(|c| c.kind() == Kind::Identifier)
        {
            locals.insert(name.text().to_string(), local_decl_type(n));
        }
    }
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
        assert_eq!(locals.get("count"), Some(&ValueType::Integer));
        assert_eq!(locals.get("ratio"), Some(&ValueType::Float));
        assert_eq!(locals.get("flag"), Some(&ValueType::Boolean));
    }

    #[test]
    fn initializer_type_wins_over_name_prefix() {
        // `fCount` would once have been forced to Float by its name prefix; now the
        // initializer (Integer) is authoritative.
        let src = "local fCount = 0;\n";
        let cst = m1_core::parse(src);
        let locals = collect_locals(cst.root());
        assert_eq!(locals.get("fCount"), Some(&ValueType::Integer));
    }
}
