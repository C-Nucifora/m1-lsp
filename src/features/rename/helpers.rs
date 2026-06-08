//! Shared validators, resolution helpers, and edit builders used by the rename
//! algorithms (local, leaf, group, file-backed func/method, .m1prj-initiated).
use crate::convert::range as to_range;
use crate::features::locate::{build_scope, path_at_byte, segment_at_byte, segment_nodes};
use crate::line_index::{LineIndex, PositionEncoding};
use crate::project_store::LoadedProject;
use m1_core::Node;
use m1_typecheck::project::Project;
use m1_typecheck::resolve::{Resolution, Scope, resolve};
use m1_typecheck::symbols::{Symbol, SymbolKind, SymbolTable};
use tower_lsp::lsp_types::{
    DocumentChangeOperation, OneOf, OptionalVersionedTextDocumentIdentifier, TextDocumentEdit,
    TextEdit, Url,
};

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
pub(super) fn split_leaf(path: &str) -> (Option<&str>, &str) {
    match path.rsplit_once('.') {
        Some((parent, leaf)) => (Some(parent), leaf),
        None => (None, path),
    }
}

pub(super) fn parent_of(path: &str) -> Option<&str> {
    path.rsplit_once('.').map(|(p, _)| p)
}

/// The resolution scope for `root` (the parsed script), in the context of
/// `project` and the script's `file_name`. The enclosing group comes from
/// `group_for_script`, which m1-typecheck now derives by the filename convention
/// when the `.m1prj` carries no `Filename=` attributes — so group-relative and
/// `This.`/`Parent.`-anchored references resolve on real projects.
pub(super) fn scope_for<'p>(
    root: Node,
    project: &'p Project,
    file_name: Option<&str>,
) -> Scope<'p> {
    build_scope(root, Some(project), file_name)
}

/// Resolve a `This.`/`Parent.`-anchored path to the symbol it denotes. `This` is
/// the enclosing group; each leading `Parent` climbs one group higher. Returns
/// `None` for a non-anchored path, a missing group, or an anchor with no tail
/// (which names the group/compound itself, not a leaf).
pub(super) fn resolve_anchored<'p>(
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
pub(super) fn resolve_to_symbol<'p>(path: &str, scope: &Scope<'p>) -> Option<&'p Symbol> {
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
pub(super) fn resolve_prefix<'p>(path: &str, scope: &Scope<'p>) -> Option<(&'p Symbol, usize)> {
    let parts: Vec<&str> = path.split('.').collect();
    for k in (1..=parts.len()).rev() {
        let prefix = parts[..k].join(".");
        if let Some(sym) = resolve_to_symbol(&prefix, scope) {
            return Some((sym, k));
        }
    }
    None
}

/// Collect the edits in one parsed script that rewrite every reference resolving
/// to `target_path`, changing only the segment at the symbol's depth.
pub(super) fn collect_ref_edits(
    root: Node,
    target_path: &str,
    new_name: &str,
    scope: &Scope,
    li: &LineIndex,
    enc: PositionEncoding,
) -> Vec<TextEdit> {
    use crate::features::locate::for_each_top_path;
    // O(n) single pass ([`for_each_top_path`]): stack-safe on deep input (#133)
    // and free of the per-node parent climbs that made the old `descendants()` +
    // `is_top_path` scan O(n²).
    let mut out = Vec::new();
    for_each_top_path(root, |n, _is_write| {
        if let Some((sym, k)) = resolve_prefix(n.text(), scope)
            && sym.path == target_path
            && let Some(seg) = segment_nodes(n).get(k - 1)
        {
            out.push(TextEdit {
                range: to_range(&seg.byte_range(), li, enc),
                new_text: new_name.to_string(),
            });
        }
    });
    out
}

