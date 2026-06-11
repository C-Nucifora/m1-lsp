//! Tests for the rename feature: locals, project-leaf, group cascade,
//! file-backed func/method, and `.m1prj`-initiated renames.
#![allow(clippy::too_many_lines)]
use super::func::rewrite_trailing_leaf;
use super::group::rewrite_filename_group_segment;
use super::*;
use crate::project_store::ProjectStore;
use std::io::Write;
use tower_lsp::lsp_types::RenameFile;

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

fn te(s: u32) -> TextEdit {
    TextEdit {
        range: tower_lsp::lsp_types::Range::default(),
        new_text: format!("x{s}"),
    }
}

fn uri(name: &str) -> Url {
    Url::parse(&format!("file:///{name}")).unwrap()
}

#[test]
fn annotate_skips_unsupported_client_and_single_file() {
    // Unsupported client → returned verbatim (still a `changes` map).
    let mut changes = HashMap::new();
    changes.insert(uri("a.m1scr"), vec![te(1)]);
    changes.insert(uri("b.m1scr"), vec![te(2)]);
    let we = WorkspaceEdit {
        changes: Some(changes),
        document_changes: None,
        change_annotations: None,
    };
    let out = annotate_for_confirmation(we, "New", false);
    assert!(out.changes.is_some() && out.change_annotations.is_none());

    // Supported, but a single edited file with no file move → unchanged.
    let mut one = HashMap::new();
    one.insert(uri("a.m1scr"), vec![te(1)]);
    let we1 = WorkspaceEdit {
        changes: Some(one),
        document_changes: None,
        change_annotations: None,
    };
    let out1 = annotate_for_confirmation(we1, "New", true);
    assert!(
        out1.change_annotations.is_none(),
        "single-file rename needs no preview"
    );
}

#[test]
fn annotate_multi_file_attaches_confirmation() {
    let mut changes = HashMap::new();
    changes.insert(uri("a.m1scr"), vec![te(1)]);
    changes.insert(uri("b.m1scr"), vec![te(2)]);
    let we = WorkspaceEdit {
        changes: Some(changes),
        document_changes: None,
        change_annotations: None,
    };
    let out = annotate_for_confirmation(we, "New", true);
    // Converted to document_changes carrying a confirmation annotation.
    assert!(out.changes.is_none());
    let anns = out.change_annotations.expect("annotations attached");
    assert_eq!(anns.len(), 1);
    assert_eq!(anns.values().next().unwrap().needs_confirmation, Some(true));
    match out.document_changes {
        Some(DocumentChanges::Operations(ops)) => {
            assert_eq!(ops.len(), 2);
            for op in ops {
                let DocumentChangeOperation::Edit(tde) = op else {
                    panic!("expected edit op")
                };
                assert!(tde.edits.iter().all(|e| matches!(e, OneOf::Right(_))));
            }
        }
        _ => panic!("expected operations"),
    }
}

#[test]
fn annotate_file_rename_op_attaches_confirmation_even_single_edit() {
    // One edited file but a RenameFile op → still needs confirmation.
    let ops = vec![
        DocumentChangeOperation::Edit(TextDocumentEdit {
            text_document: OptionalVersionedTextDocumentIdentifier {
                uri: uri("Project.m1prj"),
                version: None,
            },
            edits: vec![OneOf::Left(te(1))],
        }),
        DocumentChangeOperation::Op(ResourceOp::Rename(RenameFile {
            old_uri: uri("Old.m1scr"),
            new_uri: uri("New.m1scr"),
            options: None,
            annotation_id: None,
        })),
    ];
    let we = WorkspaceEdit {
        changes: None,
        document_changes: Some(DocumentChanges::Operations(ops)),
        change_annotations: None,
    };
    let out = annotate_for_confirmation(we, "New", true);
    assert!(out.change_annotations.is_some());
    let Some(DocumentChanges::Operations(ops)) = out.document_changes else {
        panic!("expected operations")
    };
    let renamed = ops.iter().any(|o| {
            matches!(o, DocumentChangeOperation::Op(ResourceOp::Rename(rf)) if rf.annotation_id.is_some())
        });
    assert!(renamed, "RenameFile must carry the annotation id");
}

