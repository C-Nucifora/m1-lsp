//! textDocument/rename + prepareRename.
//!
//! Two tiers of renameable thing:
//!
//!  * **Locals** — file-scoped identifiers (`local x`). Rewritten in-buffer, as
//!    before. Member-access properties (`Foo.count`) and type-annotation names
//!    (`<Count>`) are left alone. See `local`.
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
//!    other groups are left untouched. The leaf case lives inline in `execute`.
//!
//!  * **Groups / objects** (#72) — a compound container. Renamed by a
//!    **cascade** (see `group`): the group segment is rewritten in the `.m1prj`
//!    for the group *and every descendant* `Name="…"`, in every resolving
//!    reference across the scripts (only references that textually spell the group
//!    segment — relative and `This.`/`Parent.`-anchored ones stay valid once the
//!    file is renamed), and the convention-named backing scripts of method/func
//!    descendants are renamed via bundled `RenameFile` operations. Refused (the
//!    whole op) only when a backing script can't be located — never a silent
//!    partial edit. The edit is emitted as `document_changes` so it can carry the
//!    file renames.
//!
//! A user-authored function/method (a `FuncUser`/`MethodUser` backed by its own
//! `.m1scr`) is renamed across its `.m1prj` declaration, every call site, and its
//! backing file (see `func`) — refused only when the backing file can't be
//! located (#150).
//!
//! A rename can also be initiated from the `.m1prj` document itself (see
//! `m1prj`).
//!
//! Out of scope (refused with a message): other file-backed symbols (DBC signals,
//! firmware-generated scripts); and a value-bearing channel/parameter that itself
//! has children (rename its leaf members individually).
mod func;
mod group;
mod helpers;
mod local;
mod m1prj;

use crate::convert::range as to_range;
use crate::features::locate::{path_at_byte, segment_at_byte, segment_nodes};
use crate::line_index::{LineIndex, PositionEncoding};
use crate::project_store::LoadedProject;
use m1_core::Node;
use m1_typecheck::project::Project;
use std::collections::HashMap;
use tower_lsp::lsp_types::{
    AnnotatedTextEdit, ChangeAnnotation, DocumentChangeOperation, DocumentChanges, OneOf,
    OptionalVersionedTextDocumentIdentifier, PrepareRenameResponse, ResourceOp, TextDocumentEdit,
    TextEdit, Url, WorkspaceEdit,
};

// Public API surface (entry points called from backend.rs and code_action.rs).
pub use helpers::{is_valid_identifier, is_valid_symbol_name};
pub use local::{is_local_ref, local_ident_at, local_rename_edits};
pub use m1prj::{execute_m1prj, prepare_m1prj};

