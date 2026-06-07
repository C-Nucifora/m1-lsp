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
//!  * **Groups / objects** (#72) — a compound container. Renamed by a
//!    **cascade**: the group segment is rewritten in the `.m1prj` for the group
//!    *and every descendant* `Name="…"`, in every resolving reference across the
//!    scripts (only references that textually spell the group segment — relative
//!    and `This.`/`Parent.`-anchored ones stay valid once the file is renamed),
//!    and the convention-named backing scripts of method/func descendants are
//!    renamed via bundled `RenameFile` operations. Refused (the whole op) only
//!    when a backing script can't be located — never a silent partial edit. The
//!    edit is emitted as `document_changes` so it can carry the file renames.
//!
//! Out of scope (refused with a message): file-backed symbols (functions,
//! methods, DBC signals — renaming them means renaming their own backing file);
//! and a value-bearing channel/parameter that itself has children (rename its
//! leaf members individually).
use crate::convert::range as to_range;
use crate::features::locate::{
    build_scope, collect_locals, node_at_byte, path_at_byte, segment_at_byte, segment_nodes,
};
use crate::line_index::{LineIndex, PositionEncoding};
use crate::project_store::LoadedProject;
use m1_core::{Field, Kind, Node};
use m1_typecheck::project::Project;
use m1_typecheck::resolve::{Resolution, Scope, resolve};
use m1_typecheck::symbols::{Symbol, SymbolKind, SymbolTable};
use std::collections::{HashMap, HashSet};
use tower_lsp::lsp_types::{
    DocumentChangeOperation, DocumentChanges, OneOf, OptionalVersionedTextDocumentIdentifier,
    PrepareRenameResponse, RenameFile, ResourceOp, TextDocumentEdit, TextEdit, Url, WorkspaceEdit,
};

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

/// A local name: a leading letter/underscore, then letters/digits/underscores,
/// and — like other M1 names — optional *internal* spaces (`Torque Request`), but
/// no leading/trailing space and no leading digit (#148).
pub fn is_valid_identifier(name: &str) -> bool {
    if name.is_empty() || name != name.trim() {
        return false;
    }
    let mut chars = name.chars();
    matches!(chars.next(), Some(c) if c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == ' ')
}