#[test]
fn rewrite_trailing_leaf_only_on_whole_trailing_token() {
    assert_eq!(
        rewrite_trailing_leaf("Demo.Calculate.m1scr", "Calculate", "Recalculate").as_deref(),
        Some("Demo.Recalculate.m1scr")
    );
    assert_eq!(
        rewrite_trailing_leaf("Calculate.m1scr", "Calculate", "Recalculate").as_deref(),
        Some("Recalculate.m1scr")
    );
    // Not a whole trailing token → left alone.
    assert_eq!(
        rewrite_trailing_leaf("Demo.Miscalculate.m1scr", "Calculate", "Recalculate"),
        None
    );
    assert_eq!(
        rewrite_trailing_leaf("Demo.Calculate.txt", "Calculate", "Recalculate"),
        None
    );
}

/// Build a project with a method `Root.Demo.Calculate` (convention-named
/// backing script) and a caller, returning the loaded store.
fn setup_func() -> (tempfile::TempDir, ProjectStore) {
    let prj = r#"<?xml version="1.0"?>
<Project>
  <Component Classname="BuiltIn.GroupCompound" Name="Root"/>
  <Component Classname="BuiltIn.GroupCompound" Name="Root.Demo"/>
  <Component Classname="BuiltIn.MethodUser" Name="Root.Demo.Calculate"/>
  <Component Classname="BuiltIn.MethodUser" Name="Root.Demo.Other"/>
</Project>"#;
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(tmp.path().join("Project.m1prj"), prj).unwrap();
    std::fs::write(tmp.path().join("Demo.Calculate.m1scr"), "// calc\n").unwrap();
    std::fs::write(tmp.path().join("Demo.Other.m1scr"), "Demo.Calculate();\n").unwrap();
    let store = ProjectStore::new();
    store.discover_and_load(tmp.path()).unwrap();
    (tmp, store)
}

#[test]
fn file_backed_method_is_renameable_from_call_site() {
    let (tmp, store) = setup_func();
    let src = "Demo.Calculate();\n";
    let cst = m1_core::parse(src);
    let li = LineIndex::new(src);
    let byte = src.find("Calculate").unwrap();
    let pr = store.with_project(|p| {
        prepare(
            cst.root(),
            byte,
            &li,
            PositionEncoding::Utf16,
            Some(&p.unwrap().project),
            Some("Demo.Other.m1scr"),
        )
    });
    assert!(pr.is_some(), "file-backed method should be renameable");
    drop(tmp);
}

#[test]
fn rename_file_backed_method_edits_decl_callsite_and_moves_file() {
    let (tmp, store) = setup_func();
    let src = "Demo.Calculate();\n";
    let uri = Url::from_file_path(tmp.path().join("Demo.Other.m1scr")).unwrap();
    let cst = m1_core::parse(src);
    let li = LineIndex::new(src);
    let byte = src.find("Calculate").unwrap();
    let no_open = |_: &Url| None;

    let we = store
        .with_project(|p| {
            execute(
                cst.root(),
                byte,
                "Recalculate",
                uri.clone(),
                &li,
                PositionEncoding::Utf16,
                Some(p.unwrap()),
                Some("Demo.Other.m1scr"),
                &no_open,
            )
        })
        .expect("file-backed method rename should succeed");

    // .m1prj declaration leaf rewritten.
    let prj = doc_edits_for(&we, "Project.m1prj");
    assert!(
        prj.iter().any(|e| e.new_text == "Recalculate"),
        "Name= leaf must be rewritten: {prj:?}"
    );
    // Call site rewritten.
    let other = doc_edits_for(&we, "Demo.Other.m1scr");
    assert!(
        other.iter().any(|e| e.new_text == "Recalculate"),
        "call site must be rewritten: {other:?}"
    );
    // Backing file moved.
    let files = rename_files(&we);
    assert!(
        files
            .iter()
            .any(|(o, n)| o == "Demo.Calculate.m1scr" && n == "Demo.Recalculate.m1scr"),
        "backing file must be renamed: {files:?}"
    );
}

