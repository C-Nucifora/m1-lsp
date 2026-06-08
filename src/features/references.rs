//! textDocument/references + textDocument/documentHighlight.
//!
//! Both share one occurrence finder. `document_highlight` is always file-local
//! ("where else in *this* file?"). `references` is project-wide for project
//! symbols (#29): `project_references` searches every `.m1scr` in the workspace
//! for the dotted path. Locals stay file-scoped (the type model scopes them per
//! file). The script set is the one cached on `LoadedProject` at load
//! (`script_files`), enumerated from the filesystem since a real `.m1prj` omits
//! `Filename=` attributes and the symbol-table list would be empty.
use crate::convert::range;
use crate::features::locate::{
    build_scope, collect_locals, file_name_of, for_each_top_path, in_type_annotation,
    is_member_property, is_member_property_of, node_at_byte, path_at_byte, walk_ctx,
};
use crate::line_index::{LineIndex, PositionEncoding};
use m1_core::{Kind, Node};
use m1_typecheck::project::Project;
use m1_typecheck::resolve::{Resolution, Scope, resolve};
use tower_lsp::lsp_types::{DocumentHighlight, DocumentHighlightKind, Location, Url};

/// The canonical project-symbol path that `path` names when written with `scope`
/// — e.g. a group-relative `Speed` in the `Root.Engine` group resolves to
/// `Root.Engine.Speed`. `None` when it doesn't resolve to a project symbol (a
/// local, a library member, or an opaque/unresolved path), in which case callers
/// fall back to matching the path text verbatim. This is what lets the same
/// channel under different spellings collapse onto one entity (#143), mirroring
/// the call-hierarchy data-flow graph.
fn canonical(scope: &Scope, path: &str) -> Option<String> {
    match resolve(path, scope) {
        Resolution::Symbol(s) => Some(s.path.clone()),
        _ => None,
    }
}

/// Every top-level path occurrence in `root` whose canonical symbol path equals
/// `target`, resolved through this file's group `scope`. When `writes_only`, keep
/// only assignment-target (producer) sites — the go-to-implementation case.
#[allow(clippy::too_many_arguments)]
fn canonical_locations(
    project: &Project,
    file_name: &str,
    root: Node,
    target: &str,
    uri: &Url,
    li: &LineIndex,
    enc: PositionEncoding,
    writes_only: bool,
) -> Vec<Location> {
    let scope = build_scope(root, Some(project), Some(file_name));
    let mut out = Vec::new();
    for_each_top_path(root, |n, is_write| {
        if writes_only && !is_write {
            return;
        }
        if canonical(&scope, n.text()).as_deref() == Some(target) {
            out.push(Location {
                uri: uri.clone(),
                range: range(&n.byte_range(), li, enc),
            });
        }
    });
    out
}

/// The outermost path node (`identifier` / `member_expression`) at `n`: climb out
/// of any enclosing member expressions, matching `path_at_byte`.
fn top_path_node(n: Node) -> Node {
    let mut top = n;
    while let Some(p) = top.parent() {
        if p.kind() == Kind::MemberExpression {
            top = p;
        } else {
            break;
        }
    }
    top
}

/// Every identifier that refers to the local named `name` (declaration or use),
/// excluding member-access properties and type-annotation names.
///
/// O(n) context-carrying walk ([`walk_ctx`]): the member-property and
/// type-annotation tests read the parent/flag threaded by the walk instead of
/// climbing ancestors per node (which made this O(n²)). Stack-safe on deep input.
fn collect_local_idents<'a>(root: Node<'a>, name: &str, out: &mut Vec<Node<'a>>) {
    walk_ctx(root, |n, parent, in_ta| {
        if n.kind() == Kind::Identifier
            && n.text() == name
            && !is_member_property_of(n, parent)
            && !in_ta
        {
            out.push(n);
        }
    });
}

/// Every top-level path node whose dotted text equals `path`. O(n) single pass
/// ([`for_each_top_path`]) — stack-safe and no per-node parent climb.
fn collect_path_matches<'a>(root: Node<'a>, path: &str, out: &mut Vec<Node<'a>>) {
    for_each_top_path(root, |n, _is_write| {
        if n.text() == path {
            out.push(n);
        }
    });
}

