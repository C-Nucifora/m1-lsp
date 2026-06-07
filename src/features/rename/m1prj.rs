//! Rename initiated from within the `.m1prj` itself.
//!
//! Channels/parameters are *declared* in `Project.m1prj` (`<Component Name="…">`),
//! so editing that file is the natural place to rename one. The clients only
//! attach the LSP to `.m1scr`, but when the project file is opened as a document
//! these entry points let a rename on a component's `Name` propagate the same way
//! a script-initiated rename does: the declaration leaf in the `.m1prj` plus every
//! resolving reference across all scripts. Only renameable leaves are offered
//! (childless channel/parameter/constant/reference); groups/objects/compounds are
//! the cascading case and stay out of scope.
use super::func::execute_func_method;
use super::helpers::{
    collect_ref_edits, is_user_authored_script, is_valid_symbol_name, m1prj_name_edit, scope_for,
    split_leaf,
};
use crate::convert::range as to_range;
use crate::features::locate::file_name_of;
use crate::line_index::{LineIndex, PositionEncoding};
use crate::project_store::LoadedProject;
use m1_typecheck::project::Project;
use m1_typecheck::symbols::{Symbol, SymbolKind};
use std::collections::HashMap;
use tower_lsp::lsp_types::{PrepareRenameResponse, TextEdit, Url, WorkspaceEdit};

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
    // A user-authored function/method is renameable from the `.m1prj` too — its
    // backing file moves alongside (#150).
    if is_user_authored_script(sym) {
        return Ok(sym);
    }
    if sym.filename.is_some() {
        return Err(format!(
            "‘{}’ is file-backed (DBC signal / firmware-generated); renaming it is not supported",
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
    let sym = symbol_renameable(project, &target_path)?;

    let new_name = new_name.trim();
    if !is_valid_symbol_name(new_name) {
        return Err(format!(
            "‘{new_name}’ is not a valid M1 symbol name (letters, digits, spaces, underscore; no dots or quotes)"
        ));
    }
    // A file-backed function/method cascades to its call sites and backing file.
    if is_user_authored_script(sym) {
        return execute_func_method(sym, new_name, &prj_uri, enc, loaded, open_text);
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
