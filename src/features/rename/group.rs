//! Group / compound cascade rename (#72): renaming a container segment across
//! the `.m1prj` (group + every descendant declaration), every resolving
//! reference in the scripts, and the convention-named backing scripts of
//! method/func descendants (moved via bundled `RenameFile` ops).
use super::helpers::{
    is_user_authored_script, load_prj_text, project_scripts, resolve_prefix, scope_for, split_leaf,
    text_doc_edit,
};
use crate::convert::range as to_range;
use crate::features::locate::{file_name_of, for_each_top_path};
use crate::line_index::{LineIndex, PositionEncoding};
use crate::project_store::LoadedProject;
use m1_core::Node;
use m1_typecheck::project::Project;
use m1_typecheck::resolve::Scope;
use m1_typecheck::symbols::{Symbol, SymbolKind};
use tower_lsp::lsp_types::{
    DocumentChangeOperation, DocumentChanges, RenameFile, ResourceOp, TextEdit, Url, WorkspaceEdit,
};

/// The new absolute path for a descendant `path` of `group_path` when the group's
/// leaf is renamed to `new_name` (`Root.Engine.Speed` + `Root.Engine`→`Motor` ⇒
/// `Root.Motor.Speed`). The group leaf is the only segment that changes.
pub(super) fn rename_group_segment(
    path: &str,
    group_path: &str,
    old_leaf: &str,
    new_name: &str,
) -> String {
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
    use crate::features::locate::segment_nodes;
    let group_depth = group_path.split('.').count() - 1;
    let prefix = format!("{group_path}.");
    // O(n) single pass ([`for_each_top_path`]): stack-safe on deep input (#133)
    // and free of the per-node parent climbs that made the old `descendants()` +
    // `is_top_path` scan O(n²).
    let mut out = Vec::new();
    for_each_top_path(root, |n, _is_write| {
        if let Some((sym, k)) = resolve_prefix(n.text(), scope)
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
    });
    out
}

/// If `filename` begins with the path token `old_leaf` — a *whole* leading
/// segment, delimited by a space, a dot, or the start of the `.m1scr` extension —
/// return the filename with that leading token replaced by `new_name`. Returns
/// `None` when the group segment is not the leading token, so an unrelated
/// filename (`Democracy.m1scr` vs leaf `Demo`) or one whose group segment is
/// deeper than the lead is never rewritten — the `.m1prj` is left untouched
/// rather than corrupted (#149).
pub(super) fn rewrite_filename_group_segment(
    filename: &str,
    old_leaf: &str,
    new_name: &str,
) -> Option<String> {
    let rest = filename.strip_prefix(old_leaf)?;
    match rest.chars().next() {
        Some(' ') | Some('.') => Some(format!("{new_name}{rest}")),
        _ => None,
    }
}

/// `.m1prj` text edits keeping explicit `Filename="…"` attributes consistent with
/// a group rename: for each script symbol under the group whose explicit filename
/// leads with the renamed group segment, rewrite that leading segment in place.
/// Symbol-driven (not a blind text scan) so only the filenames of scripts that
/// actually live under the renamed group are touched — a same-named filename
/// under a different group is left alone (#149).
fn m1prj_filename_edits(
    prj_text: &str,
    project: &Project,
    group_path: &str,
    old_leaf: &str,
    new_name: &str,
    enc: PositionEncoding,
) -> Vec<TextEdit> {
    let li = LineIndex::new(prj_text);
    let prefix = format!("{group_path}.");
    let mut out = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for sym in project.symbols().iter() {
        if !sym.path.starts_with(&prefix) {
            continue;
        }
        let Some(fname) = sym.filename.as_deref() else {
            continue;
        };
        let Some(new_fname) = rewrite_filename_group_segment(fname, old_leaf, new_name) else {
            continue;
        };
        let needle = format!("Filename=\"{fname}\"");
        if !seen.insert(needle.clone()) {
            continue; // a filename shared by several components is edited once
        }
        let mut search = 0;
        while let Some(rel) = prj_text[search..].find(&needle) {
            let val_start = search + rel + "Filename=\"".len();
            let val_end = val_start + fname.len();
            out.push(TextEdit {
                range: to_range(&(val_start..val_end), &li, enc),
                new_text: new_fname.clone(),
            });
            search = val_end + 1;
        }
    }
    out
}

/// `RenameFile` operations for the convention-named (no explicit `Filename=`)
/// method/func scripts under the group, whose derived filename embeds the group
/// segment. Refuses (the whole rename) if any such script can't be located on
/// disk — renaming the group without renaming its file would silently break the
/// script's group-relative references, which we never do.
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
        let (old_base, new_base) = match sym.filename.as_deref() {
            // Explicit `Filename=`: rename the file only when the filename leads
            // with the renamed group segment (so it actually encodes the group).
            // Otherwise the file location is independent of the group name and
            // needs no move (#149).
            Some(f) => match rewrite_filename_group_segment(f, old_leaf, new_name) {
                Some(nf) => (f.to_string(), nf),
                None => continue,
            },
            // Derived basename convention: the path minus `Root.` + `.m1scr`.
            None => {
                let rel = sym.path.strip_prefix("Root.").unwrap_or(&sym.path);
                let old_base = format!("{rel}.m1scr");
                let new_path = rename_group_segment(&sym.path, group_path, old_leaf, new_name);
                let new_rel = new_path.strip_prefix("Root.").unwrap_or(&new_path);
                (old_base, format!("{new_rel}.m1scr"))
            }
        };
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
pub(super) fn execute_group(
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
    let (prj_uri, prj_text) = load_prj_text(loaded, open_text)?;
    let mut prj_edits = m1prj_group_edits(&prj_text, &group_path, old_leaf, new_name, enc);
    if prj_edits.is_empty() {
        return Err(format!(
            "could not locate the declaration of ‘{group_path}’ in the project file"
        ));
    }
    // Keep any explicit `Filename=` attributes consistent with the renamed group.
    prj_edits.extend(m1prj_filename_edits(
        &prj_text,
        project,
        &group_path,
        old_leaf,
        new_name,
        enc,
    ));
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