#[test]
fn rename_file_backed_method_refuses_when_backing_file_missing() {
    let (tmp, store) = setup_func();
    // Remove the backing file so the rename can't locate it.
    std::fs::remove_file(tmp.path().join("Demo.Calculate.m1scr")).unwrap();
    store.discover_and_load(tmp.path()).unwrap();
    let src = "Demo.Calculate();\n";
    let uri = Url::from_file_path(tmp.path().join("Demo.Other.m1scr")).unwrap();
    let cst = m1_core::parse(src);
    let li = LineIndex::new(src);
    let byte = src.find("Calculate").unwrap();
    let no_open = |_: &Url| None;

    let res = store.with_project(|p| {
        execute(
            cst.root(),
            byte,
            "Recalculate",
            uri.clone(),
            &li,
            PositionEncoding::Utf16,
            Some(p.unwrap()),
            Some("Demo.Other.m1scr"),
            &no_open,
        )
    });
    assert!(res.is_err(), "must refuse when backing file is missing");
}

#[test]
fn rewrite_filename_group_segment_only_on_leading_whole_token() {
    // Space- and dot-delimited leading group token → rewritten.
    assert_eq!(
        rewrite_filename_group_segment("Demo Run.m1scr", "Demo", "Widget").as_deref(),
        Some("Widget Run.m1scr")
    );
    assert_eq!(
        rewrite_filename_group_segment("Demo.Run.m1scr", "Demo", "Widget").as_deref(),
        Some("Widget.Run.m1scr")
    );
    assert_eq!(
        rewrite_filename_group_segment("Demo.m1scr", "Demo", "Widget").as_deref(),
        Some("Widget.m1scr")
    );
    // Not a whole leading token → left untouched (no corruption).
    assert_eq!(
        rewrite_filename_group_segment("Democracy.m1scr", "Demo", "Widget"),
        None
    );
    assert_eq!(
        rewrite_filename_group_segment("Other Run.m1scr", "Demo", "Widget"),
        None
    );
}

