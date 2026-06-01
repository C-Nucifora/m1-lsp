//! textDocument/rename + prepareRename.
//!
//! Two tiers of renameable thing:
//!
//!  * **Locals** — file-scoped identifiers (`local x`). Rewritten in-buffer, as
//!    before. Member-access properties (`Foo.count`) and type-annotation names
//!    (`<Count>`) are left alone.
//!
//!  * **Project leaf symbols** (#27) — a *childless* channel / parameter /
//!    constant / reference declared in `Project.m1prj`. Renamed **semantically**:
//!    every reference that *resolves* to the symbol is rewritten across every
//!    `.m1scr` in the project, plus the `<Component Name="…">` declaration in the
//!    `.m1prj`. Resolution is hierarchy-aware — absolute, `Root.`-prefixed,
//!    group-relative, and `This.`/`Parent.`-anchored, including accessor calls
//!    like `X.AsInteger` (the renamed segment is the one at the symbol's depth,
//!    so `.AsInteger` is preserved). Matching is by resolved identity, never by
//!    text, so relative references are caught and unrelated same-named symbols in
//!    other groups are left untouched.
//!
//! Out of scope (refused with a message, never a silent partial edit): groups /
//! objects / tables; functions and methods and DBC signals (file-backed —
//! renaming them means renaming their backing file); and compound symbols that
//! have children (would require a cascading rename of every descendant path).
use crate::convert::range as to_range;
use crate::features::locate::{collect_locals, node_at_byte, path_at_byte};
use crate::line_index::{LineIndex, PositionEncoding};
use crate::project_store::LoadedProject;
use m1_core::{Field, Kind, Node};
use m1_typecheck::project::Project;
use m1_typecheck::resolve::{Resolution, Scope, resolve};
use m1_typecheck::symbols::{Symbol, SymbolKind, SymbolTable};
use std::collections::{HashMap, HashSet};
use tower_lsp::lsp_types::{PrepareRenameResponse, TextEdit, Url, WorkspaceEdit};

// ---------------------------------------------------------------------------
// Locals (file-scoped) — unchanged behaviour.
// ---------------------------------------------------------------------------

/// True when `n` is the `property` half of a `member_expression` (the part after
/// the `.`), which is a channel/field access — never a local.
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

/// An identifier that refers to the local named `name` (declaration or reference).
fn is_local_ref(n: Node, name: &str) -> bool {
    n.kind() == Kind::Identifier
        && n.text() == name
        && !is_member_property(n)
        && !in_type_annotation(n)
}

/// The renameable local identifier under `byte`, if any.
fn local_ident_at(root: Node, byte: usize) -> Option<Node> {
    let node = node_at_byte(root, byte)?;
    if node.kind() != Kind::Identifier || is_member_property(node) || in_type_annotation(node) {
        return None;
    }
    if collect_locals(root).contains_key(node.text()) {
        Some(node)
    } else {
        None
    }
}

