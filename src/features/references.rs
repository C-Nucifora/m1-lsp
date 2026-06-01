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
use crate::features::locate::{collect_locals, node_at_byte, path_at_byte};
use crate::line_index::{LineIndex, PositionEncoding};
use crate::project_store::LoadedProject;
use m1_core::{Field, Kind, Node};
use tower_lsp::lsp_types::{DocumentHighlight, DocumentHighlightKind, Location, Url};

/// True when `n` is the `property` half of a `member_expression` (after the `.`).
fn is_member_property(n: Node) -> bool {
    n.parent()
        .filter(|p| p.kind() == Kind::MemberExpression)
        .and_then(|p| p.child_by_field(Field::Property))
        .map(|prop| prop.byte_range() == n.byte_range())
        .unwrap_or(false)
}

fn in_type_annotation(n: Node) -> bool {
    let mut cur = n;
    while let Some(p) = cur.parent() {
        if p.kind() == Kind::TypeAnnotation {
            return true;
        }
        cur = p;
    }
    false
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
fn collect_local_idents<'a>(n: Node<'a>, name: &str, out: &mut Vec<Node<'a>>) {
    if n.kind() == Kind::Identifier
        && n.text() == name
        && !is_member_property(n)
        && !in_type_annotation(n)
    {
        out.push(n);
    }
    for c in n.children() {
        collect_local_idents(c, name, out);
    }
}

/// Every top-level path node whose dotted text equals `path`.
fn collect_path_matches<'a>(n: Node<'a>, path: &str, out: &mut Vec<Node<'a>>) {
    let is_path = matches!(n.kind(), Kind::Identifier | Kind::MemberExpression);
    let is_top = n
        .parent()
        .map(|p| p.kind() != Kind::MemberExpression)
        .unwrap_or(true);
    if is_path && is_top && !in_type_annotation(n) && n.text() == path {
        out.push(n);
    }
    for c in n.children() {
        collect_path_matches(c, path, out);
    }
}

/// Nodes in `root` that refer to the same entity as the cursor at `byte`.
fn occurrences<'a>(root: Node<'a>, byte: usize) -> Vec<Node<'a>> {
    let Some(node) = node_at_byte(root, byte) else {
        return Vec::new();
    };
    // A bare identifier that names a known local: precise, name-based match.
    if node.kind() == Kind::Identifier
        && !is_member_property(node)
        && !in_type_annotation(node)
        && collect_locals(root).contains_key(node.text())
    {
        let mut out = Vec::new();
        collect_local_idents(root, node.text(), &mut out);
        return out;
    }
    // Otherwise match the full dotted path (channel / project symbol / library
    // member) by text.
    let Some((_, path)) = path_at_byte(root, byte) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    collect_path_matches(root, &path, &mut out);
    out
}

/// True when `n` (or the path it tops) is being written: the target of an
/// assignment or the name of a `local` declaration.
fn is_write(n: Node) -> bool {
    let top = top_path_node(n);
    match top.parent() {
        Some(p) if p.kind() == Kind::AssignmentStatement => p
            .child_by_field(Field::Target)
            .map(|t| t.byte_range() == top.byte_range())
            .unwrap_or(false),
        Some(p) if p.kind() == Kind::LocalDeclaration => p
            .child_by_field(Field::Name)
            .map(|name| name.byte_range() == n.byte_range())
            .unwrap_or(false),
        _ => false,
    }
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

/// Classify the cursor target, mirroring `occurrences`' local-vs-path branching.
pub fn ref_target(root: Node, byte: usize) -> Option<RefTarget> {
    let node = node_at_byte(root, byte)?;
    if node.kind() == Kind::Identifier
        && !is_member_property(node)
        && !in_type_annotation(node)
        && collect_locals(root).contains_key(node.text())
    {
        return Some(RefTarget::Local(node.text().to_string()));
    }
    let (_, path) = path_at_byte(root, byte)?;
    Some(RefTarget::Path(path))
}

/// All Locations of the dotted `path` within one already-parsed file.
fn path_locations(
    root: Node,
    path: &str,
    uri: &Url,
    li: &LineIndex,
    enc: PositionEncoding,
) -> Vec<Location> {
    let mut nodes = Vec::new();
    collect_path_matches(root, path, &mut nodes);
    nodes
        .into_iter()
        .map(|n| Location {
            uri: uri.clone(),
            range: range(&n.byte_range(), li, enc),
        })
        .collect()
}

/// Project-wide references (#29). A local stays file-local; a project symbol is
/// searched across every `.m1scr` in the workspace. The script set is taken from
/// the filesystem (a real `.m1prj` carries no `Filename=` attributes, so the
/// symbol-table list would be empty). `open_text` supplies the in-memory buffer
/// for a file when one is open (newer than disk); files not open are read from
/// disk. The cursor's own file is always included.
pub fn project_references(
    loaded: &LoadedProject,
    cursor_uri: &Url,
    cursor_text: &str,
    byte: usize,
    enc: PositionEncoding,
    open_text: &dyn Fn(&Url) -> Option<String>,
) -> Option<Vec<Location>> {
    let cursor_cst = m1_core::parse(cursor_text);
    match ref_target(cursor_cst.root(), byte)? {
        // Locals are file-scoped: reuse the single-file finder.
        RefTarget::Local(_) => {
            let li = LineIndex::new(cursor_text);
            references(cursor_cst.root(), byte, cursor_uri.clone(), &li, enc)
        }
        RefTarget::Path(path) => {
            // Gather (uri, text) for the cursor file first, then every other
            // project script (deduped by uri), preferring open buffers.
            let mut files: Vec<(Url, String)> = vec![(cursor_uri.clone(), cursor_text.to_string())];
            for p in &loaded.script_files {
                let Ok(uri) = Url::from_file_path(p) else {
                    continue;
                };
                if files.iter().any(|(u, _)| *u == uri) {
                    continue;
                }
                if let Some(t) = open_text(&uri).or_else(|| std::fs::read_to_string(p).ok()) {
                    files.push((uri, t));
                }
            }
            let mut locs = Vec::new();
            for (uri, text) in &files {
                let li = LineIndex::new(text);
                let cst = m1_core::parse(text);
                locs.extend(path_locations(cst.root(), &path, uri, &li, enc));
            }
            (!locs.is_empty()).then_some(locs)
        }
    }
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
                project_references(
                    p.unwrap(),
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
                project_references(
                    p.unwrap(),
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
}