/// What the cursor at `byte` refers to. A [`Local`](CursorTarget::Local) is a
/// bare identifier naming a known file-scoped local; a [`Path`](CursorTarget::Path)
/// is the dotted project path (channel / library member / unresolved) the cursor
/// sits on. This is the single place the "local vs member-property vs path" rule
/// lives — references, document-highlight and the project-wide search all classify
/// the cursor through it, so they can't drift.
enum CursorTarget {
    Local(String),
    Path(String),
}

/// Classify the cursor at `byte`: a known local (bare identifier, not a member
/// property or a type-annotation name, present in the local table) or else the
/// dotted path it sits on.
fn classify_cursor(root: Node, byte: usize) -> Option<CursorTarget> {
    let node = node_at_byte(root, byte)?;
    if node.kind() == Kind::Identifier
        && !is_member_property(node)
        && !in_type_annotation(node)
        && collect_locals(root).contains_key(node.text())
    {
        return Some(CursorTarget::Local(node.text().to_string()));
    }
    let (_, path) = path_at_byte(root, byte)?;
    Some(CursorTarget::Path(path))
}

/// Nodes in `root` that refer to the same entity as the cursor at `byte`.
fn occurrences<'a>(root: Node<'a>, byte: usize) -> Vec<Node<'a>> {
    let mut out = Vec::new();
    match classify_cursor(root, byte) {
        // A known local: precise, name-based match.
        Some(CursorTarget::Local(name)) => collect_local_idents(root, &name, &mut out),
        // Otherwise the full dotted path (channel / project symbol / library member).
        Some(CursorTarget::Path(path)) => collect_path_matches(root, &path, &mut out),
        None => {}
    }
    out
}

/// True when `n` (or the path it tops) is being written: the target of an
/// assignment or the name of a `local` declaration. For a node already collected
/// from a top-path scan this is its [`for_each_top_path`] write flag; this form
/// climbs to the top path first, for the post-scan per-node callers that hold an
/// arbitrary path node. Shares the classification with `locate::is_write_of`.
fn is_write(n: Node) -> bool {
    let top = top_path_node(n);
    crate::features::locate::is_write_of(top, top.parent())
}

/// Every top-level dotted-path occurrence in `root`, as `(path, byte_range,
/// is_write)`. A "write" is an assignment target or a `local` declaration name;
/// everything else is a read. Skips type-annotation names. Powers the
/// call-hierarchy channel↔script read/write index ([`super::call_hierarchy`]).
pub(crate) fn path_occurrences(root: Node) -> Vec<(String, std::ops::Range<usize>, bool)> {
    // O(n) single pass ([`for_each_top_path`]): one pre-order walk that threads
    // parent context, so it is both stack-safe on deep input (#133) and free of
    // the per-node parent climbs that made the old `descendants()` scan O(n²).
    let mut out = Vec::new();
    for_each_top_path(root, |n, is_write| {
        out.push((n.text().to_string(), n.byte_range(), is_write));
    });
    out
}

pub fn references(
    root: Node,
    byte: usize,
    uri: Url,
    li: &LineIndex,
    enc: PositionEncoding,
) -> Option<Vec<Location>> {
    let nodes = occurrences(root, byte);
    if nodes.is_empty() {
        return None;
    }
    Some(
        nodes
            .into_iter()
            .map(|n| Location {
                uri: uri.clone(),
                range: range(&n.byte_range(), li, enc),
            })
            .collect(),
    )
}

/// What the cursor refers to, for reference search. A `Local` is file-scoped (the
/// type model scopes locals per file); a `Path` is a project symbol that can be
/// referenced from any `.m1scr` in the workspace.
pub enum RefTarget {
    Local(String),
    Path(String),
}

/// Classify the cursor target for the project-wide search, via the shared
/// `classify_cursor` (so references and document-highlight share one rule).
pub fn ref_target(root: Node, byte: usize) -> Option<RefTarget> {
    Some(match classify_cursor(root, byte)? {
        CursorTarget::Local(name) => RefTarget::Local(name),
        CursorTarget::Path(path) => RefTarget::Path(path),
    })
}