/// A local name must be a bare identifier: a leading letter/underscore, then
/// letters/digits/underscores. (Locals never contain spaces.)
pub fn is_valid_identifier(name: &str) -> bool {
    let mut chars = name.chars();
    matches!(chars.next(), Some(c) if c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

fn collect_local_edits(
    n: Node,
    name: &str,
    new_name: &str,
    li: &LineIndex,
    enc: PositionEncoding,
    out: &mut Vec<TextEdit>,
) {
    if is_local_ref(n, name) {
        out.push(TextEdit {
            range: to_range(&n.byte_range(), li, enc),
            new_text: new_name.to_string(),
        });
    }
    for c in n.children() {
        collect_local_edits(c, name, new_name, li, enc, out);
    }
}

// ---------------------------------------------------------------------------
// Project leaf symbols — hierarchy-aware semantic resolution.
// ---------------------------------------------------------------------------

/// A valid M1 symbol leaf name: non-empty, no surrounding space, and free of the
/// characters that are structural in a path (`.`) or unsafe in the `.m1prj` XML.
/// Spaces *are* allowed — M1 leaf names commonly contain them (`Drive State`).
pub fn is_valid_symbol_name(name: &str) -> bool {
    !name.is_empty()
        && name == name.trim()
        && !name.contains(['.', '"', '<', '>', '&', '\n', '\r', '\t'])
}

/// `("Root.Engine", "Speed")` for `"Root.Engine.Speed"`; `(None, path)` if the
/// path has no `.` (a bare root, which a project symbol never is in practice).
fn split_leaf(path: &str) -> (Option<&str>, &str) {
    match path.rsplit_once('.') {
        Some((parent, leaf)) => (Some(parent), leaf),
        None => (None, path),
    }
}

fn parent_of(path: &str) -> Option<&str> {
    path.rsplit_once('.').map(|(p, _)| p)
}

/// The enclosing group of the script `file_name`, for group-relative and
/// anchor resolution. Prefers the project's own `file → group` map (populated
/// from explicit `Filename=` attributes); falls back to the filename
/// **convention** — `Engine.Update.m1scr` is the body of `Root.Engine.Update`,
/// whose group is `Root.Engine` — verified against the symbol table so a
/// non-conforming name yields `None` rather than a guess. (The real corpus omits
/// `Filename=`, so the convention is the path that actually resolves there.)
fn group_for(project: &Project, file_name: Option<&str>) -> Option<String> {
    let file_name = file_name?;
    if let Some(g) = project.group_for_script(file_name) {
        return Some(g);
    }
    let base = file_name.rsplit(['/', '\\']).next().unwrap_or(file_name);
    let stem = base
        .strip_suffix(".m1scr")
        .or_else(|| base.strip_suffix(".M1SCR"))
        .unwrap_or(base);
    let candidate = match stem.rsplit_once('.') {
        Some((group_rel, _)) => format!("Root.{group_rel}"),
        None => "Root".to_string(),
    };
    project
        .symbols()
        .get(&candidate)
        .filter(|s| matches!(s.kind, SymbolKind::Group))
        .map(|_| candidate)
}

/// The resolution scope for `root` (the parsed script), in the context of
/// `project` and the script's `file_name`. Like `locate::build_scope` but with
/// the filename-convention group fallback above, so relative references resolve
/// even when the `.m1prj` carries no `Filename=` attributes.
fn scope_for<'p>(root: Node, project: &'p Project, file_name: Option<&str>) -> Scope<'p> {
    Scope {
        locals: collect_locals(root),
        group: group_for(project, file_name),
        project: Some(project),
    }
}

/// Resolve a `This.`/`Parent.`-anchored path to the symbol it denotes. `This` is
/// the enclosing group; each leading `Parent` climbs one group higher. Returns
/// `None` for a non-anchored path, a missing group, or an anchor with no tail
/// (which names the group/compound itself, not a leaf).
fn resolve_anchored<'p>(
    path: &str,
    group: Option<&str>,
    table: &'p SymbolTable,
) -> Option<&'p Symbol> {
    let parts: Vec<&str> = path.split('.').collect();
    if !matches!(parts.first(), Some(&"This") | Some(&"Parent")) {
        return None;
    }
    let group = group?;
    let mut base = group.to_string();
    let mut i = 0;
    while let Some(seg) = parts.get(i) {
        match *seg {
            "This" => base = group.to_string(),
            "Parent" => base = parent_of(&base)?.to_string(),
            _ => break,
        }
        i += 1;
    }
    let tail = parts[i..].join(".");
    if tail.is_empty() {
        return None;
    }
    table.get(&format!("{base}.{tail}"))
}

/// Resolve a dotted `path` to the project symbol it denotes, covering every M1
/// reference form: locals shadow (returning `None`), `This.`/`Parent.` anchors,
/// then the shared resolver (absolute, `Root.`-prefixed, group-relative).
fn resolve_to_symbol<'p>(path: &str, scope: &Scope<'p>) -> Option<&'p Symbol> {
    let project = scope.project?;
    if let Some(sym) = resolve_anchored(path, scope.group.as_deref(), project.symbols()) {
        return Some(sym);
    }
    match resolve(path, scope) {
        Resolution::Symbol(s) => Some(s),
        _ => None,
    }
}

/// The longest *prefix* of `path` (by dotted segment) that resolves to a symbol,
/// with the number of segments in that prefix. This is what lets accessor calls
/// be rewritten correctly: for `Threshold.AsInteger` the prefix `Threshold`
/// resolves (the full path is an opaque accessor), so the renamed segment is the
/// 1st, not the trailing `AsInteger`.
fn resolve_prefix<'p>(path: &str, scope: &Scope<'p>) -> Option<(&'p Symbol, usize)> {
    let parts: Vec<&str> = path.split('.').collect();
    for k in (1..=parts.len()).rev() {
        let prefix = parts[..k].join(".");
        if let Some(sym) = resolve_to_symbol(&prefix, scope) {
            return Some((sym, k));
        }
    }
    None
}

