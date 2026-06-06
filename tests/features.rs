//! Feature-level tests: hover/goto/document-symbol/completion against an
//! in-memory project fixture written to a tempdir.
use m1_lsp::features::{completion, goto, hover};
use m1_lsp::line_index::{LineIndex, PositionEncoding};
use m1_lsp::project_store::ProjectStore;
use std::io::Write;
use tower_lsp::lsp_types::HoverContents;

const M1PRJ: &str = r#"<?xml version="1.0"?>
<Project>
  <Component Classname="BuiltIn.GroupCompound" Name="Root"/>
  <Component Classname="BuiltIn.ChannelMeasure" Name="Root.Speed Glonk"/>
  <Component Classname="BuiltIn.FuncUser" Name="Root.Do Thing" Filename="Do Thing.m1scr"/>
</Project>"#;

fn store_with_project() -> (tempfile::TempDir, ProjectStore) {
    let tmp = tempfile::tempdir().unwrap();
    let prj = tmp.path().join("Project.m1prj");
    std::fs::File::create(&prj)
        .unwrap()
        .write_all(M1PRJ.as_bytes())
        .unwrap();
    let store = ProjectStore::new();
    store.discover_and_load(tmp.path()).unwrap();
    (tmp, store)
}

#[test]
fn hover_shows_channel_kind() {
    let (_tmp, store) = store_with_project();
    let src = "Speed Glonk = 1;\n";
    let cst = m1_core::parse(src);
    let li = LineIndex::new(src);
    store.with_project(|p| {
        let h = hover::hover(
            cst.root(),
            0,
            p.map(|lp| &lp.project),
            Some("X.m1scr"),
            &li,
            PositionEncoding::Utf16,
        );
        let h = h.expect("known channel should hover");
        if let HoverContents::Markup(m) = h.contents {
            assert!(m.value.contains("channel"));
        } else {
            panic!("markup expected")
        }
    });
}

#[test]
fn completion_includes_project_symbol() {
    let (_tmp, store) = store_with_project();
    let src = "local x = 1;\n";
    let cst = m1_core::parse(src);
    store.with_project(|p| {
        let li = LineIndex::new(src);
        let items = completion::completions(
            cst.root(),
            p,
            Some("X.m1scr"),
            src,
            src.len(),
            &li,
            PositionEncoding::Utf16,
        );
        assert!(items.iter().any(|i| i.label.contains("Speed Glonk")));
    });
}

#[test]
fn goto_unknown_symbol_is_none() {
    let (_tmp, store) = store_with_project();
    let src = "Nonexistent Thing = 1;\n";
    let cst = m1_core::parse(src);
    store.with_project(|p| {
        assert!(goto::goto(cst.root(), 0, p.unwrap(), Some("X.m1scr")).is_none());
    });
}