fn collect_local_edits(
    root: Node,
    name: &str,
    new_name: &str,
    li: &LineIndex,
    enc: PositionEncoding,
    out: &mut Vec<TextEdit>,
) {
    // Iterative pre-order traversal (m1-core's `descendants`) rather than
    // recursion, so a pathologically deep tree can't overflow the stack (#133).
    for n in root.descendants() {
        if is_local_ref(n, name) {
            out.push(TextEdit {
                range: to_range(&n.byte_range(), li, enc),
                new_text: new_name.to_string(),
            });
        }
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

/// The resolution scope for `root` (the parsed script), in the context of
/// `project` and the script's `file_name`. The enclosing group comes from
/// `group_for_script`, which m1-typecheck now derives by the filename convention
/// when the `.m1prj` carries no `Filename=` attributes — so group-relative and
/// `This.`/`Parent.`-anchored references resolve on real projects.
fn scope_for<'p>(root: Node, project: &'p Project, file_name: Option<&str>) -> Scope<'p> {
    build_scope(root, Some(project), file_name)
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
    // Iterative pre-order traversal (m1-core's `descendants`) rather than
    // recursion, so a pathologically deep tree can't overflow the stack (#133).
    let mut out = Vec::new();
    for n in root.descendants() {
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
    }
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
                .and_then(|p| crate::disk_read::read_disk(&p))
        })
    };
    if let Some(t) = read(cursor_uri) {
        seen.insert(cursor_uri.clone());
        out.push((cursor_uri.clone(), t));
    }
    for p in &loaded.script_files {
        let Ok(u) = Url::from_file_path(p) else {
            continue;
        };
        if seen.contains(&u) {
            continue;
        }
        if let Some(t) = open_text(&u).or_else(|| crate::disk_read::read_disk(p)) {
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

/// What the cursor is renaming. The *segment the cursor sits on* decides: the
/// prefix up to that segment resolves either to a childless leaf (ordinary
/// semantic rename) or to a group/object container (cascading rename of the
/// segment across the group and all its descendants, plus backing-file renames).
enum Target<'p> {
    Leaf(&'p Symbol),
    Group(&'p Symbol),
}

/// Decide renameability of `sym` (independent of which entry point found it).
/// `Ok(None)` is never returned here — callers map "no symbol" themselves; this
/// returns the [`Target`] or the user-facing reason it can't be renamed.
fn classify<'p>(project: &'p Project, sym: &'p Symbol) -> Result<Target<'p>, String> {
    if sym.filename.is_some() {
        return Err(format!(
            "‘{}’ is defined in its own file; renaming file-backed symbols (functions, methods, DBC signals) is not supported",
            sym.path
        ));
    }
    let children = project.symbols().immediate_children(&sym.path).len();
    match sym.kind {
        // A group/object container cascades: the segment is renamed across every
        // descendant path and the convention-named backing scripts.
        SymbolKind::Group | SymbolKind::Object if children > 0 => Ok(Target::Group(sym)),
        SymbolKind::Channel
        | SymbolKind::Parameter
        | SymbolKind::Constant
        | SymbolKind::Reference
            if children == 0 =>
        {
            Ok(Target::Leaf(sym))
        }
        // A value-bearing channel/parameter that itself has children is the
        // residual case the cascade can't safely fold in — refuse explicitly.
        _ if children > 0 => Err(format!(
            "‘{}’ has {children} child component(s); rename its leaf members individually",
            sym.path
        )),
        _ => Err(format!(
            "‘{}’ is a {:?}; only channels, parameters, constants, references and groups can be renamed",
            sym.path, sym.kind
        )),
    }
}

/// The rename target under `byte`: resolve the path prefix *up to the segment the
/// cursor is on* (so the cursor on `Engine` in `Engine.Speed` targets the group,
/// while the cursor on `Speed` targets the leaf), then classify it. `Ok(None)`
/// means "no project symbol here" (the caller falls back to the local path).
fn cursor_target<'p>(
    root: Node,
    byte: usize,
    project: &'p Project,
    file_name: Option<&str>,
) -> Result<Option<Target<'p>>, String> {
    let Some((top, _)) = path_at_byte(root, byte) else {
        return Ok(None);
    };
    let scope = scope_for(root, project, file_name);
    let segs = segment_nodes(top);
    let i = segment_at_byte(top, byte).unwrap_or(segs.len().saturating_sub(1));
    let prefix = segs[..=i.min(segs.len().saturating_sub(1))]
        .iter()
        .map(|n| n.text())
        .collect::<Vec<_>>()
        .join(".");
    let Some((sym, _)) = resolve_prefix(&prefix, &scope) else {
        return Ok(None);
    };
    classify(project, sym).map(Some)
}

// ---------------------------------------------------------------------------
// Group / compound cascade (#72).
// ---------------------------------------------------------------------------

/// One `TextDocumentEdit` (unversioned) for a file's edits, for `document_changes`.
fn text_doc_edit(uri: Url, edits: Vec<TextEdit>) -> DocumentChangeOperation {
    DocumentChangeOperation::Edit(TextDocumentEdit {
        text_document: OptionalVersionedTextDocumentIdentifier { uri, version: None },
        edits: edits.into_iter().map(OneOf::Left).collect(),
    })
}

/// The new absolute path for a descendant `path` of `group_path` when the group's
/// leaf is renamed to `new_name` (`Root.Engine.Speed` + `Root.Engine`→`Motor` ⇒
/// `Root.Motor.Speed`). The group leaf is the only segment that changes.
fn rename_group_segment(path: &str, group_path: &str, old_leaf: &str, new_name: &str) -> String {
    let head_len = group_path.len() - old_leaf.len(); // up to and including the trailing `.`
    format!(
        "{}{}{}",
        &path[..head_len],
        new_name,
        &path[group_path.len()..]
    )
}

/// `.m1prj` edits renaming the group segment in the group's own `Name="…"` and in
/// every descendant `Name="…<group>.…"`. Scans the `Name="` attribute text
/// directly (the LSP carries no XML dependency), editing only the group segment.
fn m1prj_group_edits(
    prj_text: &str,
    group_path: &str,
    old_leaf: &str,
    new_name: &str,
    enc: PositionEncoding,
) -> Vec<TextEdit> {
    let li = LineIndex::new(prj_text);
    let prefix = format!("{group_path}.");
    let head = group_path.len() - old_leaf.len();
    const NEEDLE: &str = "Name=\"";
    let mut out = Vec::new();
    let mut search = 0;
    while let Some(rel) = prj_text[search..].find(NEEDLE) {
        let val_start = search + rel + NEEDLE.len();
        let Some(qrel) = prj_text[val_start..].find('"') else {
            break;
        };
        let val_end = val_start + qrel;
        let val = &prj_text[val_start..val_end];
        if val == group_path || val.starts_with(&prefix) {
            let seg_start = val_start + head;
            let seg_end = seg_start + old_leaf.len();
            out.push(TextEdit {
                range: to_range(&(seg_start..seg_end), &li, enc),
                new_text: new_name.to_string(),
            });
        }
        search = val_end + 1;
    }
    out
}

/// Script edits rewriting every reference that resolves to the group *or any
/// descendant*, changing only the group segment. The renamed segment is the one
/// at the group's depth within the reference; relative/anchored references that
/// don't spell the group segment are left untouched (their resolution stays valid
/// once the backing file is renamed). The text guard (`== old_leaf`) makes this
/// safe regardless of the index arithmetic.
fn collect_group_ref_edits(
    root: Node,
    group_path: &str,
    old_leaf: &str,
    new_name: &str,
    scope: &Scope,
    li: &LineIndex,
    enc: PositionEncoding,
) -> Vec<TextEdit> {
    let group_depth = group_path.split('.').count() - 1;
    let prefix = format!("{group_path}.");
    // Iterative pre-order traversal (m1-core's `descendants`) rather than
    // recursion, so a pathologically deep tree can't overflow the stack (#133).
    let mut out = Vec::new();
    for n in root.descendants() {
        if is_top_path(n)
            && let Some((sym, k)) = resolve_prefix(n.text(), scope)
            && (sym.path == group_path || sym.path.starts_with(&prefix))
        {
            let sym_last = sym.path.split('.').count() - 1;
            let j = sym_last - group_depth; // segments from the leaf up to the group
            if let Some(idx) = (k.checked_sub(1)).and_then(|kk| kk.checked_sub(j))
                && let Some(seg) = segment_nodes(n).get(idx)
                && seg.text() == old_leaf
            {
                out.push(TextEdit {
                    range: to_range(&seg.byte_range(), li, enc),
                    new_text: new_name.to_string(),
                });
            }
        }
    }
    out
}

/// `RenameFile` operations for the convention-named (no explicit `Filename=`)
/// method/func scripts under the group, whose derived filename embeds the group
/// segment. Refuses (the whole rename) if any such script can't be located on
/// disk — renaming the group without renaming its file would silently break the
/// script's group-relative references, which we never do.
/// True for a symbol backed by a user-authored `.m1scr` (so a missing file is a
/// real problem), as opposed to a firmware/auto-generated method that never has
/// one. Used to decide whether a missing backing file should refuse a group
/// rename or just be skipped (#147).
fn is_user_authored_script(sym: &m1_typecheck::symbols::Symbol) -> bool {
    matches!(
        sym.classname.as_deref(),
        Some(c) if c.starts_with("BuiltIn.FuncUser")
            || c.starts_with("BuiltIn.CalFuncUser")
            || c == "BuiltIn.MethodUser"
    )
}

fn group_file_renames(
    loaded: &LoadedProject,
    project: &Project,
    group_path: &str,
    old_leaf: &str,
    new_name: &str,
) -> Result<Vec<RenameFile>, String> {
    let prefix = format!("{group_path}.");
    let mut out = Vec::new();
    for sym in project.symbols().iter() {
        if !matches!(sym.kind, SymbolKind::Function | SymbolKind::Method) {
            continue;
        }
        if !sym.path.starts_with(&prefix) {
            continue;
        }
        // An explicit Filename doesn't encode the group, so it needs no rename.
        if sym.filename.is_some() {
            continue;
        }
        // Derived basename convention: the path minus the `Root.` prefix + `.m1scr`.
        let rel = sym.path.strip_prefix("Root.").unwrap_or(&sym.path);
        let old_base = format!("{rel}.m1scr");
        let new_path = rename_group_segment(&sym.path, group_path, old_leaf, new_name);
        let new_rel = new_path.strip_prefix("Root.").unwrap_or(&new_path);
        let new_base = format!("{new_rel}.m1scr");
        let Some(disk) = loaded.script_files.iter().find(|p| {
            p.file_name()
                .map(|f| f == old_base.as_str())
                .unwrap_or(false)
        }) else {
            // Firmware/auto-generated methods (FuncGenerated, IO methods) are not
            // backed by a user `.m1scr` — there is no file to rename, so skip them.
            // Only a genuine user-authored script (FuncUser/CalFuncUser/MethodUser)
            // with a missing file is a real hazard worth refusing for (#147).
            if is_user_authored_script(sym) {
                return Err(format!(
                    "‘{}’ has no locatable backing script ({old_base}); rename that file before renaming the group so references aren't silently broken",
                    sym.path
                ));
            }
            continue;
        };
        let (Ok(old_uri), Ok(new_uri)) = (
            Url::from_file_path(disk),
            Url::from_file_path(disk.with_file_name(&new_base)),
        ) else {
            continue;
        };
        out.push(RenameFile {
            old_uri,
            new_uri,
            options: None,
            annotation_id: None,
        });
    }
    Ok(out)
}

/// Execute a group/compound cascade rename: the group segment in the `.m1prj`
/// (group + every descendant declaration), every resolving reference across all
/// scripts, and `RenameFile` ops for the convention-named backing scripts.
fn execute_group(
    group: &Symbol,
    new_name: &str,
    cursor_uri: &Url,
    enc: PositionEncoding,
    loaded: &LoadedProject,
    open_text: &dyn Fn(&Url) -> Option<String>,
) -> Result<WorkspaceEdit, String> {
    let project = &loaded.project;
    let group_path = group.path.clone();
    let (parent, old_leaf) = split_leaf(&group_path);
    if new_name == old_leaf {
        return Err("the new name is the same as the current name".to_string());
    }
    let new_full = match parent {
        Some(p) => format!("{p}.{new_name}"),
        None => new_name.to_string(),
    };
    if project.symbols().get(&new_full).is_some() {
        return Err(format!(
            "a symbol named ‘{new_name}’ already exists at ‘{new_full}’"
        ));
    }

    // Backing-file renames first: this is the step that can refuse the whole op.
    let renames = group_file_renames(loaded, project, &group_path, old_leaf, new_name)?;

    let mut ops: Vec<DocumentChangeOperation> = Vec::new();

    // 1) `.m1prj`: the group + descendant declarations.
    let prj_uri = Url::from_file_path(&loaded.m1prj_path)
        .map_err(|_| "cannot form a URL for the project file".to_string())?;
    let prj_text = open_text(&prj_uri)
        .or_else(|| crate::disk_read::read_disk(&loaded.m1prj_path))
        .ok_or_else(|| "cannot read the project file".to_string())?;
    let prj_edits = m1prj_group_edits(&prj_text, &group_path, old_leaf, new_name, enc);
    if prj_edits.is_empty() {
        return Err(format!(
            "could not locate the declaration of ‘{group_path}’ in the project file"
        ));
    }
    ops.push(text_doc_edit(prj_uri, prj_edits));

    // 2) References across every script (text edits use the current URIs).
    for (su, stext) in project_scripts(loaded, cursor_uri, open_text) {
        let scst = m1_core::parse(&stext);
        let sli = LineIndex::new(&stext);
        let sfname = file_name_of(&su);
        let sscope = scope_for(scst.root(), project, sfname.as_deref());
        let edits = collect_group_ref_edits(
            scst.root(),
            &group_path,
            old_leaf,
            new_name,
            &sscope,
            &sli,
            enc,
        );
        if !edits.is_empty() {
            ops.push(text_doc_edit(su, edits));
        }
    }

    // 3) Then the file renames (applied after the text edits above).
    for rf in renames {
        ops.push(DocumentChangeOperation::Op(ResourceOp::Rename(rf)));
    }

    Ok(WorkspaceEdit {
        changes: None,
        document_changes: Some(DocumentChanges::Operations(ops)),
        change_annotations: None,
    })
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
    let target = cursor_target(root, byte, project, file_name).ok()??;
    let (top, path) = path_at_byte(root, byte)?;
    // The editable segment: the leaf's resolving segment, or — for a group — the
    // group segment the cursor is on.
    let seg_idx = match target {
        Target::Group(_) => segment_at_byte(top, byte)?,
        Target::Leaf(_) => {
            let scope = scope_for(root, project, file_name);
            let (_, k) = resolve_prefix(&path, &scope)?;
            k - 1
        }
    };
    let seg = segment_nodes(top).into_iter().nth(seg_idx)?;
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
                "‘{new_name}’ is not a valid local name (letters, digits, underscore and internal spaces; no leading digit or surrounding space)"
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
    let target = match cursor_target(root, byte, project, file_name)? {
        Some(t) => t,
        None => {
            return Err(
                "no renameable symbol here — place the cursor on a local, channel, parameter, constant, reference or group"
                    .to_string(),
            );
        }
    };

    let new_name = new_name.trim();
    if !is_valid_symbol_name(new_name) {
        return Err(format!(
            "‘{new_name}’ is not a valid M1 symbol name (letters, digits, spaces, underscore; no dots or quotes)"
        ));
    }

    // A group/object container cascades across the workspace + backing files.
    let sym = match target {
        Target::Group(g) => return execute_group(g, new_name, &uri, enc, loaded, open_text),
        Target::Leaf(s) => s,
    };
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
        .or_else(|| crate::disk_read::read_disk(&loaded.m1prj_path))
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

// ---------------------------------------------------------------------------
// Rename initiated from within the `.m1prj` itself.
//
// Channels/parameters are *declared* in `Project.m1prj` (`<Component Name="…">`),
// so editing that file is the natural place to rename one. The clients only
// attach the LSP to `.m1scr`, but when the project file is opened as a document
// these entry points let a rename on a component's `Name` propagate the same way
// a script-initiated rename does: the declaration leaf in the `.m1prj` plus every
// resolving reference across all scripts. Only renameable leaves are offered
// (childless channel/parameter/constant/reference); groups/objects/compounds are
// the cascading case and stay out of scope.
// ---------------------------------------------------------------------------

/// The component `Name="…"` value whose span contains `byte`, as
/// `(full_path, leaf_byte_range)`. Scans for the `Name="` attribute rather than
/// parsing XML — the attribute is unambiguous and this keeps the LSP free of an
/// XML dependency.
fn name_attr_at(text: &str, byte: usize) -> Option<(String, std::ops::Range<usize>)> {
    const NEEDLE: &str = "Name=\"";
    let mut search = 0;
    while let Some(rel) = text[search..].find(NEEDLE) {
        let val_start = search + rel + NEEDLE.len();
        let val_end = val_start + text[val_start..].find('"')?;
        if byte >= val_start && byte <= val_end {
            let path = text[val_start..val_end].to_string();
            let leaf_off = path.rfind('.').map(|i| i + 1).unwrap_or(0);
            return Some((path, (val_start + leaf_off)..val_end));
        }
        search = val_end + 1;
    }
    None
}

/// Renameability for a symbol identified by path (shared shape with `cursor_leaf`,
/// which works from a script node): a childless, non-file-backed channel /
/// parameter / constant / reference.
fn symbol_renameable<'p>(project: &'p Project, path: &str) -> Result<&'p Symbol, String> {
    let Some(sym) = project.symbols().get(path) else {
        return Err(format!("‘{path}’ is not a renameable project symbol"));
    };
    if sym.filename.is_some() {
        return Err(format!(
            "‘{}’ is file-backed (function/method/DBC signal); renaming it is not supported",
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
            "‘{}’ has {children} child component(s); renaming a compound would require a cascading rename (not yet supported)",
            sym.path
        ));
    }
    Ok(sym)
}

