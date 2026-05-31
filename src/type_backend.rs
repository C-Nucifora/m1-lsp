//! Real type-diagnostic provider backed by m1-typecheck + the ProjectStore.
use crate::analysis::TypeProvider;
use crate::convert::type_diagnostic;
use crate::line_index::{LineIndex, PositionEncoding};
use crate::project_store::ProjectStore;
use m1_typecheck::rules::{check_script, check_script_no_project};
use std::path::PathBuf;
use std::sync::Arc;
use tower_lsp::lsp_types::{Diagnostic as LspDiag, Url};

pub struct M1Type {
    store: Arc<ProjectStore>,
}

impl M1Type {
    pub fn new(store: Arc<ProjectStore>) -> Self {
        Self { store }
    }
}

/// Best-effort file-system path for `uri` (for group-relative resolution).
fn uri_path(uri: &Url) -> PathBuf {
    uri.to_file_path()
        .unwrap_or_else(|_| PathBuf::from(uri.path()))
}

impl TypeProvider for M1Type {
    fn types(&self, uri: &Url, src: &str, li: &LineIndex, enc: PositionEncoding) -> Vec<LspDiag> {
        let path = uri_path(uri);
        let result = self.store.with_project(|p| match p {
            Some(lp) => check_script(&lp.project, &path, src),
            None => check_script_no_project(src),
        });
        // Syntax errors are reported by m1-core in analyze(); ignore them here.
        result
            .diagnostics
            .iter()
            .map(|d| type_diagnostic(d, li, enc))
            .collect()
    }

    fn project_loaded(&self) -> bool {
        self.store.project_loaded()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flags_float_equality_without_project() {
        // T002 fires in project-less mode (no project needed for float-eq typing).
        let src = "fGain = 1.0;\nif (fGain == 2.0) {\n}\n";
        let store = Arc::new(ProjectStore::new());
        let p = M1Type::new(store);
        let uri = Url::parse("file:///x.m1scr").unwrap();
        let li = LineIndex::new(src);
        let diags = p.types(&uri, src, &li, PositionEncoding::Utf16);
        assert!(
            diags
                .iter()
                .any(|d| d.source.as_deref() == Some("m1-typecheck"))
        );
        assert!(!p.project_loaded());
    }
}