use group::execute_group;
use helpers::{
    Target, collect_ref_edits, cursor_target, load_prj_text, m1prj_name_edit, project_scripts,
    resolve_prefix, scope_for, split_leaf,
};

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
    if let Some(node) = local::local_ident_at(root, byte) {
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
        Target::Leaf(_) | Target::FileBacked(_) => {
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
    if let Some(node) = local::local_ident_at(root, byte) {
        if !is_valid_identifier(new_name) {
            return Err(format!(
                "‘{new_name}’ is not a valid local name (letters, digits, underscore and internal spaces; no leading digit or surrounding space)"
            ));
        }
        let name = node.text().to_string();
        let mut edits = Vec::new();
        local::collect_local_edits(root, &name, new_name, li, enc, &mut edits);
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

    // A group/object container cascades across the workspace + backing files; a
    // file-backed function/method renames its declaration, call sites and file.
    let sym = match target {
        Target::Group(g) => return execute_group(g, new_name, &uri, enc, loaded, open_text),
        Target::FileBacked(s) => {
            return func::execute_func_method(s, new_name, &uri, enc, loaded, open_text);
        }
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
    let (prj_uri, prj_text) = load_prj_text(loaded, open_text)?;
    let prj_edit =
        m1prj_name_edit(&prj_text, &target_path, old_leaf, new_name, enc).ok_or_else(|| {
            format!("could not locate the declaration of ‘{target_path}’ in the project file")
        })?;
    changes.entry(prj_uri).or_default().push(prj_edit);

    // 2) Every resolving reference across every script.
    for (su, stext) in project_scripts(loaded, &uri, open_text) {
        let scst = m1_core::parse(&stext);
        let sli = LineIndex::new(&stext);
        let sfname = crate::features::locate::file_name_of(&su);
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

/// Attach a confirmation [`ChangeAnnotation`] to a rename that touches more than
/// one file or moves any file, so a client advertising `changeAnnotationSupport`
/// shows a preview/confirm step before applying (#151). Single-file edits (a
/// local-variable rename, or a leaf rename that resolves within one script) and
/// clients without support are returned unchanged.
pub fn annotate_for_confirmation(
    edit: WorkspaceEdit,
    new_name: &str,
    supported: bool,
) -> WorkspaceEdit {
    if !supported {
        return edit;
    }
    // Count touched files and file-rename ops without consuming the edit.
    let file_count = match (&edit.changes, &edit.document_changes) {
        (Some(c), _) => c.len(),
        (_, Some(DocumentChanges::Operations(ops))) => ops
            .iter()
            .filter(|o| matches!(o, DocumentChangeOperation::Edit(_)))
            .count(),
        (_, Some(DocumentChanges::Edits(e))) => e.len(),
        _ => 0,
    };
    let rename_count = match &edit.document_changes {
        Some(DocumentChanges::Operations(ops)) => ops
            .iter()
            .filter(|o| matches!(o, DocumentChangeOperation::Op(ResourceOp::Rename(_))))
            .count(),
        _ => 0,
    };
    // A single edited file with no file move applies immediately — no preview.
    if file_count <= 1 && rename_count == 0 {
        return edit;
    }

    const ANN_ID: &str = "m1.rename";
    let id: tower_lsp::lsp_types::ChangeAnnotationIdentifier = ANN_ID.to_string();
    let renamed = if rename_count > 0 {
        format!(", {rename_count} file(s) renamed")
    } else {
        String::new()
    };
    let annotate_edits = |edits: Vec<OneOf<TextEdit, AnnotatedTextEdit>>| {
        edits
            .into_iter()
            .map(|e| {
                let text_edit = match e {
                    OneOf::Left(te) => te,
                    OneOf::Right(ate) => ate.text_edit,
                };
                OneOf::Right(AnnotatedTextEdit {
                    text_edit,
                    annotation_id: id.clone(),
                })
            })
            .collect::<Vec<_>>()
    };

    let mut ops: Vec<DocumentChangeOperation> = Vec::new();
    if let Some(changes) = edit.changes {
        for (uri, edits) in changes {
            ops.push(DocumentChangeOperation::Edit(TextDocumentEdit {
                text_document: OptionalVersionedTextDocumentIdentifier { uri, version: None },
                edits: annotate_edits(edits.into_iter().map(OneOf::Left).collect()),
            }));
        }
    } else if let Some(DocumentChanges::Operations(existing)) = edit.document_changes {
        for op in existing {
            match op {
                DocumentChangeOperation::Edit(tde) => {
                    ops.push(DocumentChangeOperation::Edit(TextDocumentEdit {
                        text_document: tde.text_document,
                        edits: annotate_edits(tde.edits),
                    }));
                }
                DocumentChangeOperation::Op(ResourceOp::Rename(mut rf)) => {
                    rf.annotation_id = Some(id.clone());
                    ops.push(DocumentChangeOperation::Op(ResourceOp::Rename(rf)));
                }
                other => ops.push(other),
            }
        }
    } else if let Some(DocumentChanges::Edits(edits)) = edit.document_changes {
        for tde in edits {
            ops.push(DocumentChangeOperation::Edit(TextDocumentEdit {
                text_document: tde.text_document,
                edits: annotate_edits(tde.edits),
            }));
        }
    }

    let mut annotations = HashMap::new();
    annotations.insert(
        id,
        ChangeAnnotation {
            label: format!("Rename to ‘{new_name}’"),
            needs_confirmation: Some(true),
            description: Some(format!("{file_count} file(s) edited{renamed}")),
        },
    );

    WorkspaceEdit {
        changes: None,
        document_changes: Some(DocumentChanges::Operations(ops)),
        change_annotations: Some(annotations),
    }
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
mod tests;
