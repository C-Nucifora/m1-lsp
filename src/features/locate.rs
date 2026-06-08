//! Cursor → CST node → dotted path, plus the m1-typecheck Scope builder.
use m1_core::{Field, Kind, Node};
use m1_typecheck::project::Project;
use m1_typecheck::resolve::Scope;
use m1_typecheck::types::ValueType;
use std::collections::HashMap;
use tower_lsp::lsp_types::Url;

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

/// True when `n` is the `property` half of a `member_expression` (the part after
/// the `.`), which is a channel/field access — never a local.
pub(crate) fn is_member_property(n: Node) -> bool {
    n.parent()
        .filter(|p| p.kind() == Kind::MemberExpression)
        .and_then(|p| p.child_by_field(Field::Property))
        .map(|prop| prop.byte_range() == n.byte_range())
        .unwrap_or(false)
}

/// True when `n` is inside a `<Type>` annotation (e.g. `local <Integer>`), where
/// an identifier names a type, not a value.
pub(crate) fn in_type_annotation(n: Node) -> bool {
    let mut cur = n;
    while let Some(p) = cur.parent() {
        if p.kind() == Kind::TypeAnnotation {
            return true;
        }
        cur = p;
    }
    false
}

/// Pre-order walk of every node in `root`, calling `f(node, parent, in_type_annotation)`.
///
/// This is the O(n) backbone for the reference/highlight/occurrence scans. It
/// threads the two pieces of context those scans need — the node's `parent` and
/// whether it sits inside a `<Type>` annotation — *downward* through the walk, so
/// callers never climb parents to re-derive them. The earlier scans applied
/// outermost-path / write / type-annotation predicates per node, each of which
/// climbed to the root (`in_type_annotation` walks every ancestor),
/// making a full-document scan O(n²) — pathological on deeply nested input (a
/// 16k-deep expression with an identifier at every level took minutes; #133's
/// sibling perf bug). Carrying context makes every per-node test O(1).
///
/// Iterative (explicit stack) so deep input can't overflow the call stack either.
/// Children are visited left-to-right (source order), matching `descendants()`.
pub(crate) fn walk_ctx<'a>(root: Node<'a>, mut f: impl FnMut(Node<'a>, Option<Node<'a>>, bool)) {
    // (node, parent, inside-a-TypeAnnotation). Push children reversed so they pop
    // in source order — a pre-order visit identical to the old `descendants()`.
    let mut stack: Vec<(Node<'a>, Option<Node<'a>>, bool)> = vec![(root, None, false)];
    while let Some((node, parent, in_ta)) = stack.pop() {
        f(node, parent, in_ta);
        let child_in_ta = in_ta || node.kind() == Kind::TypeAnnotation;
        for child in node.children().into_iter().rev() {
            stack.push((child, Some(node), child_in_ta));
        }
    }
}

/// True when `n` is the `property` half of `parent` (a `member_expression`) — the
/// part after the `.`, a field access that is never a local. The parent-context
/// form of [`is_member_property`]: O(1), no parent climb (the caller already holds
/// the parent from [`walk_ctx`]).
pub(crate) fn is_member_property_of(n: Node, parent: Option<Node>) -> bool {
    parent
        .filter(|p| p.kind() == Kind::MemberExpression)
        .and_then(|p| p.child_by_field(Field::Property))
        .map(|prop| prop.byte_range() == n.byte_range())
        .unwrap_or(false)
}

/// True when `node` (a top-level path) is being *written*: the target of an
/// assignment or the name of a `local` declaration, given its `parent`. The
/// parent-context form of [`super::references`]'s write test — O(1).
pub(crate) fn is_write_of(node: Node, parent: Option<Node>) -> bool {
    match parent {
        Some(p) if p.kind() == Kind::AssignmentStatement => p
            .child_by_field(Field::Target)
            .map(|t| t.byte_range() == node.byte_range())
            .unwrap_or(false),
        Some(p) if p.kind() == Kind::LocalDeclaration => p
            .child_by_field(Field::Name)
            .map(|name| name.byte_range() == node.byte_range())
            .unwrap_or(false),
        _ => false,
    }
}

/// Every outermost dotted-path node in `root` (an `identifier` / `member_expression`
/// not itself the child of a `member_expression`, excluding type-annotation names),
/// in source order, with whether it is a write. The O(n) replacement for scanning
/// `descendants()` and testing an outermost-path / write predicate per node.
/// `node` is the occurrence; `is_write` is true for assignment targets and
/// `local` declaration names.
pub(crate) fn for_each_top_path<'a>(root: Node<'a>, mut f: impl FnMut(Node<'a>, bool)) {
    walk_ctx(root, |node, parent, in_ta| {
        let is_path = matches!(node.kind(), Kind::Identifier | Kind::MemberExpression);
        let parent_is_member = parent
            .map(|p| p.kind() == Kind::MemberExpression)
            .unwrap_or(false);
        if is_path && !parent_is_member && !in_ta {
            f(node, is_write_of(node, parent));
        }
    });
}

/// The file name (basename) of a `file://` URI, for group-relative resolution.
pub(crate) fn file_name_of(uri: &Url) -> Option<String> {
    uri.to_file_path()
        .ok()?
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
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
/// The inferred type of a `local`'s initializer, resolved in `scope` — so a
/// channel read (`local s = Demo.Speed;`) types from the project model and a
/// local-to-local copy (`local c = s;`) propagates from `scope.locals` (#153).
/// `scope` should carry the locals declared *before* this one (declaration-order
/// threading), the script's group, and the project, as [`build_scope`] provides.
pub fn local_decl_type(decl: Node, scope: &Scope) -> ValueType {
    // The grammar names the initializer `value`; using the field (rather than
    // "first non-name child") correctly handles an *Identifier* initializer
    // (`local copy = other;`), which the old kind-exclusion heuristic dropped.
    match decl.child_by_field(Field::Value) {
        Some(init) => m1_typecheck::typer::type_of(init, scope),
        None => ValueType::Unknown,
    }
}

/// Collect locals (name -> inferred type) from the CST in the context of
/// `project`/`group`, threading declaration order so each local sees the ones
/// declared before it (and channel reads resolve when a project is present).
fn collect_locals_with(
    root: Node,
    project: Option<&Project>,
    group: Option<&str>,
) -> HashMap<String, ValueType> {
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
            let scope = Scope {
                locals: locals.clone(),
                group: group.map(str::to_string),
                project,
            };
            let t = local_decl_type(n, &scope);
            locals.insert(name.text().to_string(), t);
        }
    }
    locals
}

/// Collect locals (name -> inferred type) from the CST, mirroring m1-typecheck.
/// Project-less: literal and local-to-local-copy types resolve; channel reads
/// stay `Unknown` (callers that only test membership don't care).
pub fn collect_locals(root: Node) -> HashMap<String, ValueType> {
    collect_locals_with(root, None, None)
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
        locals: collect_locals_with(root, project, group.as_deref()),
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