/// Locations of the dotted `path` within one already-parsed file, matched by
/// verbatim text. When `writes_only`, keep only producer (assignment-target)
/// sites — the go-to-implementation fallback for non-project paths.
fn path_text_locations(
    root: Node,
    path: &str,
    uri: &Url,
    li: &LineIndex,
    enc: PositionEncoding,
    writes_only: bool,
) -> Vec<Location> {
    let mut nodes = Vec::new();
    collect_path_matches(root, path, &mut nodes);
    nodes
        .into_iter()
        .filter(|n| !writes_only || is_write(*n))
        .map(|n| Location {
            uri: uri.clone(),
            range: range(&n.byte_range(), li, enc),
        })
        .collect()
}

/// Project-wide references (#29). A local stays file-local; a project symbol is
/// searched across every `.m1scr` in the workspace. The script set (`script_files`)
/// is taken from the filesystem (a real `.m1prj` carries no `Filename=` attributes,
/// so the symbol-table list would be empty). `open_text` supplies the in-memory
/// buffer for a file when one is open (newer than disk); files not open are read
/// from disk. The cursor's own file is always included.
///
/// Project-wide canonical reference search, shared by [`project_references`]
/// (`writes_only = false`) and [`project_implementations`] (`writes_only =
/// true`). A local stays file-local; a project symbol is searched across every
/// `.m1scr`, matched by resolved canonical path so group-relative and full-path
/// spellings of the same channel collapse (#143), falling back to verbatim text
/// matching for library members / opaque / unresolved paths. When `writes_only`,
/// only producer (assignment-target) sites are kept.
#[allow(clippy::too_many_arguments)]
fn project_canonical_refs(
    writes_only: bool,
    project: &Project,
    script_files: &[std::path::PathBuf],
    cursor_uri: &Url,
    cursor_text: &str,
    byte: usize,
    enc: PositionEncoding,
    open_text: &dyn Fn(&Url) -> Option<String>,
) -> Option<Vec<Location>> {
    let cursor_cst = m1_core::parse(cursor_text);
    match ref_target(cursor_cst.root(), byte)? {
        // Locals are file-scoped.
        RefTarget::Local(name) => {
            let li = LineIndex::new(cursor_text);
            let mut nodes = Vec::new();
            collect_local_idents(cursor_cst.root(), &name, &mut nodes);
            let locs: Vec<Location> = nodes
                .into_iter()
                .filter(|n| !writes_only || is_write(*n))
                .map(|n| Location {
                    uri: cursor_uri.clone(),
                    range: range(&n.byte_range(), &li, enc),
                })
                .collect();
            (!locs.is_empty()).then_some(locs)
        }
        RefTarget::Path(path) => {
            let files = crate::project_store::gather_project_scripts(
                script_files,
                cursor_uri,
                Some(cursor_text),
                open_text,
            );
            let cursor_scope = build_scope(
                cursor_cst.root(),
                Some(project),
                file_name_of(cursor_uri).as_deref(),
            );
            let target = canonical(&cursor_scope, &path);
            let mut locs = Vec::new();
            for (uri, text) in &files {
                let li = LineIndex::new(text);
                let cst = m1_core::parse(text);
                match &target {
                    Some(t) => locs.extend(canonical_locations(
                        project,
                        file_name_of(uri).as_deref().unwrap_or_default(),
                        cst.root(),
                        t,
                        uri,
                        &li,
                        enc,
                        writes_only,
                    )),
                    None => locs.extend(path_text_locations(
                        cst.root(),
                        &path,
                        uri,
                        &li,
                        enc,
                        writes_only,
                    )),
                }
            }
            (!locs.is_empty()).then_some(locs)
        }
    }
}

pub fn project_references(
    project: &Project,
    script_files: &[std::path::PathBuf],
    cursor_uri: &Url,
    cursor_text: &str,
    byte: usize,
    enc: PositionEncoding,
    open_text: &dyn Fn(&Url) -> Option<String>,
) -> Option<Vec<Location>> {
    project_canonical_refs(
        false,
        project,
        script_files,
        cursor_uri,
        cursor_text,
        byte,
        enc,
        open_text,
    )
}

