//! Real type-diagnostic provider backed by m1-typecheck + the ProjectStore.
use crate::analysis::TypeProvider;
use crate::config::DiagFilter;
use crate::convert::type_diagnostic;
use crate::line_index::{LineIndex, PositionEncoding};
use crate::project_store::ProjectStore;
use m1_typecheck::rules::check_script_with_channels;
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::{Arc, RwLock};
use tower_lsp::lsp_types::{Diagnostic as LspDiag, Url};

pub struct M1Type {
    store: Arc<ProjectStore>,
    /// Opt-in type codes to activate this run, derived from the unified config's
    /// `[diagnostics] select` (∩ the registry's opt-in set). Updated by
    /// [`TypeProvider::set_type_config`]; empty by default, so nothing opt-in runs
    /// until the user selects it — parity with the CLI's `enabled_opt_in`.
    enabled_opt_in: RwLock<HashSet<String>>,
}

impl M1Type {
    pub fn new(store: Arc<ProjectStore>) -> Self {
        Self {
            store,
            enabled_opt_in: RwLock::new(HashSet::new()),
        }
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
        let enabled = self.enabled_opt_in.read().unwrap();
        // Check against the loaded project *with* the project-wide solved channel
        // taints seeded in, so a cross-script T080/T081 whose source lives in
        // another file surfaces at this file's sinks (parity with the CLI's
        // `check_script_with_channels`, not the taint-blind `check_script`). The
        // project already carries inferred user-function return types (applied at
        // load), so call sites here type-check instead of resolving to Unknown.
        // Opt-in rules run only for the codes the config selected (`enabled`).
        let (result, prj) = self.store.with_cross_script(|cs| match cs {
            Some((lp, taints)) => (
                check_script_with_channels(
                    &enabled,
                    Some(&lp.project),
                    Some(path.as_path()),
                    src,
                    taints,
                ),
                Some(lp.m1prj_path.clone()),
            ),
            None => (
                // No project: still honour opt-in selection, but there is no
                // channel identity to propagate, so pass empty taints.
                check_script_with_channels(
                    &enabled,
                    None,
                    Some(path.as_path()),
                    src,
                    &m1_typecheck::cross_script::ChannelTaints::default(),
                ),
                None,
            ),
        });
        // Syntax errors are reported by m1-core in analyze(); ignore them here.
        result
            .diagnostics
            .iter()
            .map(|d| type_diagnostic(d, li, enc, prj.as_deref()))
            .collect()
    }

    fn project_loaded(&self) -> bool {
        self.store.project_loaded()
    }

    fn set_type_config(&self, diagnostics: &DiagFilter) {
        // Activate an opt-in code only when the resolved `select` names it —
        // exactly the CLI's `enabled_opt_in` derivation.
        let enabled: HashSet<String> = m1_typecheck::rules::Registry::opt_in_codes()
            .iter()
            .map(|c| c.as_str().to_string())
            .filter(|c| diagnostics.select.contains(c))
            .collect();
        *self.enabled_opt_in.write().unwrap() = enabled;
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

    fn has_code(diags: &[LspDiag], code: &str) -> bool {
        diags.iter().any(|d| {
            matches!(&d.code, Some(tower_lsp::lsp_types::NumberOrString::String(c)) if c == code)
        })
    }

    // Finding 1(c): an opt-in type rule (T064) is off by default but activates the
    // moment `[diagnostics] select` names it — the editor path now honours the
    // same opt-in selection the CLI does, instead of the rule silently never
    // running (which, combined with the post-filter, left the editor blank).
    #[test]
    fn opt_in_rule_activates_via_select() {
        // A wrong-arity call to a fully-modelled library method — T064's trigger,
        // and it needs no project.
        let src = "local x = Calculate.Max(1, 2, 3, 4, 5);\n";
        let store = Arc::new(ProjectStore::new());
        let p = M1Type::new(store);
        let uri = Url::parse("file:///x.m1scr").unwrap();
        let li = LineIndex::new(src);

        // Default config: opt-in T064 does not run.
        let diags = p.types(&uri, src, &li, PositionEncoding::Utf16);
        assert!(!has_code(&diags, "T064"), "T064 must be off by default");

        // Select T064: the rule now runs and surfaces in the editor path.
        let mut filter = DiagFilter::default();
        filter.select.insert("T064".into());
        p.set_type_config(&filter);
        let diags = p.types(&uri, src, &li, PositionEncoding::Utf16);
        assert!(
            has_code(&diags, "T064"),
            "T064 must fire once `select` names it: {diags:?}"
        );
    }

    // Finding 1(a): a cross-script T080 whose invalid-value SOURCE is a different
    // script now surfaces at this file's annotated sink in the editor path — the
    // provider seeds the project-wide solved channel taints (parity with the CLI's
    // `check_script_with_channels`), where the old taint-blind `check_script` left
    // it silent. Mirrors m1-typecheck's own `cross_script` integration scenario.
    #[test]
    fn cross_script_taint_surfaces_in_editor_path() {
        use std::io::Write;
        let tmp = tempfile::tempdir().unwrap();
        let prj = r#"<?xml version="1.0"?>
<Project>
  <Component Classname="BuiltIn.GroupCompound" Name="Root"/>
  <Component Classname="BuiltIn.GroupCompound" Name="Root.Sensors"/>
  <Component Classname="BuiltIn.GroupCompound" Name="Root.Control"/>
  <Component Classname="BuiltIn.Channel" Name="Root.Sensors.Yaw"><Props Type="f32"/></Component>
  <Component Classname="BuiltIn.Channel" Name="Root.Control.Demand"><Props Type="f32"/></Component>
</Project>"#;
        std::fs::File::create(tmp.path().join("Project.m1prj"))
            .unwrap()
            .write_all(prj.as_bytes())
            .unwrap();
        // The taint SOURCE lives in another file on disk (a division that can be
        // Inf/NaN feeding a channel).
        std::fs::write(
            tmp.path().join("Sensors.Update.m1scr"),
            "Sensors.Yaw = 1 / Control.Demand;\n",
        )
        .unwrap();
        // The sink file is the one being edited; its annotated read requires finite.
        let reader_src = "// @m1:requires-finite\nControl.Demand = Sensors.Yaw * 2;\n";
        let reader_path = tmp.path().join("Control.Update.m1scr");
        std::fs::write(&reader_path, reader_src).unwrap();

        let store = Arc::new(ProjectStore::new());
        assert!(store.discover_and_load(tmp.path()).unwrap());
        let p = M1Type::new(store);
        let uri = Url::from_file_path(&reader_path).unwrap();
        let li = LineIndex::new(reader_src);
        let diags = p.types(&uri, reader_src, &li, PositionEncoding::Utf16);
        assert!(
            has_code(&diags, "T080"),
            "cross-script T080 must surface at the annotated sink via the seeded \
             taints: {diags:?}"
        );
        // The provenance names the writing script, proving it's the cross-file
        // solve (not a local finding).
        assert!(
            diags
                .iter()
                .any(|d| d.message.contains("Sensors.Update.m1scr")),
            "T080 provenance should name the remote writer: {diags:?}"
        );
    }
}
