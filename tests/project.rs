//! Project discovery and type-diagnostic wiring.
use m1_lsp::analysis::{NoLint, analyze};
use m1_lsp::line_index::{LineIndex, PositionEncoding};
use m1_lsp::project_store::ProjectStore;
use m1_lsp::type_backend::M1Type;
use std::sync::Arc;
use tower_lsp::lsp_types::Url;

#[test]
fn type_diagnostics_published_via_analyze() {
    // project-less mode still produces type diagnostics for typed float-eq.
    let store = Arc::new(ProjectStore::new());
    let types = M1Type::new(store);
    let src = "fGain = 1.0;\nif (fGain == 2.0) {\n}\n";
    let li = LineIndex::new(src);
    let uri = Url::parse("file:///x.m1scr").unwrap();
    let diags = analyze(
        &uri,
        src,
        &li,
        PositionEncoding::Utf16,
        &NoLint,
        &types,
        &m1_lsp::config::DiagFilter::default(),
    );
    assert!(
        diags
            .iter()
            .any(|d| d.source.as_deref() == Some("m1-typecheck"))
    );
}