/// textDocument/implementation: jump to where the symbol under the cursor is
/// **written** (produced). For an M1 channel that is the assignment statement(s)
/// across the project that compute its value — distinct from go-to-definition,
/// which resolves the declaration in `Project.m1prj`. For a local it is the
/// declaration / assignment sites within the file. Mirrors
/// [`project_references`] but keeps only write occurrences.
pub fn project_implementations(
    project: &Project,
    script_files: &[std::path::PathBuf],
    cursor_uri: &Url,
    cursor_text: &str,
    byte: usize,
    enc: PositionEncoding,
    open_text: &dyn Fn(&Url) -> Option<String>,
) -> Option<Vec<Location>> {
    project_canonical_refs(
        true,
        project,
        script_files,
        cursor_uri,
        cursor_text,
        byte,
        enc,
        open_text,
    )
}

pub fn document_highlights(
    root: Node,
    byte: usize,
    li: &LineIndex,
    enc: PositionEncoding,
) -> Option<Vec<DocumentHighlight>> {
    let nodes = occurrences(root, byte);
    if nodes.is_empty() {
        return None;
    }
    Some(
        nodes
            .into_iter()
            .map(|n| DocumentHighlight {
                range: range(&n.byte_range(), li, enc),
                kind: Some(if is_write(n) {
                    DocumentHighlightKind::WRITE
                } else {
                    DocumentHighlightKind::READ
                }),
            })
            .collect(),
    )
}