/// The identifier nodes of a dotted-path node, leftmost first. For
/// `Root.Engine.Speed` this is `[Root, Engine, Speed]`; for a bare `Speed` it is
/// `[Speed]`. Anchors are ordinary segments here (`[This, Speed]`).
fn segment_nodes(top: Node) -> Vec<Node> {
    fn rec<'a>(n: Node<'a>, out: &mut Vec<Node<'a>>) {
        if n.kind() == Kind::MemberExpression {
            if let Some(obj) = n.child_by_field(Field::Object) {
                rec(obj, out);
            }
            if let Some(prop) = n.child_by_field(Field::Property) {
                out.push(prop);
            }
        } else {
            out.push(n);
        }
    }
    let mut out = Vec::new();
    rec(top, &mut out);
    out
}

/// True for the outermost node of a dotted path (an `identifier` /
/// `member_expression` not itself the child of a `member_expression`), excluding
/// type-annotation names.
fn is_top_path(n: Node) -> bool {
    matches!(n.kind(), Kind::Identifier | Kind::MemberExpression)
        && n.parent()
            .map(|p| p.kind() != Kind::MemberExpression)
            .unwrap_or(true)
        && !in_type_annotation(n)
}

/// Collect the edits in one parsed script that rewrite every reference resolving
/// to `target_path`, changing only the segment at the symbol's depth.
fn collect_ref_edits(
    root: Node,
    target_path: &str,
    new_name: &str,
    scope: &Scope,
    li: &LineIndex,
    enc: PositionEncoding,
) -> Vec<TextEdit> {
    fn walk(
        n: Node,
        target_path: &str,
        new_name: &str,
        scope: &Scope,
        li: &LineIndex,
        enc: PositionEncoding,
        out: &mut Vec<TextEdit>,
    ) {
        if is_top_path(n)
            && let Some((sym, k)) = resolve_prefix(n.text(), scope)
            && sym.path == target_path
            && let Some(seg) = segment_nodes(n).get(k - 1)
        {
            out.push(TextEdit {
                range: to_range(&seg.byte_range(), li, enc),
                new_text: new_name.to_string(),
            });
        }
        for c in n.children() {
            walk(c, target_path, new_name, scope, li, enc, out);
        }
    }
    let mut out = Vec::new();
    walk(root, target_path, new_name, scope, li, enc, &mut out);
    out
}

/// The TextEdit that rewrites the leaf of `Name="<target_path>"` in the `.m1prj`
/// text to `new_name`, touching only the leaf segment within the attribute.
fn m1prj_name_edit(
    prj_text: &str,
    target_path: &str,
    old_leaf: &str,
    new_name: &str,
    enc: PositionEncoding,
) -> Option<TextEdit> {
    // The closing quote in the needle prevents matching a longer path that has
    // `target_path` as a prefix (e.g. `…Speed` vs `…Speed.Value`).
    let needle = format!("Name=\"{target_path}\"");
    let idx = prj_text.find(&needle)?;
    let leaf_start = idx + "Name=\"".len() + (target_path.len() - old_leaf.len());
    let leaf_end = leaf_start + old_leaf.len();
    let li = LineIndex::new(prj_text);
    Some(TextEdit {
        range: to_range(&(leaf_start..leaf_end), &li, enc),
        new_text: new_name.to_string(),
    })
}

/// All `*.m1scr` files under `root`, recursively. Robust to a `.m1prj` that
/// carries no `Filename=` attributes (the real-corpus case), where the project's
/// own file list is empty.
fn walk_m1scr(root: &std::path::Path) -> Vec<std::path::PathBuf> {
    fn walk(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for e in entries.flatten() {
            let p = e.path();
            if p.is_dir() {
                walk(&p, out);
            } else if p.extension().and_then(|x| x.to_str()) == Some("m1scr") {
                out.push(p);
            }
        }
    }
    let mut out = Vec::new();
    walk(root, &mut out);
    out.sort();
    out
}