/// The TextEdit that rewrites the leaf of `Name="<target_path>"` in the `.m1prj`
/// text to `new_name`, touching only the leaf segment within the attribute.
pub(super) fn m1prj_name_edit(
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
/// by URI, preferring open buffers over disk. Thin wrapper over
/// `project_store::gather_project_scripts` that reads the cursor file itself
/// (rename only holds the cursor URI, not its text).
pub(super) fn project_scripts(
    loaded: &LoadedProject,
    cursor_uri: &Url,
    open_text: &dyn Fn(&Url) -> Option<String>,
) -> Vec<(Url, String)> {
    crate::project_store::gather_project_scripts(&loaded.script_files, cursor_uri, None, open_text)
}

/// `(uri, text)` for the project's `.m1prj`, preferring an open editor buffer
/// over the on-disk text. Errs (with a user-facing message) when the path can't
/// form a URL or the file can't be read — used by each cross-file rename that
/// must rewrite the project declaration.
pub(super) fn load_prj_text(
    loaded: &LoadedProject,
    open_text: &dyn Fn(&Url) -> Option<String>,
) -> std::result::Result<(Url, String), String> {
    let prj_uri = Url::from_file_path(&loaded.m1prj_path)
        .map_err(|_| "cannot form a URL for the project file".to_string())?;
    let prj_text = open_text(&prj_uri)
        .or_else(|| crate::disk_read::read_disk(&loaded.m1prj_path))
        .ok_or_else(|| "cannot read the project file".to_string())?;
    Ok((prj_uri, prj_text))
}

/// What the cursor is renaming. The *segment the cursor sits on* decides: the
/// prefix up to that segment resolves either to a childless leaf (ordinary
/// semantic rename) or to a group/object container (cascading rename of the
/// segment across the group and all its descendants, plus backing-file renames).
pub(super) enum Target<'p> {
    Leaf(&'p Symbol),
    Group(&'p Symbol),
    /// A user-authored function/method backed by its own `.m1scr` script: renamed
    /// across its `.m1prj` declaration and every call site, with the backing file
    /// moved alongside (#150).
    FileBacked(&'p Symbol),
}

/// True for a symbol backed by a user-authored `.m1scr` (so a missing file is a
/// real problem), as opposed to a firmware/auto-generated method that never has
/// one. Used to decide whether a missing backing file should refuse a rename or
/// just be skipped (#147).
pub(super) fn is_user_authored_script(sym: &Symbol) -> bool {
    matches!(
        sym.classname.as_deref(),
        Some(c) if c.starts_with("BuiltIn.FuncUser")
            || c.starts_with("BuiltIn.CalFuncUser")
            || c == "BuiltIn.MethodUser"
    )
}

/// Decide renameability of `sym` (independent of which entry point found it).
/// `Ok(None)` is never returned here — callers map "no symbol" themselves; this
/// returns the [`Target`] or the user-facing reason it can't be renamed.
pub(super) fn classify<'p>(project: &'p Project, sym: &'p Symbol) -> Result<Target<'p>, String> {
    // A user-authored function/method is renameable, with its backing `.m1scr`
    // moved alongside (#150). This holds whether the script is convention-named
    // (`filename: None`) or carries an explicit `Filename=`.
    if is_user_authored_script(sym) {
        return Ok(Target::FileBacked(sym));
    }
    if sym.filename.is_some() {
        return Err(format!(
            "‘{}’ is defined in its own file; renaming file-backed symbols (DBC signals, firmware-generated scripts) is not supported",
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
pub(super) fn cursor_target<'p>(
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

/// One `TextDocumentEdit` (unversioned) for a file's edits, for `document_changes`.
pub(super) fn text_doc_edit(uri: Url, edits: Vec<TextEdit>) -> DocumentChangeOperation {
    DocumentChangeOperation::Edit(TextDocumentEdit {
        text_document: OptionalVersionedTextDocumentIdentifier { uri, version: None },
        edits: edits.into_iter().map(OneOf::Left).collect(),
    })
}