/// Project-aware document highlight: like [`document_highlights`], but when the
/// cursor is on a project symbol it matches every occurrence in the file by
/// canonical path, so a channel spelled group-relative in one line and full-path
/// in another both highlight (#143). Falls back to the text/name-based highlight
/// for locals, library members, and when no project is loaded.
pub fn document_highlights_scoped(
    project: Option<&Project>,
    file_name: Option<&str>,
    root: Node,
    byte: usize,
    li: &LineIndex,
    enc: PositionEncoding,
) -> Option<Vec<DocumentHighlight>> {
    // Locals stay name-based (already exact); only project paths need canonicalising.
    if let Some(CursorTarget::Path(path)) = classify_cursor(root, byte)
        && let Some(proj) = project
    {
        let scope = build_scope(root, Some(proj), file_name);
        if let Some(target) = canonical(&scope, &path) {
            let mut out = Vec::new();
            for_each_top_path(root, |n, is_write| {
                if canonical(&scope, n.text()).as_deref() == Some(target.as_str()) {
                    out.push(DocumentHighlight {
                        range: range(&n.byte_range(), li, enc),
                        kind: Some(if is_write {
                            DocumentHighlightKind::WRITE
                        } else {
                            DocumentHighlightKind::READ
                        }),
                    });
                }
            });
            if !out.is_empty() {
                return Some(out);
            }
        }
    }
    document_highlights(root, byte, li, enc)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn url() -> Url {
        Url::parse("file:///t.m1scr").unwrap()
    }

    fn refs(src: &str, at: &str) -> Vec<Location> {
        let cst = m1_core::parse(src);
        let li = LineIndex::new(src);
        let byte = src.find(at).unwrap();
        references(cst.root(), byte, url(), &li, PositionEncoding::Utf16).unwrap_or_default()
    }

    #[test]
    fn finds_all_local_occurrences() {
        let locs = refs("local count = 0;\ncount = count + 1;\n", "count");
        assert_eq!(locs.len(), 3, "declaration + two uses");
    }

    #[test]
    fn local_search_ignores_same_named_member_property() {
        // `Foo.count` is a field access, not the local.
        let locs = refs(
            "local count = 0;\nFoo.count = 1;\ncount = count + 1;\n",
            "count",
        );
        assert_eq!(locs.len(), 3);
    }

    #[test]
    fn finds_channel_path_occurrences() {
        // Not a local -> match by full dotted path. Two writes to the same channel.
        let locs = refs("Output.Value = 1;\nOutput.Value = 2;\n", "Output");
        assert_eq!(locs.len(), 2);
    }

    #[test]
    fn no_references_on_whitespace() {
        let cst = m1_core::parse("x = 1;\n");
        let li = LineIndex::new("x = 1;\n");
        let byte = "x = 1;\n".find("= 1").unwrap() + 1; // the space
        assert!(references(cst.root(), byte, url(), &li, PositionEncoding::Utf16).is_none());
    }

    #[test]
    fn highlights_classify_write_vs_read() {
        let src = "local count = 0;\ncount = count + 1;\n";
        let cst = m1_core::parse(src);
        let li = LineIndex::new(src);
        let byte = src.find("count").unwrap();
        let hl = document_highlights(cst.root(), byte, &li, PositionEncoding::Utf16).unwrap();
        let writes = hl
            .iter()
            .filter(|h| h.kind == Some(DocumentHighlightKind::WRITE))
            .count();
        let reads = hl
            .iter()
            .filter(|h| h.kind == Some(DocumentHighlightKind::READ))
            .count();
        assert_eq!(
            writes, 2,
            "the decl and the `count =` assignment are writes"
        );
        assert_eq!(reads, 1, "the `count + 1` use is a read");
    }

    #[test]
    fn path_occurrences_do_not_overflow_on_deep_input() {
        // Regression for #133: the pre-order walk ([`walk_ctx`]) is iterative, so a
        // pathologically deep document must not overflow the call stack. Reaching
        // the assertion is the proof — a stack overflow would abort the process.
        let depth = 20_000;
        let mut src = String::with_capacity(depth * 2 + 8);
        src.push_str("x = ");
        for _ in 0..depth {
            src.push('(');
        }
        src.push('1');
        for _ in 0..depth {
            src.push(')');
        }
        src.push_str(";\n");
        let cst = m1_core::parse(&src);
        let occ = path_occurrences(cst.root());
        // The assignment target `x` is the one top-level path occurrence.
        assert!(
            occ.iter().any(|(p, _, w)| p == "x" && *w),
            "expected the write to `x`"
        );
    }

    #[test]
    fn path_occurrences_skip_type_annotation_names() {
        // The walk threads an "inside <Type>" flag instead of climbing ancestors;
        // the type name `Integer` in the annotation must not be a path occurrence,
        // while the declared local `myValue` is (a write).
        let cst = m1_core::parse("local <Integer> myValue = 0;\nmyValue = 1;\n");
        let occ = path_occurrences(cst.root());
        assert!(
            !occ.iter().any(|(p, _, _)| p == "Integer"),
            "type-annotation name must be skipped: {occ:?}"
        );
        let writes: Vec<_> = occ
            .iter()
            .filter(|(p, _, w)| p == "myValue" && *w)
            .collect();
        assert_eq!(writes.len(), 2, "decl + assignment are writes: {occ:?}");
    }

    #[test]
    fn path_occurrences_scale_linearly_on_deep_input() {
        // Perf regression guard for the reviewer's O(n²) finding. Worst case: an
        // identifier at *every* nesting level, so the old code's per-node ancestor
        // climbs (`is_top_path`/`is_write`/`in_type_annotation`) cost O(depth) each
        // → O(n²) overall (this shape didn't finish depth=2_000 in 90 s). The
        // single-pass [`for_each_top_path`] walk is O(n): ~0.1 s here. A generous
        // bound catches a quadratic regression (minutes) without flaking on slow CI.
        let depth = 20_000;
        let mut src = String::with_capacity(depth * 6 + 8);
        src.push_str("x = ");
        for _ in 0..depth {
            src.push_str("(a + ");
        }
        src.push('1');
        for _ in 0..depth {
            src.push(')');
        }
        src.push_str(";\n");
        let cst = m1_core::parse(&src);
        let start = std::time::Instant::now();
        let occ = path_occurrences(cst.root());
        let elapsed = start.elapsed();
        // One occurrence per `a` plus the `x` target.
        assert_eq!(occ.len(), depth + 1);
        assert!(occ.iter().any(|(p, _, w)| p == "x" && *w));
        assert!(
            elapsed < std::time::Duration::from_secs(10),
            "path_occurrences took {elapsed:?} on depth {depth} — expected ~O(n); \
             a quadratic regression would take minutes"
        );
    }

    #[test]
    fn project_references_span_multiple_scripts() {
        use crate::project_store::ProjectStore;
        use std::io::Write;

        let tmp = tempfile::tempdir().unwrap();
        let m1prj = r#"<?xml version="1.0"?>
<Project>
  <Component Classname="BuiltIn.GroupCompound" Name="Root"/>
  <Component Classname="BuiltIn.GroupCompound" Name="Root.Engine"/>
  <Component Classname="BuiltIn.Channel" Name="Root.Engine.Speed"><Props Type="f32"/></Component>
  <Component Classname="BuiltIn.FuncUser" Name="Root.Engine.Write" Filename="A.m1scr"/>
  <Component Classname="BuiltIn.FuncUser" Name="Root.Engine.Read" Filename="B.m1scr"/>
</Project>"#;
        std::fs::File::create(tmp.path().join("Project.m1prj"))
            .unwrap()
            .write_all(m1prj.as_bytes())
            .unwrap();
        let a_src = "Root.Engine.Speed = 1.0;\n";
        let b_src = "local x = Root.Engine.Speed;\n";
        std::fs::write(tmp.path().join("A.m1scr"), a_src).unwrap();
        std::fs::write(tmp.path().join("B.m1scr"), b_src).unwrap();

        let store = ProjectStore::new();
        store.discover_and_load(tmp.path()).unwrap();

        let a_uri = Url::from_file_path(tmp.path().join("A.m1scr")).unwrap();
        let byte = 0; // cursor on `Root.Engine.Speed` in A
        let no_open = |_: &Url| None;
        let locs = store
            .with_project(|p| {
                let lp = p.unwrap();
                project_references(
                    &lp.project,
                    &lp.script_files,
                    &a_uri,
                    a_src,
                    byte,
                    PositionEncoding::Utf16,
                    &no_open,
                )
            })
            .expect("references across files");

        // One write site in A, one read site in B.
        let files: std::collections::BTreeSet<_> =
            locs.iter().map(|l| l.uri.path().to_string()).collect();
        assert_eq!(
            files.len(),
            2,
            "references should span both scripts: {locs:?}"
        );
        assert!(locs.iter().any(|l| l.uri.path().ends_with("A.m1scr")));
        assert!(locs.iter().any(|l| l.uri.path().ends_with("B.m1scr")));
    }

    #[test]
    fn references_canonicalize_across_path_spellings() {
        // #143: the same channel written group-relative in one script and read
        // full-path in another must be found by a single Find-All-References,
        // regardless of which spelling the cursor is on.
        use crate::project_store::ProjectStore;
        use std::io::Write;

        let tmp = tempfile::tempdir().unwrap();
        let m1prj = r#"<?xml version="1.0"?>
<Project>
  <Component Classname="BuiltIn.GroupCompound" Name="Root"/>
  <Component Classname="BuiltIn.GroupCompound" Name="Root.Engine"/>
  <Component Classname="BuiltIn.Channel" Name="Root.Engine.Speed"><Props Type="f32"/></Component>
  <Component Classname="BuiltIn.FuncUser" Name="Root.Engine.Write" Filename="A.m1scr"/>
  <Component Classname="BuiltIn.FuncUser" Name="Root.Engine.Read" Filename="B.m1scr"/>
</Project>"#;
        std::fs::File::create(tmp.path().join("Project.m1prj"))
            .unwrap()
            .write_all(m1prj.as_bytes())
            .unwrap();
        // A writes the channel GROUP-RELATIVE (it lives in Root.Engine); B reads
        // it FULL-PATH. Different spellings, same channel.
        let a_src = "Speed = 1.0;\n";
        let b_src = "local x = Root.Engine.Speed;\n";
        std::fs::write(tmp.path().join("A.m1scr"), a_src).unwrap();
        std::fs::write(tmp.path().join("B.m1scr"), b_src).unwrap();

        let store = ProjectStore::new();
        store.discover_and_load(tmp.path()).unwrap();
        let a_uri = Url::from_file_path(tmp.path().join("A.m1scr")).unwrap();
        let no_open = |_: &Url| None;

        // Cursor on the group-relative `Speed` in A must still find B's full-path read.
        let locs = store
            .with_project(|p| {
                let lp = p.unwrap();
                project_references(
                    &lp.project,
                    &lp.script_files,
                    &a_uri,
                    a_src,
                    a_src.find("Speed").unwrap(),
                    PositionEncoding::Utf16,
                    &no_open,
                )
            })
            .expect("references across spellings");
        let files: std::collections::BTreeSet<_> =
            locs.iter().map(|l| l.uri.path().to_string()).collect();
        assert_eq!(
            files.len(),
            2,
            "group-relative cursor should still find the full-path reference: {locs:?}"
        );
        assert!(locs.iter().any(|l| l.uri.path().ends_with("A.m1scr")));
        assert!(locs.iter().any(|l| l.uri.path().ends_with("B.m1scr")));
    }

    #[test]
    fn project_implementations_resolve_to_the_write_site() {
        use crate::project_store::ProjectStore;
        use std::io::Write;

        let tmp = tempfile::tempdir().unwrap();
        let m1prj = r#"<?xml version="1.0"?>
<Project>
  <Component Classname="BuiltIn.GroupCompound" Name="Root"/>
  <Component Classname="BuiltIn.GroupCompound" Name="Root.Engine"/>
  <Component Classname="BuiltIn.Channel" Name="Root.Engine.Speed"><Props Type="f32"/></Component>
  <Component Classname="BuiltIn.FuncUser" Name="Root.Engine.Write" Filename="A.m1scr"/>
  <Component Classname="BuiltIn.FuncUser" Name="Root.Engine.Read" Filename="B.m1scr"/>
</Project>"#;
        std::fs::File::create(tmp.path().join("Project.m1prj"))
            .unwrap()
            .write_all(m1prj.as_bytes())
            .unwrap();
        let a_src = "Root.Engine.Speed = 1.0;\n"; // writes (the implementation)
        let b_src = "local x = Root.Engine.Speed;\n"; // reads
        std::fs::write(tmp.path().join("A.m1scr"), a_src).unwrap();
        std::fs::write(tmp.path().join("B.m1scr"), b_src).unwrap();

        let store = ProjectStore::new();
        store.discover_and_load(tmp.path()).unwrap();

        // Cursor on the READ of Root.Engine.Speed in B → implementation jumps to
        // the producer (write) site in A, and only that — not the read itself.
        let b_uri = Url::from_file_path(tmp.path().join("B.m1scr")).unwrap();
        let byte = b_src.find("Root").unwrap();
        let no_open = |_: &Url| None;
        let locs = store
            .with_project(|p| {
                let lp = p.unwrap();
                project_implementations(
                    &lp.project,
                    &lp.script_files,
                    &b_uri,
                    b_src,
                    byte,
                    PositionEncoding::Utf16,
                    &no_open,
                )
            })
            .expect("implementation resolves to the producer site");

        assert_eq!(locs.len(), 1, "exactly one write site: {locs:?}");
        assert!(
            locs[0].uri.path().ends_with("A.m1scr"),
            "implementation is the write in A, got {locs:?}"
        );
    }

    #[test]
    fn project_references_span_scripts_without_filename_attributes() {
        // Real-corpus shape: the `.m1prj` carries no `Filename=` attributes, so
        // the script set must come from the filesystem, not the symbol table.
        use crate::project_store::ProjectStore;
        use std::io::Write;

        let tmp = tempfile::tempdir().unwrap();
        let m1prj = r#"<?xml version="1.0"?>
<Project>
  <Component Classname="BuiltIn.GroupCompound" Name="Root"/>
  <Component Classname="BuiltIn.GroupCompound" Name="Root.Engine"/>
  <Component Classname="BuiltIn.Channel" Name="Root.Engine.Speed"><Props Type="f32"/></Component>
  <Component Classname="BuiltIn.MethodUser" Name="Root.Engine.Write"/>
  <Component Classname="BuiltIn.MethodUser" Name="Root.Engine.Read"/>
</Project>"#;
        std::fs::File::create(tmp.path().join("Project.m1prj"))
            .unwrap()
            .write_all(m1prj.as_bytes())
            .unwrap();
        // Scripts live in a subdirectory (the walk recurses), named by the
        // path-encoding convention.
        let scripts = tmp.path().join("Scripts");
        std::fs::create_dir_all(&scripts).unwrap();
        let a_src = "Root.Engine.Speed = 1.0;\n";
        let b_src = "local x = Root.Engine.Speed;\n";
        std::fs::write(scripts.join("Engine.Write.m1scr"), a_src).unwrap();
        std::fs::write(scripts.join("Engine.Read.m1scr"), b_src).unwrap();

        let store = ProjectStore::new();
        store.discover_and_load(tmp.path()).unwrap();
        // Precondition: the symbol-table list is empty (no `Filename=`), so this
        // only works via the filesystem walk.
        store.with_project(|p| {
            assert!(
                p.unwrap()
                    .project
                    .symbols()
                    .iter()
                    .all(|s| s.filename.is_none()),
                "this fixture must have no Filename attributes"
            );
        });

        let a_uri = Url::from_file_path(scripts.join("Engine.Write.m1scr")).unwrap();
        let no_open = |_: &Url| None;
        let locs = store
            .with_project(|p| {
                let lp = p.unwrap();
                project_references(
                    &lp.project,
                    &lp.script_files,
                    &a_uri,
                    a_src,
                    0,
                    PositionEncoding::Utf16,
                    &no_open,
                )
            })
            .expect("references across files");

        let files: std::collections::BTreeSet<_> =
            locs.iter().map(|l| l.uri.path().to_string()).collect();
        assert_eq!(
            files.len(),
            2,
            "references should span both scripts: {locs:?}"
        );
        assert!(
            locs.iter()
                .any(|l| l.uri.path().ends_with("Engine.Write.m1scr"))
        );
        assert!(
            locs.iter()
                .any(|l| l.uri.path().ends_with("Engine.Read.m1scr"))
        );
    }

    #[test]
    fn highlights_canonicalize_mixed_spellings_in_one_file() {
        // #143: a channel written group-relative and read full-path within the
        // SAME file should highlight as one entity.
        use crate::project_store::ProjectStore;
        use std::io::Write;

        let tmp = tempfile::tempdir().unwrap();
        let m1prj = r#"<?xml version="1.0"?>
<Project>
  <Component Classname="BuiltIn.GroupCompound" Name="Root"/>
  <Component Classname="BuiltIn.GroupCompound" Name="Root.Engine"/>
  <Component Classname="BuiltIn.Channel" Name="Root.Engine.Speed"><Props Type="f32"/></Component>
  <Component Classname="BuiltIn.FuncUser" Name="Root.Engine.Calc" Filename="C.m1scr"/>
</Project>"#;
        std::fs::File::create(tmp.path().join("Project.m1prj"))
            .unwrap()
            .write_all(m1prj.as_bytes())
            .unwrap();
        let c_src = "Speed = 1.0;\nlocal x = Root.Engine.Speed;\n";
        std::fs::write(tmp.path().join("C.m1scr"), c_src).unwrap();
        let store = ProjectStore::new();
        store.discover_and_load(tmp.path()).unwrap();

        store.with_project(|p| {
            let lp = p.unwrap();
            let cst = m1_core::parse(c_src);
            let li = LineIndex::new(c_src);
            let hl = document_highlights_scoped(
                Some(&lp.project),
                Some("C.m1scr"),
                cst.root(),
                c_src.find("Speed").unwrap(),
                &li,
                PositionEncoding::Utf16,
            )
            .expect("highlights");
            assert_eq!(
                hl.len(),
                2,
                "group-relative write + full-path read should both highlight: {hl:?}"
            );
        });
    }
}