#[test]
fn group_rename_rewrites_explicit_filename_and_moves_the_file() {
    // A project with an *explicit* `Filename=` that embeds the group segment.
    let prj = r#"<?xml version="1.0"?>
<Project>
  <Component Classname="BuiltIn.GroupCompound" Name="Root"/>
  <Component Classname="BuiltIn.GroupCompound" Name="Root.Demo"/>
  <Component Classname="BuiltIn.FuncUser" Name="Root.Demo.Run" Filename="Demo Run.m1scr"/>
</Project>"#;
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(tmp.path().join("Project.m1prj"), prj).unwrap();
    std::fs::write(tmp.path().join("Demo Run.m1scr"), "// demo\n").unwrap();
    let store = ProjectStore::new();
    store.discover_and_load(tmp.path()).unwrap();

    // Rename group Demo -> Widget, cursor on `Demo` in the script's own ref.
    let src = "local x = Root.Demo.Run;\n";
    let uri = Url::from_file_path(tmp.path().join("Demo Run.m1scr")).unwrap();
    let cst = m1_core::parse(src);
    let li = LineIndex::new(src);
    let byte = src.find("Demo").unwrap();
    let no_open = |_: &Url| None;

    let we = store
        .with_project(|p| {
            execute(
                cst.root(),
                byte,
                "Widget",
                uri.clone(),
                &li,
                PositionEncoding::Utf16,
                Some(p.unwrap()),
                Some("Demo Run.m1scr"),
                &no_open,
            )
        })
        .expect("group rename should succeed");

    // The `.m1prj` edits include the Filename value rewrite (Demo Run -> Widget Run).
    let prj_edits = doc_edits_for(&we, "Project.m1prj");
    assert!(
        prj_edits.iter().any(|e| e.new_text == "Widget Run.m1scr"),
        "Filename= must be rewritten: {prj_edits:?}"
    );
    // And a RenameFile op moves the backing file (URI paths are percent-encoded).
    let files = rename_files(&we);
    let decode = |s: &str| s.replace("%20", " ");
    assert!(
        files
            .iter()
            .any(|(o, n)| decode(o) == "Demo Run.m1scr" && decode(n) == "Widget Run.m1scr"),
        "explicit-filename script must be renamed on disk: {files:?}"
    );
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

// ---- workspace/willRenameFiles (#250) ----------------------------------

/// The TextDocumentEdits of `we` whose target URI ends with `suffix`.
fn op_edits_for(we: &WorkspaceEdit, suffix: &str) -> Vec<TextEdit> {
    let Some(DocumentChanges::Operations(ops)) = &we.document_changes else {
        return vec![];
    };
    ops.iter()
        .filter_map(|op| match op {
            DocumentChangeOperation::Edit(e) if e.text_document.uri.path().ends_with(suffix) => {
                Some(e.edits.iter().map(|x| match x {
                    OneOf::Left(t) => t.clone(),
                    OneOf::Right(a) => a.text_edit.clone(),
                }))
            }
            _ => None,
        })
        .flatten()
        .collect()
}

/// The RenameFile ops of `we`.
fn rename_ops(we: &WorkspaceEdit) -> Vec<RenameFile> {
    let Some(DocumentChanges::Operations(ops)) = &we.document_changes else {
        return vec![];
    };
    ops.iter()
        .filter_map(|op| match op {
            DocumentChangeOperation::Op(ResourceOp::Rename(r)) => Some(r.clone()),
            _ => None,
        })
        .collect()
}

fn file_rename_result(
    store: &ProjectStore,
    dir: &std::path::Path,
    old: &str,
    new: &str,
) -> Result<Option<WorkspaceEdit>, String> {
    let old_uri = Url::from_file_path(dir.join(old)).unwrap();
    let new_uri = Url::from_file_path(dir.join(new)).unwrap();
    let no_open = |_: &Url| None;
    store.with_project(|p| {
        execute_file_rename(
            &old_uri,
            &new_uri,
            PositionEncoding::Utf16,
            p.expect("project loaded"),
            &no_open,
        )
    })
}

#[test]
fn file_rename_of_group_segment_runs_the_cascade() {
    let (tmp, store) = setup();
    let a = "local a = Engine.Threshold;\n";
    let b = "local e = Root.Engine.Threshold;\n";
    std::fs::write(tmp.path().join("Engine.Update.m1scr"), a).unwrap();
    std::fs::write(tmp.path().join("Other.Update.m1scr"), b).unwrap();
    store.discover_and_load(tmp.path()).unwrap();

    // Renaming `Engine.Update.m1scr` -> `Motor.Update.m1scr` implies the group
    // rename Root.Engine -> Root.Motor.
    let we = file_rename_result(
        &store,
        tmp.path(),
        "Engine.Update.m1scr",
        "Motor.Update.m1scr",
    )
    .expect("cascade should succeed")
    .expect("an edit is produced");

    // .m1prj: the group declaration + every descendant Name= gains Motor.
    let prj = op_edits_for(&we, "Project.m1prj");
    assert!(prj.len() >= 5, "group + descendants, got {}", prj.len());
    assert!(prj.iter().all(|e| e.new_text == "Motor"));

    // Both scripts' resolving references are rewritten.
    assert_eq!(op_edits_for(&we, "Engine.Update.m1scr").len(), 1);
    assert_eq!(op_edits_for(&we, "Other.Update.m1scr").len(), 1);

    // The user's own rename is stripped: no RenameFile op for the file the
    // client is already renaming (and Root.Engine backs no other script).
    assert!(
        rename_ops(&we).is_empty(),
        "requested rename must be stripped"
    );
}

#[test]
fn file_rename_updates_an_explicit_filename_attribute() {
    const PRJ_EXPLICIT: &str = r#"<?xml version="1.0"?>
<Project>
  <Component Classname="BuiltIn.GroupCompound" Name="Root"/>
  <Component Classname="BuiltIn.GroupCompound" Name="Root.Engine"/>
  <Component Classname="BuiltIn.MethodUser" Filename="Custom Name.m1scr" Name="Root.Engine.Update"/>
</Project>"#;
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(tmp.path().join("Project.m1prj"), PRJ_EXPLICIT).unwrap();
    std::fs::write(tmp.path().join("Custom Name.m1scr"), "local a = 1;\n").unwrap();
    let store = ProjectStore::new();
    store.discover_and_load(tmp.path()).unwrap();

    let we = file_rename_result(&store, tmp.path(), "Custom Name.m1scr", "Other Name.m1scr")
        .expect("attribute update should succeed")
        .expect("an edit is produced");
    let prj = op_edits_for(&we, "Project.m1prj");
    assert_eq!(prj.len(), 1, "exactly the Filename attribute value");
    assert_eq!(prj[0].new_text, "Other Name.m1scr");
    assert!(rename_ops(&we).is_empty());
}

#[test]
fn file_rename_of_function_leaf_is_refused_with_guidance() {
    let (tmp, store) = setup();
    std::fs::write(tmp.path().join("Engine.Update.m1scr"), "local a = 1;\n").unwrap();
    store.discover_and_load(tmp.path()).unwrap();
    let err = file_rename_result(
        &store,
        tmp.path(),
        "Engine.Update.m1scr",
        "Engine.Tick.m1scr",
    )
    .expect_err("leaf change is a symbol rename");
    assert!(err.contains("rename ‘Root.Engine.Update’"), "got: {err}");
}

#[test]
fn file_rename_dropping_the_extension_is_refused() {
    let (tmp, store) = setup();
    std::fs::write(tmp.path().join("Engine.Update.m1scr"), "local a = 1;\n").unwrap();
    store.discover_and_load(tmp.path()).unwrap();
    let err = file_rename_result(
        &store,
        tmp.path(),
        "Engine.Update.m1scr",
        "Engine.Update.txt",
    )
    .expect_err("extension change breaks the mapping");
    assert!(err.contains(".m1scr"), "got: {err}");
}

#[test]
fn file_rename_of_an_unrelated_file_is_a_no_op() {
    let (tmp, store) = setup();
    std::fs::write(tmp.path().join("NotAScript.m1scr"), "local a = 1;\n").unwrap();
    store.discover_and_load(tmp.path()).unwrap();
    let r = file_rename_result(&store, tmp.path(), "NotAScript.m1scr", "StillNot.m1scr")
        .expect("no error");
    assert!(r.is_none(), "no project symbol -> nothing to do");
}

#[test]
fn file_move_keeping_the_basename_is_a_no_op() {
    let (tmp, store) = setup();
    std::fs::write(tmp.path().join("Engine.Update.m1scr"), "local a = 1;\n").unwrap();
    store.discover_and_load(tmp.path()).unwrap();
    let old_uri = Url::from_file_path(tmp.path().join("Engine.Update.m1scr")).unwrap();
    let new_uri = Url::from_file_path(tmp.path().join("sub/Engine.Update.m1scr")).unwrap();
    let no_open = |_: &Url| None;
    let r = store
        .with_project(|p| {
            execute_file_rename(
                &old_uri,
                &new_uri,
                PositionEncoding::Utf16,
                p.unwrap(),
                &no_open,
            )
        })
        .expect("no error");
    assert!(r.is_none(), "basename unchanged -> mapping intact");
}
