//! File-backed function/method rename (#150): a user-authored `FuncUser` /
//! `MethodUser` backed by its own `.m1scr`. Renamed across its `.m1prj`
//! declaration, every resolving call site, and its backing file (moved via a
//! bundled `RenameFile`; an explicit `Filename=` is kept consistent when it
//! encodes the leaf).
use super::helpers::{
    collect_ref_edits, load_prj_text, m1prj_name_edit, project_scripts, scope_for, split_leaf,
    text_doc_edit,
};
use crate::convert::range as to_range;
use crate::features::locate::file_name_of;
use crate::line_index::{LineIndex, PositionEncoding};
use crate::project_store::LoadedProject;
use m1_typecheck::symbols::Symbol;
use tower_lsp::lsp_types::{
    DocumentChangeOperation, DocumentChanges, RenameFile, ResourceOp, TextEdit, Url, WorkspaceEdit,
};

/// Rewrite a trailing `<old_leaf>` token in a `.m1scr` filename
/// (`Demo.Calculate.m1scr` + `Calculate`→`Recalculate` ⇒ `Demo.Recalculate.m1scr`).
/// The leaf must be a whole token immediately before the `.m1scr` extension,
/// optionally preceded by a `.`/` ` delimiter. `None` when the filename does not
/// end with the leaf token — its location is then independent of the symbol name,
/// so the file is left where it is (#150).
pub(super) fn rewrite_trailing_leaf(
    filename: &str,
    old_leaf: &str,
    new_name: &str,
) -> Option<String> {
    let stem = filename.strip_suffix(".m1scr")?;
    let head = stem.strip_suffix(old_leaf)?;
    if head.is_empty() || head.ends_with('.') || head.ends_with(' ') {
        Some(format!("{head}{new_name}.m1scr"))
    } else {
        None
    }
}

/// `(old_basename, new_basename, rewrites_explicit_filename)` for a
/// function/method's backing file when its leaf is renamed. `None` => no file
/// move (an explicit `Filename=` that doesn't encode the leaf; its location is
/// independent of the symbol name, so only the `Name=` is updated).
fn func_backing_basenames(
    sym: &Symbol,
    parent: Option<&str>,
    old_leaf: &str,
    new_name: &str,
) -> Option<(String, String, bool)> {
    match sym.filename.as_deref() {
        None => {
            // Convention: the path minus `Root.` + `.m1scr`.
            let rel = sym.path.strip_prefix("Root.").unwrap_or(&sym.path);
            let old_base = format!("{rel}.m1scr");
            let new_path = match parent {
                Some(p) => format!("{p}.{new_name}"),
                None => new_name.to_string(),
            };
            let new_rel = new_path.strip_prefix("Root.").unwrap_or(&new_path);
            Some((old_base, format!("{new_rel}.m1scr"), false))
        }
        Some(f) => rewrite_trailing_leaf(f, old_leaf, new_name).map(|nf| (f.to_string(), nf, true)),
    }
}

/// Execute a file-backed function/method rename: the `.m1prj` `Name=` (and an
/// explicit `Filename=` when it encodes the leaf), every resolving call site, and
/// a `RenameFile` op moving the backing `.m1scr` (#150).
pub(super) fn execute_func_method(
    sym: &Symbol,
    new_name: &str,
    cursor_uri: &Url,
    enc: PositionEncoding,
    loaded: &LoadedProject,
    open_text: &dyn Fn(&Url) -> Option<String>,
) -> Result<WorkspaceEdit, String> {
    let project = &loaded.project;
    let target_path = sym.path.clone();
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

    // Resolve the backing-file rename first — it is the step that can refuse.
    let mut rename_op: Option<RenameFile> = None;
    let mut filename_attr: Option<(String, String)> = None; // (old, new) for the .m1prj edit
    if let Some((old_base, new_base, rewrites_attr)) =
        func_backing_basenames(sym, parent, old_leaf, new_name)
    {
        let disk = loaded.script_files.iter().find(|p| {
            p.file_name()
                .map(|f| f == old_base.as_str())
                .unwrap_or(false)
        });
        match disk {
            Some(disk) => {
                if let (Ok(old_uri), Ok(new_uri)) = (
                    Url::from_file_path(disk),
                    Url::from_file_path(disk.with_file_name(&new_base)),
                ) {
                    rename_op = Some(RenameFile {
                        old_uri,
                        new_uri,
                        options: None,
                        annotation_id: None,
                    });
                }
            }
            None => {
                return Err(format!(
                    "‘{target_path}’ has no locatable backing script ({old_base}); rename that file before renaming the symbol so references aren't silently broken"
                ));
            }
        }
        if rewrites_attr {
            filename_attr = Some((old_base, new_base));
        }
    }

    let mut ops: Vec<DocumentChangeOperation> = Vec::new();

    // 1) `.m1prj`: the declaration `Name=` (and an explicit `Filename=`).
    let (prj_uri, prj_text) = load_prj_text(loaded, open_text)?;
    let mut prj_edits = vec![
        m1prj_name_edit(&prj_text, &target_path, old_leaf, new_name, enc).ok_or_else(|| {
            format!("could not locate the declaration of ‘{target_path}’ in the project file")
        })?,
    ];
    if let Some((old_f, new_f)) = &filename_attr {
        let li = LineIndex::new(&prj_text);
        let needle = format!("Filename=\"{old_f}\"");
        if let Some(rel) = prj_text.find(&needle) {
            let val_start = rel + "Filename=\"".len();
            let val_end = val_start + old_f.len();
            prj_edits.push(TextEdit {
                range: to_range(&(val_start..val_end), &li, enc),
                new_text: new_f.clone(),
            });
        }
    }
    ops.push(text_doc_edit(prj_uri, prj_edits));

    // 2) Every resolving call site across every script.
    for (su, stext) in project_scripts(loaded, cursor_uri, open_text) {
        let scst = m1_core::parse(&stext);
        let sli = LineIndex::new(&stext);
        let sfname = file_name_of(&su);
        let sscope = scope_for(scst.root(), project, sfname.as_deref());
        let edits = collect_ref_edits(scst.root(), &target_path, new_name, &sscope, &sli, enc);
        if !edits.is_empty() {
            ops.push(text_doc_edit(su, edits));
        }
    }

    // 3) Then the backing-file rename (applied after the text edits).
    if let Some(rf) = rename_op {
        ops.push(DocumentChangeOperation::Op(ResourceOp::Rename(rf)));
    }

    Ok(WorkspaceEdit {
        changes: None,
        document_changes: Some(DocumentChanges::Operations(ops)),
        change_annotations: None,
    })
}