/// `(uri, text)` for every project script: the cursor file first (always, using
/// its open buffer), then every other `*.m1scr` under the project root, deduped
/// by URI, preferring open buffers over disk.
fn project_scripts(
    loaded: &LoadedProject,
    cursor_uri: &Url,
    open_text: &dyn Fn(&Url) -> Option<String>,
) -> Vec<(Url, String)> {
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    let read = |u: &Url| -> Option<String> {
        open_text(u).or_else(|| {
            u.to_file_path()
                .ok()
                .and_then(|p| std::fs::read_to_string(p).ok())
        })
    };
    if let Some(t) = read(cursor_uri) {
        seen.insert(cursor_uri.clone());
        out.push((cursor_uri.clone(), t));
    }
    for p in walk_m1scr(&loaded.root) {
        let Ok(u) = Url::from_file_path(&p) else {
            continue;
        };
        if seen.contains(&u) {
            continue;
        }
        if let Some(t) = open_text(&u).or_else(|| std::fs::read_to_string(&p).ok()) {
            seen.insert(u.clone());
            out.push((u, t));
        }
    }
    out
}

fn file_name_of(uri: &Url) -> Option<String> {
    uri.to_file_path()
        .ok()?
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
}

/// The eligible project leaf symbol the cursor resolves to, with the project's
/// resolution scope for the cursor file. `Err` carries the user-facing reason a
/// resolved symbol is *not* renameable; `Ok(None)` means "not a project symbol"
/// (the caller falls back to / has already tried the local path).
fn cursor_leaf<'p>(
    root: Node,
    byte: usize,
    project: &'p Project,
    file_name: Option<&str>,
) -> Result<Option<&'p Symbol>, String> {
    let Some((_, path)) = path_at_byte(root, byte) else {
        return Ok(None);
    };
    let scope = scope_for(root, project, file_name);
    let Some((sym, _)) = resolve_prefix(&path, &scope) else {
        return Ok(None);
    };
    if sym.filename.is_some() {
        return Err(format!(
            "‘{}’ is defined in its own file; renaming file-backed symbols (functions, methods, DBC signals) is not supported",
            sym.path
        ));
    }
    if !matches!(
        sym.kind,
        SymbolKind::Channel | SymbolKind::Parameter | SymbolKind::Constant | SymbolKind::Reference
    ) {
        return Err(format!(
            "‘{}’ is a {:?}; only channels, parameters, constants and references can be renamed",
            sym.path, sym.kind
        ));
    }
    let children = project.symbols().immediate_children(&sym.path).len();
    if children > 0 {
        return Err(format!(
            "‘{}’ has {children} child component(s); renaming a compound symbol would require a cascading rename (not yet supported) — rename its leaf members individually",
            sym.path
        ));
    }
    Ok(Some(sym))
}

// ---------------------------------------------------------------------------
// Public entry points (dispatch local vs project).
// ---------------------------------------------------------------------------

/// prepareRename: the editable range under `byte` — the local identifier, or the
/// leaf segment of a renameable project symbol. `None` if nothing renameable is
/// here (the client then shows "cannot rename").
pub fn prepare(
    root: Node,
    byte: usize,
    li: &LineIndex,
    enc: PositionEncoding,
    project: Option<&Project>,
    file_name: Option<&str>,
) -> Option<PrepareRenameResponse> {
    if let Some(node) = local_ident_at(root, byte) {
        return Some(PrepareRenameResponse::Range(to_range(
            &node.byte_range(),
            li,
            enc,
        )));
    }
    let project = project?;
    // Only offer a range when the symbol is actually renameable.
    cursor_leaf(root, byte, project, file_name).ok()??;
    let (top, path) = path_at_byte(root, byte)?;
    let scope = scope_for(root, project, file_name);
    let (_, k) = resolve_prefix(&path, &scope)?;
    let seg = segment_nodes(top).into_iter().nth(k - 1)?;
    Some(PrepareRenameResponse::Range(to_range(
        &seg.byte_range(),
        li,
        enc,
    )))
}