/// prepareRename for a `.m1prj` document: the editable leaf range when the cursor
/// is on a renameable component's `Name`.
pub fn prepare_m1prj(
    prj_text: &str,
    byte: usize,
    enc: PositionEncoding,
    project: Option<&Project>,
) -> Option<PrepareRenameResponse> {
    let project = project?;
    let (path, leaf_range) = name_attr_at(prj_text, byte)?;
    symbol_renameable(project, &path).ok()?;
    let li = LineIndex::new(prj_text);
    Some(PrepareRenameResponse::Range(to_range(
        &leaf_range,
        &li,
        enc,
    )))
}

/// textDocument/rename for a `.m1prj` document: rename the component under the
/// cursor across its declaration and every resolving reference in every script.
#[allow(clippy::too_many_arguments)]
pub fn execute_m1prj(
    prj_text: &str,
    byte: usize,
    new_name: &str,
    prj_uri: Url,
    enc: PositionEncoding,
    loaded: &LoadedProject,
    open_text: &dyn Fn(&Url) -> Option<String>,
) -> Result<WorkspaceEdit, String> {
    let project = &loaded.project;
    let (target_path, _) = name_attr_at(prj_text, byte)
        .ok_or_else(|| "place the cursor on a component Name in the project file".to_string())?;
    symbol_renameable(project, &target_path)?;

    let new_name = new_name.trim();
    if !is_valid_symbol_name(new_name) {
        return Err(format!(
            "‘{new_name}’ is not a valid M1 symbol name (letters, digits, spaces, underscore; no dots or quotes)"
        ));
    }
    let (parent, old_leaf) = split_leaf(&target_path);
    if new_name == old_leaf {
        return Err("the new name is the same as the current name".to_string());
    }
    let new_full = match parent {
        Some(p) => format!("{p}.{new_name}"),
        None => new_name.to_string(),
    };
    if project.symbols().get(&new_full).is_some() {
        return Err(format!(
            "a symbol named ‘{new_name}’ already exists at ‘{new_full}’"
        ));
    }

    let mut changes: HashMap<Url, Vec<TextEdit>> = HashMap::new();

    // 1) The declaration leaf in the `.m1prj` (using the open buffer text).
    let prj_edit =
        m1prj_name_edit(prj_text, &target_path, old_leaf, new_name, enc).ok_or_else(|| {
            format!("could not locate the declaration of ‘{target_path}’ in the project file")
        })?;
    changes.entry(prj_uri).or_default().push(prj_edit);

    // 2) Every resolving reference across every script (the `.m1prj` is not a
    //    script, so it is not part of this walk — its edit is added above).
    for p in &loaded.script_files {
        let Ok(su) = Url::from_file_path(p) else {
            continue;
        };
        let Some(stext) = open_text(&su).or_else(|| crate::disk_read::read_disk(p)) else {
            continue;
        };
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

/// Apply the text edits in `we` that target `uri` to `text`, returning the new
/// content (`None` if the edit doesn't touch `uri`). Reads edits from both the
/// `changes` map and `document_changes`. Used to derive the post-rename
/// `.m1prj` text so the LSP can refresh its symbol model without waiting for the
/// client to write the file back to disk.
pub fn apply_workspace_edit_to(
    we: &WorkspaceEdit,
    uri: &Url,
    text: &str,
    enc: PositionEncoding,
) -> Option<String> {
    let mut edits: Vec<&TextEdit> = Vec::new();
    if let Some(changes) = &we.changes
        && let Some(es) = changes.get(uri)
    {
        edits.extend(es.iter());
    }
    match &we.document_changes {
        Some(DocumentChanges::Operations(ops)) => {
            for op in ops {
                if let DocumentChangeOperation::Edit(tde) = op
                    && &tde.text_document.uri == uri
                {
                    edits.extend(tde.edits.iter().map(|e| match e {
                        OneOf::Left(te) => te,
                        OneOf::Right(ate) => &ate.text_edit,
                    }));
                }
            }
        }
        Some(DocumentChanges::Edits(tdes)) => {
            for tde in tdes {
                if &tde.text_document.uri == uri {
                    edits.extend(tde.edits.iter().map(|e| match e {
                        OneOf::Left(te) => te,
                        OneOf::Right(ate) => &ate.text_edit,
                    }));
                }
            }
        }
        None => {}
    }
    if edits.is_empty() {
        return None;
    }
    let li = LineIndex::new(text);
    let mut byte_edits: Vec<(usize, usize, &str)> = edits
        .iter()
        .map(|e| {
            (
                li.offset(e.range.start, text, enc),
                li.offset(e.range.end, text, enc),
                e.new_text.as_str(),
            )
        })
        .collect();
    // Apply right-to-left so earlier offsets stay valid as we splice.
    byte_edits.sort_by_key(|&(start, ..)| std::cmp::Reverse(start));
    let mut out = text.to_string();
    for (start, end, new_text) in byte_edits {
        if start <= end && end <= out.len() {
            out.replace_range(start..end, new_text);
        }
    }
    Some(out)
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
    fn renames_a_multi_word_local() {
        // #148: M1 locals may contain spaces (`local Torque Request`); renaming
        // one to another multi-word name must succeed, not be rejected.
        let src = "local Torque Request = 0;\nTorque Request = Torque Request + 1;\n";
        let cst = m1_core::parse(src);
        let li = LineIndex::new(src);
        let no_open = |_: &Url| None;
        let byte = src.find("Torque Request").unwrap() + 6; // inside the name
        let edit = execute(
            cst.root(),
            byte,
            "Brake Force",
            url(),
            &li,
            PositionEncoding::Utf16,
            None,
            None,
            &no_open,
        )
        .expect("multi-word local rename should succeed");
        let edits = edit.changes.unwrap().into_values().next().unwrap();
        assert_eq!(edits.len(), 3, "declaration + two references: {edits:?}");
        assert!(edits.iter().all(|e| e.new_text == "Brake Force"));
    }

    #[test]
    fn validates_names() {
        assert!(is_valid_identifier("total"));
        assert!(is_valid_identifier("Torque Request")); // internal spaces OK (#148)
        assert!(!is_valid_identifier("9bad")); // no leading digit
        assert!(!is_valid_identifier(" pad")); // no surrounding space
        assert!(!is_valid_identifier("a.b")); // dots are structural, not a local
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
        // Reload so the scripts (written after the initial load) are in the
        // cached `script_files` set the workspace search walks.
        store.discover_and_load(tmp.path()).unwrap();

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

    // #125: a disk-sourced script that is NOT valid UTF-8 (a Windows-1252 `°`
    // = 0xB0 in a comment) must still be decoded, parsed, and included in the
    // rename's WorkspaceEdit — previously `read_to_string(p).ok()` turned the
    // bad-encoding read into `None` and silently dropped the file, leaving its
    // occurrences un-renamed.
    #[test]
    fn renames_into_non_utf8_script() {
        let (tmp, store) = setup();
        // The renamed symbol lives under Root.Engine; this third script's owner
        // is Root.Engine.Update so a bare `Engine.Threshold` resolves to it.
        let a = "local a = Engine.Threshold;\n";
        std::fs::write(tmp.path().join("Engine.Update.m1scr"), a).unwrap();
        // A SECOND script, owned by Root.Other.Update, containing a lone 0xB0
        // byte in a comment (Windows-1252 `°`) — invalid UTF-8 — plus an
        // absolute reference to the renamed Root.Engine.Threshold symbol.
        let mut b: Vec<u8> = Vec::new();
        b.extend_from_slice(b"// temp in \xb0C threshold\n");
        b.extend_from_slice(b"local g = Root.Engine.Threshold;\n");
        std::fs::write(tmp.path().join("Other.Update.m1scr"), &b).unwrap();
        // Sanity: the file really is not UTF-8, so the old read path would drop it.
        assert!(
            std::fs::read_to_string(tmp.path().join("Other.Update.m1scr")).is_err(),
            "the fixture must be non-UTF-8 for this test to be meaningful"
        );
        store.discover_and_load(tmp.path()).unwrap();

        let a_uri = Url::from_file_path(tmp.path().join("Engine.Update.m1scr")).unwrap();
        let cst = m1_core::parse(a);
        let li = LineIndex::new(a);
        let byte = a.find("Threshold").unwrap();
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

        // The non-UTF-8 script must be present in the WorkspaceEdit with its one
        // absolute reference rewritten — not silently absent.
        let be = changes_for(&we, "Other.Update.m1scr");
        assert_eq!(
            be.len(),
            1,
            "the non-UTF-8 script's Root.Engine.Threshold reference must be renamed, \
             not silently dropped: {be:?}"
        );
        assert_eq!(be[0].new_text, "Trip Point");
    }

    // #74: renaming from a *reference* (read) site must also rewrite the
    // *definition* (write/assignment-target) site and the `.m1prj` declaration —
    // otherwise the editor looks correct but M1-Build, which re-reads from disk
    // and the component list, sees the old name. The leaf rename matches by
    // resolved identity, so a write target and a read of the same symbol are both
    // rewritten.
    #[test]
    fn rename_from_reference_also_rewrites_definition_and_m1prj() {
        let (tmp, store) = setup();
        // Line 0 is a WRITE (assignment target = the definition for M1-Build);
        // line 1 is a READ. Cursor is placed on the READ.
        let src = "Engine.Threshold = 1.0;\nlocal x = Engine.Threshold;\n";
        std::fs::write(tmp.path().join("Engine.Update.m1scr"), src).unwrap();
        store.discover_and_load(tmp.path()).unwrap();

        let uri = Url::from_file_path(tmp.path().join("Engine.Update.m1scr")).unwrap();
        let cst = m1_core::parse(src);
        let li = LineIndex::new(src);
        // Cursor on the second occurrence (the read), not the definition.
        let byte = src.rfind("Threshold").unwrap();
        let no_open = |_: &Url| None;

        let we = store
            .with_project(|p| {
                execute(
                    cst.root(),
                    byte,
                    "Trip Point",
                    uri.clone(),
                    &li,
                    PositionEncoding::Utf16,
                    p,
                    Some("Engine.Update.m1scr"),
                    &no_open,
                )
            })
            .expect("rename should succeed");

        // Both occurrences in the script are rewritten — including the write on
        // line 0 (the definition M1-Build compiles from).
        let edits = changes_for(&we, "Engine.Update.m1scr");
        assert_eq!(
            edits.len(),
            2,
            "the write (definition) and the read must both be rewritten: {edits:?}"
        );
        assert!(edits.iter().all(|e| e.new_text == "Trip Point"));
        assert!(
            edits.iter().any(|e| e.range.start.line == 0),
            "the definition/write site on line 0 must be in the edit: {edits:?}"
        );
        // And the `.m1prj` component declaration is updated too.
        let prj = changes_for(&we, "Project.m1prj");
        assert_eq!(prj.len(), 1, "the .m1prj declaration is renamed: {prj:?}");
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

    // #119 + the nvim "undefined until restart" report: after a rename rewrites
    // Project.m1prj, applying the edit back to the project text must yield the new
    // declaration, and reloading the store from that text must make the renamed
    // symbol immediately live (no disk round-trip, no server restart).
    #[test]
    fn post_rename_m1prj_text_makes_renamed_symbol_live_on_reload() {
        let (tmp, store) = setup();
        std::fs::write(
            tmp.path().join("Engine.Update.m1scr"),
            "Engine.Threshold = 1.0;\n",
        )
        .unwrap();
        store.discover_and_load(tmp.path()).unwrap();

        let prj_path = tmp.path().join("Project.m1prj");
        let prj_text = std::fs::read_to_string(&prj_path).unwrap();
        let prj_uri = Url::from_file_path(&prj_path).unwrap();

        // Rename the leaf parameter Root.Engine.Threshold -> Trip Point.
        let src = "Engine.Threshold = 1.0;\n";
        let cst = m1_core::parse(src);
        let li = LineIndex::new(src);
        let byte = src.find("Threshold").unwrap();
        let enc = PositionEncoding::Utf16;
        let no_open = |_: &Url| None;
        let we = store
            .with_project(|p| {
                execute(
                    cst.root(),
                    byte,
                    "Trip Point",
                    Url::from_file_path(tmp.path().join("Engine.Update.m1scr")).unwrap(),
                    &li,
                    enc,
                    p,
                    Some("Engine.Update.m1scr"),
                    &no_open,
                )
            })
            .expect("rename should succeed");

        // Derive the post-rename project text and reload the model from it.
        let new_prj = apply_workspace_edit_to(&we, &prj_uri, &prj_text, enc)
            .expect("the rename must touch Project.m1prj");
        assert!(new_prj.contains("Root.Engine.Trip Point"), "got: {new_prj}");
        assert!(!new_prj.contains("Root.Engine.Threshold"), "got: {new_prj}");

        assert!(store.reload_from_m1prj_text(&new_prj).unwrap());
        store.with_project(|p| {
            let t = p.unwrap().project.symbols();
            assert!(
                t.get("Root.Engine.Trip Point").is_some(),
                "renamed symbol must be live without a restart"
            );
            assert!(t.get("Root.Engine.Threshold").is_none());
        });
    }

    // ---- group / compound cascade (#72) ----------------------------------

    /// Pull the `RenameFile` ops out of a `document_changes` workspace edit.
    fn rename_files(we: &WorkspaceEdit) -> Vec<(String, String)> {
        match we.document_changes.as_ref() {
            Some(DocumentChanges::Operations(ops)) => ops
                .iter()
                .filter_map(|op| match op {
                    DocumentChangeOperation::Op(ResourceOp::Rename(rf)) => Some((
                        rf.old_uri.path().rsplit('/').next().unwrap().to_string(),
                        rf.new_uri.path().rsplit('/').next().unwrap().to_string(),
                    )),
                    _ => None,
                })
                .collect(),
            _ => Vec::new(),
        }
    }

    /// Text edits for the file whose URI ends with `ends_with`, from a
    /// `document_changes` edit.
    fn doc_edits_for(we: &WorkspaceEdit, ends_with: &str) -> Vec<TextEdit> {
        match we.document_changes.as_ref() {
            Some(DocumentChanges::Operations(ops)) => ops
                .iter()
                .find_map(|op| match op {
                    DocumentChangeOperation::Edit(e)
                        if e.text_document.uri.path().ends_with(ends_with) =>
                    {
                        Some(
                            e.edits
                                .iter()
                                .map(|x| match x {
                                    OneOf::Left(te) => te.clone(),
                                    OneOf::Right(ate) => ate.text_edit.clone(),
                                })
                                .collect(),
                        )
                    }
                    _ => None,
                })
                .unwrap_or_default(),
            _ => Vec::new(),
        }
    }

    #[test]
    fn renames_group_across_prj_scripts_and_backing_files() {
        let (tmp, store) = setup();
        // Engine.Update.m1scr (owned by Root.Engine.Update, derived name): mixes an
        // absolute ref, a group-relative ref, and a `This.`-anchored ref.
        let engine =
            "Engine.Threshold = 1.0;\nlocal a = Root.Engine.Speed;\nlocal b = This.Threshold;\n";
        // A consumer in another group references Engine absolutely.
        let dash = "local d = Root.Engine.Threshold;\nlocal e = Engine.Speed;\n";
        std::fs::write(tmp.path().join("Engine.Update.m1scr"), engine).unwrap();
        std::fs::write(tmp.path().join("Other.Update.m1scr"), dash).unwrap();
        store.discover_and_load(tmp.path()).unwrap();

        let uri = Url::from_file_path(tmp.path().join("Other.Update.m1scr")).unwrap();
        let cst = m1_core::parse(dash);
        let li = LineIndex::new(dash);
        // Cursor on `Engine` in `Root.Engine.Threshold` (the group segment).
        let byte = dash.find("Engine").unwrap();
        let no_open = |_: &Url| None;

        let we = store
            .with_project(|p| {
                let loaded = p.unwrap();
                execute(
                    cst.root(),
                    byte,
                    "Motor",
                    uri.clone(),
                    &li,
                    PositionEncoding::Utf16,
                    Some(loaded),
                    Some("Other.Update.m1scr"),
                    &no_open,
                )
            })
            .expect("group rename should succeed");

        // .m1prj: the group + every descendant Name segment renamed. Descendants:
        // Root.Engine itself, .Threshold, .Speed, .Speed.Value, .Update = 5.
        let prj = doc_edits_for(&we, "Project.m1prj");
        assert_eq!(prj.len(), 5, "group + 4 descendants: {prj:?}");
        assert!(prj.iter().all(|e| e.new_text == "Motor"));

        // Other.Update.m1scr: both the absolute and group-relative Engine segments.
        let other = doc_edits_for(&we, "Other.Update.m1scr");
        assert_eq!(other.len(), 2, "Root.Engine.* and Engine.Speed: {other:?}");

        // Engine.Update.m1scr: the absolute refs spell `Engine`; the `This.`-anchored
        // and the bare relative refs do not, so only the two absolute segments edit.
        let eng = doc_edits_for(&we, "Engine.Update.m1scr");
        assert_eq!(
            eng.len(),
            2,
            "Engine.Threshold + Root.Engine.Speed: {eng:?}"
        );

        // The convention-named backing script is renamed to match the new group.
        let files = rename_files(&we);
        assert_eq!(
            files,
            vec![(
                "Engine.Update.m1scr".to_string(),
                "Motor.Update.m1scr".to_string()
            )]
        );
    }

    #[test]
    fn group_rename_rewrites_references_to_descendant_methods() {
        // Renaming the group must also fix call sites of its *method* descendants
        // in other scripts (e.g. `Engine.Update()` → `Motor.Update()`), not just
        // channel references — the method is a descendant like any other.
        let (tmp, store) = setup();
        std::fs::write(
            tmp.path().join("Engine.Update.m1scr"),
            "Engine.Threshold = 1.0;\n",
        )
        .unwrap();
        let caller = "Root.Engine.Update();\nEngine.Update();\n";
        std::fs::write(tmp.path().join("Other.Update.m1scr"), caller).unwrap();
        store.discover_and_load(tmp.path()).unwrap();

        let uri = Url::from_file_path(tmp.path().join("Other.Update.m1scr")).unwrap();
        let cst = m1_core::parse(caller);
        let li = LineIndex::new(caller);
        let byte = caller.find("Engine").unwrap(); // group segment of Root.Engine.Update
        let no_open = |_: &Url| None;

        let we = store
            .with_project(|p| {
                execute(
                    cst.root(),
                    byte,
                    "Motor",
                    uri.clone(),
                    &li,
                    PositionEncoding::Utf16,
                    p,
                    Some("Other.Update.m1scr"),
                    &no_open,
                )
            })
            .expect("group rename should succeed");

        // Both call sites of the method have their `Engine` segment rewritten.
        let calls = doc_edits_for(&we, "Other.Update.m1scr");
        assert_eq!(
            calls.len(),
            2,
            "Root.Engine.Update() and Engine.Update() call sites: {calls:?}"
        );
        assert!(calls.iter().all(|e| e.new_text == "Motor"));
        // And the method's own backing file is renamed.
        assert_eq!(
            rename_files(&we),
            vec![(
                "Engine.Update.m1scr".to_string(),
                "Motor.Update.m1scr".to_string()
            )]
        );
    }

    #[test]
    fn group_rename_refuses_when_backing_file_missing() {
        let (tmp, store) = setup();
        // Engine.Update has no backing file on disk → cascade can't keep its
        // group-relative refs consistent, so the whole rename is refused.
        let src = "local d = Root.Engine.Threshold;\n";
        std::fs::write(tmp.path().join("Other.Update.m1scr"), src).unwrap();
        store.discover_and_load(tmp.path()).unwrap();

        let uri = Url::from_file_path(tmp.path().join("Other.Update.m1scr")).unwrap();
        let cst = m1_core::parse(src);
        let li = LineIndex::new(src);
        let byte = src.find("Engine").unwrap();
        let no_open = |_: &Url| None;
        let res = store.with_project(|p| {
            execute(
                cst.root(),
                byte,
                "Motor",
                uri.clone(),
                &li,
                PositionEncoding::Utf16,
                p,
                Some("Other.Update.m1scr"),
                &no_open,
            )
        });
        let err = res.unwrap_err();
        assert!(err.contains("backing script"), "got: {err}");
    }

    #[test]
    fn group_rename_skips_firmware_generated_children() {
        // #147: a top-level group whose only file-less method descendants are
        // firmware-generated (FuncGenerated/IO methods — never backed by a user
        // script) must rename, not be refused for a file those methods never have.
        let tmp = tempfile::tempdir().unwrap();
        let prj = r#"<?xml version="1.0"?>
<Project>
  <Component Classname="BuiltIn.GroupCompound" Name="Root"/>
  <Component Classname="BuiltIn.GroupCompound" Name="Root.CAN"/>
  <Component Classname="BuiltIn.Channel" Name="Root.CAN.Active"><Props Type="f32"/></Component>
  <Component Classname="BuiltIn.FuncGenerated.IO" Name="Root.CAN.Generated Method"/>
</Project>"#;
        std::fs::write(tmp.path().join("Project.m1prj"), prj).unwrap();
        let src = "local a = Root.CAN.Active;\n";
        std::fs::write(tmp.path().join("X.m1scr"), src).unwrap();
        let store = ProjectStore::new();
        store.discover_and_load(tmp.path()).unwrap();

        let uri = Url::from_file_path(tmp.path().join("X.m1scr")).unwrap();
        let cst = m1_core::parse(src);
        let li = LineIndex::new(src);
        let byte = src.find("CAN").unwrap();
        let no_open = |_: &Url| None;
        let res = store.with_project(|p| {
            execute(
                cst.root(),
                byte,
                "Comms",
                uri.clone(),
                &li,
                PositionEncoding::Utf16,
                p,
                Some("X.m1scr"),
                &no_open,
            )
        });
        let edit = res.expect("group with only firmware-generated methods should rename");
        assert!(
            edit.document_changes.is_some() || edit.changes.is_some(),
            "expected a non-empty edit: {edit:?}"
        );
    }

    #[test]
    fn prepare_offers_group_segment() {
        let (tmp, store) = setup();
        // Backing file present so the group is renameable.
        std::fs::write(
            tmp.path().join("Engine.Update.m1scr"),
            "Engine.Threshold = 1.0;\n",
        )
        .unwrap();
        store.discover_and_load(tmp.path()).unwrap();
        let src = "local d = Root.Engine.Threshold;\n";
        let cst = m1_core::parse(src);
        let li = LineIndex::new(src);
        let enc = PositionEncoding::Utf16;
        store.with_project(|p| {
            let project = p.map(|lp| &lp.project);
            // Cursor on `Engine` (the group segment) → offered.
            let on_group = prepare(
                cst.root(),
                src.find("Engine").unwrap(),
                &li,
                enc,
                project,
                Some("Other.Update.m1scr"),
            );
            assert!(on_group.is_some(), "group segment is renameable");
        });
    }

    // ---- rename initiated from the .m1prj --------------------------------

    #[test]
    fn rename_from_m1prj_propagates_to_decl_and_scripts() {
        let (tmp, store) = setup();
        // A script that references the channel we'll rename from the project file.
        let a = "local x = Engine.Threshold;\nEngine.Threshold = 1.0;\n";
        std::fs::write(tmp.path().join("Engine.Update.m1scr"), a).unwrap();
        store.discover_and_load(tmp.path()).unwrap();

        let prj_path = tmp.path().join("Project.m1prj");
        let prj_text = std::fs::read_to_string(&prj_path).unwrap();
        let prj_uri = Url::from_file_path(&prj_path).unwrap();
        // Cursor on the leaf of `Name="Root.Engine.Threshold"`.
        let byte = prj_text.find("Root.Engine.Threshold").unwrap() + "Root.Engine.".len();
        let no_open = |_: &Url| None;

        let we = store
            .with_project(|p| {
                execute_m1prj(
                    &prj_text,
                    byte,
                    "Trip Point",
                    prj_uri.clone(),
                    PositionEncoding::Utf16,
                    p.unwrap(),
                    &no_open,
                )
            })
            .expect("rename from .m1prj should succeed");

        // The .m1prj declaration leaf is renamed…
        let prj = changes_for(&we, "Project.m1prj");
        assert_eq!(prj.len(), 1, "one declaration edit");
        assert_eq!(prj[0].new_text, "Trip Point");
        // …and both references in the script are rewritten.
        let se = changes_for(&we, "Engine.Update.m1scr");
        assert_eq!(se.len(), 2, "both Engine.Threshold references: {se:?}");
        assert!(se.iter().all(|e| e.new_text == "Trip Point"));
    }

    #[test]
    fn prepare_m1prj_offers_leaf_rejects_compound() {
        let (tmp, store) = setup();
        let _ = tmp;
        store.with_project(|p| {
            let project = p.map(|lp| &lp.project);
            // On the leaf parameter declaration -> a range.
            let leaf = PRJ.find("Root.Engine.Threshold").unwrap() + 5;
            assert!(
                prepare_m1prj(PRJ, leaf, PositionEncoding::Utf16, project).is_some(),
                "leaf parameter is renameable from the .m1prj"
            );
            // On the compound channel `Root.Engine.Speed` (has a `.Value` child) -> none.
            let compound = PRJ.find("Root.Engine.Speed\"").unwrap() + 5;
            assert!(
                prepare_m1prj(PRJ, compound, PositionEncoding::Utf16, project).is_none(),
                "the compound channel is not renameable"
            );
        });
    }
}
