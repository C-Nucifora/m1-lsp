//! `workspace/willRenameFiles` (#250): renaming a `.m1scr` in the file
//! explorer updates the project mapping instead of silently breaking it.
//!
//! Scripts map to symbols by an explicit `Filename="…"` attribute or by the
//! path-encoding convention (`Root.Engine.Update` ↔ `Engine.Update.m1scr`), so
//! a file rename is the *inverse gesture* of a symbol rename:
//!
//! * **Explicit `Filename=`** — the attribute value is updated; nothing else
//!   depends on the basename.
//! * **Convention-named, one group segment changed** — exactly the group-rename
//!   cascade (`super::group::execute_group`): the `.m1prj` declarations, every
//!   resolving reference, and the sibling backing scripts. The rename the user
//!   is already performing is stripped from the returned operations (the
//!   client does that part itself).
//! * **Anything else expressible only as a symbol rename** (function leaf
//!   changed, several segments changed, extension dropped) — refused with an
//!   actionable message telling the user to rename the symbol in-editor
//!   instead, which cascades to the file correctly.

use super::group::execute_group;
use super::helpers::{load_prj_text, text_doc_edit};
use crate::convert::range as to_range;
use crate::features::locate::file_name_of;
use crate::line_index::{LineIndex, PositionEncoding};
use crate::project_store::LoadedProject;
use tower_lsp::lsp_types::{
    DocumentChangeOperation, DocumentChanges, ResourceOp, Url, WorkspaceEdit,
};

/// The workspace edit keeping the project consistent with renaming `old_uri`
/// to `new_uri`. `Ok(None)` when the file backs no project symbol (nothing to
/// do); `Err` when the rename breaks the convention in a way only a symbol
/// rename can express — surfaced to the user as a warning.
pub fn execute_file_rename(
    old_uri: &Url,
    new_uri: &Url,
    enc: PositionEncoding,
    loaded: &LoadedProject,
    open_text: &dyn Fn(&Url) -> Option<String>,
) -> Result<Option<WorkspaceEdit>, String> {
    let (Some(old_base), Some(new_base)) = (file_name_of(old_uri), file_name_of(new_uri)) else {
        return Ok(None);
    };
    // A move to another directory keeps the basename: the mapping is
    // basename-keyed, so nothing needs editing.
    if old_base == new_base {
        return Ok(None);
    }

    let project = &loaded.project;
    let Some(sym_path) = project.function_symbol_for_script(&old_base) else {
        return Ok(None); // not a project-backed script
    };
    let Some(sym) = project.symbols().get(&sym_path) else {
        return Ok(None);
    };

    // Explicit `Filename="…"`: update the attribute; the symbol name (and so
    // every reference) is independent of the basename.
    if sym.filename.as_deref() == Some(old_base.as_str()) {
        let (prj_uri, prj_text) = load_prj_text(loaded, open_text)?;
        let li = LineIndex::new(&prj_text);
        let needle = format!("Filename=\"{old_base}\"");
        let mut edits = Vec::new();
        let mut search = 0;
        while let Some(rel) = prj_text[search..].find(&needle) {
            let val_start = search + rel + "Filename=\"".len();
            let val_end = val_start + old_base.len();
            edits.push(tower_lsp::lsp_types::TextEdit {
                range: to_range(&(val_start..val_end), &li, enc),
                new_text: new_base.clone(),
            });
            search = val_end + 1;
        }
        if edits.is_empty() {
            return Ok(None);
        }
        return Ok(Some(WorkspaceEdit {
            document_changes: Some(DocumentChanges::Operations(vec![text_doc_edit(
                prj_uri, edits,
            )])),
            ..Default::default()
        }));
    }

    // Convention-named: the basename *is* the symbol path. Work out what the
    // new name implies, segment by segment.
    let old_stem = old_base.strip_suffix(".m1scr").unwrap_or(&old_base);
    let Some(new_stem) = new_base.strip_suffix(".m1scr") else {
        return Err(format!(
            "renaming ‘{old_base}’ to ‘{new_base}’ drops the .m1scr extension — \
             the project mapping for ‘{sym_path}’ would break"
        ));
    };
    let olds: Vec<&str> = old_stem.split('.').collect();
    let news: Vec<&str> = new_stem.split('.').collect();
    if olds == news {
        return Ok(None);
    }
    let changed: Vec<usize> = (0..olds.len().max(news.len()))
        .filter(|&i| olds.get(i) != news.get(i))
        .collect();
    if olds.len() != news.len() || changed.len() != 1 {
        return Err(format!(
            "renaming ‘{old_base}’ to ‘{new_base}’ does not map onto the \
             filename↔symbol convention for ‘{sym_path}’ — rename the symbol \
             in the editor instead (that cascades to the file), or update the \
             .m1prj’s Filename attribute"
        ));
    }
    let i = changed[0];
    if i == olds.len() - 1 {
        return Err(format!(
            "renaming ‘{old_base}’ to ‘{new_base}’ changes the function name — \
             rename ‘{sym_path}’ in the editor instead, which renames the file \
             and updates every reference"
        ));
    }

    // One *group* segment changed: this is exactly the group-rename cascade.
    let group_path = format!("Root.{}", olds[..=i].join("."));
    let Some(group_sym) = project.symbols().get(&group_path) else {
        return Err(format!(
            "‘{old_base}’ encodes group ‘{group_path}’, which is not declared \
             in the project — update the .m1prj manually"
        ));
    };
    let mut edit = execute_group(group_sym, news[i], old_uri, enc, loaded, open_text)?;
    strip_requested_rename(&mut edit, old_uri, &new_base)?;
    Ok(Some(edit))
}

/// Remove the cascade's `RenameFile` op for the file the user is already
/// renaming — the client performs that rename itself; returning it too would
/// double-apply. Errs if the cascade planned a *different* new name for this
/// file than the user typed (the rename then doesn't mean what they think).
fn strip_requested_rename(
    edit: &mut WorkspaceEdit,
    old_uri: &Url,
    new_base: &str,
) -> Result<(), String> {
    let Some(DocumentChanges::Operations(ops)) = &mut edit.document_changes else {
        return Ok(());
    };
    let old_path = old_uri.to_file_path().ok();
    let mut planned: Option<String> = None;
    ops.retain(|op| {
        if let DocumentChangeOperation::Op(ResourceOp::Rename(r)) = op
            && r.old_uri.to_file_path().ok() == old_path
        {
            planned = r
                .new_uri
                .to_file_path()
                .ok()
                .and_then(|p| p.file_name().map(|f| f.to_string_lossy().into_owned()));
            return false;
        }
        true
    });
    match planned {
        Some(p) if p == new_base => Ok(()),
        Some(p) => Err(format!(
            "the group cascade for this rename would name the file ‘{p}’, not \
             ‘{new_base}’ — rename the group symbol in the editor instead"
        )),
        // The cascade did not plan to rename this file at all: the file's name
        // doesn't actually encode the changed segment; refuse rather than
        // guess.
        None => Err(
            "this file rename does not correspond to the group cascade; \
             update the .m1prj manually"
                .to_string(),
        ),
    }
}