/// textDocument/rename. Locals are rewritten in-buffer; project leaf symbols are
/// renamed semantically across the workspace (and the `.m1prj` declaration).
/// `Err(msg)` is surfaced to the user (e.g. "‘X’ has children …").
#[allow(clippy::too_many_arguments)]
pub fn execute(
    root: Node,
    byte: usize,
    new_name: &str,
    uri: Url,
    li: &LineIndex,
    enc: PositionEncoding,
    loaded: Option<&LoadedProject>,
    file_name: Option<&str>,
    open_text: &dyn Fn(&Url) -> Option<String>,
) -> Result<WorkspaceEdit, String> {
    // Local rename: file-scoped, in-buffer.
    if let Some(node) = local_ident_at(root, byte) {
        if !is_valid_identifier(new_name) {
            return Err(format!(
                "‘{new_name}’ is not a valid local name (letters, digits, underscore; no leading digit or spaces)"
            ));
        }
        let name = node.text().to_string();
        let mut edits = Vec::new();
        collect_local_edits(root, &name, new_name, li, enc, &mut edits);
        let mut changes = HashMap::new();
        changes.insert(uri, edits);
        return Ok(WorkspaceEdit {
            changes: Some(changes),
            document_changes: None,
            change_annotations: None,
        });
    }

    let Some(loaded) = loaded else {
        return Err(
            "no renameable symbol here — only `local` variables can be renamed without a project"
                .to_string(),
        );
    };
    let project = &loaded.project;
    let Some(sym) = cursor_leaf(root, byte, project, file_name)? else {
        return Err(
            "no renameable symbol here — place the cursor on a local, channel, parameter, constant or reference"
                .to_string(),
        );
    };

    let new_name = new_name.trim();
    if !is_valid_symbol_name(new_name) {
        return Err(format!(
            "‘{new_name}’ is not a valid M1 symbol name (letters, digits, spaces, underscore; no dots or quotes)"
        ));
    }
    let target_path = sym.path.clone();
    let (parent, old_leaf) = split_leaf(&target_path);
    let new_full = match parent {
        Some(p) => format!("{p}.{new_name}"),
        None => new_name.to_string(),
    };
    if new_name == old_leaf {
        return Err("the new name is the same as the current name".to_string());
    }
    if project.symbols().get(&new_full).is_some() {
        return Err(format!(
            "a symbol named ‘{new_name}’ already exists at ‘{new_full}’"
        ));
    }

    let mut changes: HashMap<Url, Vec<TextEdit>> = HashMap::new();

    // 1) The `.m1prj` declaration (`Name="…<old_leaf>"`).
    let prj_uri = Url::from_file_path(&loaded.m1prj_path)
        .map_err(|_| "cannot form a URL for the project file".to_string())?;
    let prj_text = open_text(&prj_uri)
        .or_else(|| std::fs::read_to_string(&loaded.m1prj_path).ok())
        .ok_or_else(|| "cannot read the project file".to_string())?;
    let prj_edit =
        m1prj_name_edit(&prj_text, &target_path, old_leaf, new_name, enc).ok_or_else(|| {
            format!("could not locate the declaration of ‘{target_path}’ in the project file")
        })?;
    changes.entry(prj_uri).or_default().push(prj_edit);

    // 2) Every resolving reference across every script.
    for (su, stext) in project_scripts(loaded, &uri, open_text) {
        let scst = m1_core::parse(&stext);
        let sli = LineIndex::new(&stext);
        let sfname = file_name_of(&su);
        let sscope = scope_for(scst.root(), project, sfname.as_deref());
        let edits = collect_ref_edits(scst.root(), &target_path, new_name, &sscope, &sli, enc);
        if !edits.is_empty() {
            changes.entry(su).or_default().extend(edits);
        }
    }

    Ok(WorkspaceEdit {
        changes: Some(changes),
        document_changes: None,
        change_annotations: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::project_store::ProjectStore;
    use std::io::Write;

    fn url() -> Url {
        Url::parse("file:///t.m1scr").unwrap()
    }

    // ---- locals -----------------------------------------------------------

    fn local_edits(src: &str, at: &str, new: &str) -> Vec<TextEdit> {
        let cst = m1_core::parse(src);
        let li = LineIndex::new(src);
        let byte = src.find(at).unwrap();
        let no_open = |_: &Url| None;
        execute(
            cst.root(),
            byte,
            new,
            url(),
            &li,
            PositionEncoding::Utf16,
            None,
            None,
            &no_open,
        )
        .map(|e| e.changes.unwrap().into_values().next().unwrap())
        .unwrap_or_default()
    }

    #[test]
    fn renames_all_local_occurrences() {
        let edits = local_edits("local count = 0;\ncount = count + 1;\n", "count", "total");
        assert_eq!(edits.len(), 3, "declaration + two references");
        assert!(edits.iter().all(|e| e.new_text == "total"));
    }

    #[test]
    fn local_rename_ignores_same_named_member_property() {
        let edits = local_edits(
            "local count = 0;\nFoo.count = 1;\ncount = count + 1;\n",
            "count",
            "total",
        );
        assert_eq!(edits.len(), 3);
    }

    #[test]
    fn rejects_invalid_local_name() {
        let cst = m1_core::parse("local count = 0;\n");
        let li = LineIndex::new("local count = 0;\n");
        let no_open = |_: &Url| None;
        let err = execute(
            cst.root(),
            "local count".find("count").unwrap() + 6,
            "9bad",
            url(),
            &li,
            PositionEncoding::Utf16,
            None,
            None,
            &no_open,
        );
        assert!(err.is_err());
    }

    #[test]
    fn validates_names() {
        assert!(is_valid_identifier("total"));
        assert!(!is_valid_identifier("has space"));
        assert!(is_valid_symbol_name("Drive State")); // spaces OK for symbols
        assert!(!is_valid_symbol_name("a.b")); // dots are structural
        assert!(!is_valid_symbol_name("")); // empty
        assert!(!is_valid_symbol_name(" pad")); // surrounding space
    }

    // ---- project leaf rename ---------------------------------------------

    // A project where `Threshold` is a childless parameter under `Root.Engine`,
    // with a sibling group `Root.Other` that has its *own* `Threshold`. No
    // `Filename=` attributes — group resolution must use the filename convention.
    const PRJ: &str = r#"<?xml version="1.0"?>
<Project>
  <Component Classname="BuiltIn.GroupCompound" Name="Root"/>
  <Component Classname="BuiltIn.GroupCompound" Name="Root.Engine"/>
  <Component Classname="BuiltIn.Parameter" Name="Root.Engine.Threshold"><Props Type="f32"/></Component>
  <Component Classname="BuiltIn.Channel" Name="Root.Engine.Speed"><Props Type="f32"/></Component>
  <Component Classname="BuiltIn.Channel" Name="Root.Engine.Speed.Value"><Props Type="f32"/></Component>
  <Component Classname="BuiltIn.GroupCompound" Name="Root.Other"/>
  <Component Classname="BuiltIn.Parameter" Name="Root.Other.Threshold"><Props Type="f32"/></Component>
  <Component Classname="BuiltIn.MethodUser" Name="Root.Engine.Update"/>
  <Component Classname="BuiltIn.MethodUser" Name="Root.Other.Update"/>
</Project>"#;

    fn setup() -> (tempfile::TempDir, ProjectStore) {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::File::create(tmp.path().join("Project.m1prj"))
            .unwrap()
            .write_all(PRJ.as_bytes())
            .unwrap();
        let store = ProjectStore::new();
        store.discover_and_load(tmp.path()).unwrap();
        (tmp, store)
    }

    fn changes_for(we: &WorkspaceEdit, ends_with: &str) -> Vec<TextEdit> {
        we.changes
            .as_ref()
            .unwrap()
            .iter()
            .find(|(u, _)| u.path().ends_with(ends_with))
            .map(|(_, e)| e.clone())
            .unwrap_or_default()
    }

    #[test]
    fn renames_project_leaf_across_scripts_and_m1prj() {
        let (tmp, store) = setup();
        // Owner Root.Engine.Update -> group Root.Engine (derived from filename).
        let a = "local a = Engine.Threshold;\nlocal b = Threshold;\nlocal c = This.Threshold;\nlocal d = Threshold.AsInteger;\n";
        // Owner Root.Other.Update -> group Root.Other. The absolute ref hits the
        // Engine symbol; the bare `Threshold` resolves to Root.Other.Threshold
        // (a *different* symbol) and must NOT be touched.
        let b = "local e = Root.Engine.Threshold;\nlocal f = Threshold;\n";
        std::fs::write(tmp.path().join("Engine.Update.m1scr"), a).unwrap();
        std::fs::write(tmp.path().join("Other.Update.m1scr"), b).unwrap();

        let a_uri = Url::from_file_path(tmp.path().join("Engine.Update.m1scr")).unwrap();
        let cst = m1_core::parse(a);
        let li = LineIndex::new(a);
        let byte = a.find("Threshold").unwrap(); // on `Engine.Threshold`
        let no_open = |_: &Url| None;

        let we = store
            .with_project(|p| {
                execute(
                    cst.root(),
                    byte,
                    "Trip Point",
                    a_uri.clone(),
                    &li,
                    PositionEncoding::Utf16,
                    p,
                    Some("Engine.Update.m1scr"),
                    &no_open,
                )
            })
            .expect("rename should succeed");

        // .m1prj: exactly the leaf renamed.
        let prj = changes_for(&we, "Project.m1prj");
        assert_eq!(prj.len(), 1, "one declaration edit");
        assert_eq!(prj[0].new_text, "Trip Point");

        // Engine.Update.m1scr: all four references rewritten (incl. accessor).
        let ae = changes_for(&we, "Engine.Update.m1scr");
        assert_eq!(
            ae.len(),
            4,
            "Engine.Threshold, Threshold, This.Threshold, Threshold.AsInteger"
        );
        assert!(ae.iter().all(|e| e.new_text == "Trip Point"));

        // Other.Update.m1scr: only the absolute Engine ref; the bare local-group
        // `Threshold` belongs to Root.Other.Threshold and is left alone.
        let be = changes_for(&we, "Other.Update.m1scr");
        assert_eq!(be.len(), 1, "only the absolute Root.Engine.Threshold ref");
    }

    #[test]
    fn refuses_compound_with_children() {
        let (tmp, store) = setup();
        let _ = &tmp;
        // `Root.Engine.Speed` is a channel that has a `.Value` child -> a rename
        // would have to cascade, so it is refused.
        let src = "Engine.Speed = 1.0;\n";
        let cst = m1_core::parse(src);
        let li = LineIndex::new(src);
        let byte = src.find("Speed").unwrap();
        let no_open = |_: &Url| None;
        let res = store.with_project(|p| {
            execute(
                cst.root(),
                byte,
                "Motor",
                url(),
                &li,
                PositionEncoding::Utf16,
                p,
                Some("Engine.Update.m1scr"),
                &no_open,
            )
        });
        let err = res.unwrap_err();
        assert!(err.contains("child"), "got: {err}");
    }

    #[test]
    fn refuses_name_collision() {
        let (tmp, store) = setup();
        // Rename Root.Engine.Threshold to a name that already exists there.
        // (Add a sibling to collide with.)
        std::fs::write(
            tmp.path().join("Project.m1prj"),
            PRJ.replace(
                r#"<Component Classname="BuiltIn.Parameter" Name="Root.Engine.Threshold"><Props Type="f32"/></Component>"#,
                r#"<Component Classname="BuiltIn.Parameter" Name="Root.Engine.Threshold"><Props Type="f32"/></Component>
  <Component Classname="BuiltIn.Parameter" Name="Root.Engine.Limit"><Props Type="f32"/></Component>"#,
            ),
        )
        .unwrap();
        store.discover_and_load(tmp.path()).unwrap();

        let src = "Engine.Threshold = 1.0;\n";
        let cst = m1_core::parse(src);
        let li = LineIndex::new(src);
        let byte = src.find("Threshold").unwrap();
        let no_open = |_: &Url| None;
        let res = store.with_project(|p| {
            execute(
                cst.root(),
                byte,
                "Limit",
                url(),
                &li,
                PositionEncoding::Utf16,
                p,
                Some("Engine.Update.m1scr"),
                &no_open,
            )
        });
        assert!(res.unwrap_err().contains("already exists"));
    }

    #[test]
    fn prepare_offers_leaf_for_project_symbol_rejects_group() {
        let (tmp, store) = setup();
        let _ = tmp;
        let src = "Engine.Threshold = 1.0;\n";
        let cst = m1_core::parse(src);
        let li = LineIndex::new(src);
        let enc = PositionEncoding::Utf16;
        store.with_project(|p| {
            let project = p.map(|lp| &lp.project);
            // On the leaf -> Some range; on the group segment -> None.
            let on_leaf = prepare(
                cst.root(),
                src.find("Threshold").unwrap(),
                &li,
                enc,
                project,
                Some("Engine.Update.m1scr"),
            );
            assert!(on_leaf.is_some(), "leaf symbol is renameable");
            // A compound channel (has a `.Value` child) offers no rename range.
            let compound = "Engine.Speed = 1.0;\n";
            let cst2 = m1_core::parse(compound);
            let li2 = LineIndex::new(compound);
            let on_compound = prepare(
                cst2.root(),
                compound.find("Speed").unwrap(),
                &li2,
                enc,
                project,
                Some("Engine.Update.m1scr"),
            );
            assert!(
                on_compound.is_none(),
                "the compound channel is not renameable"
            );
        });
    }
}
