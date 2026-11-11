//! The diagnostics pipeline: computing a document's diagnostic set (open buffer
//! or disk), the shared project-scope conversion, the push path
//! ([`Backend::publish`]), the project-derived view refresh, and the pull
//! handlers (`textDocument/diagnostic`, `workspace/diagnostic`) with their LSP
//! 3.17 `result_id`/`Unchanged` reconciliation (#140, #259). The single source of
//! truth so push and pull report identically.
use tower_lsp::jsonrpc::Result;
use tower_lsp::lsp_types::*;

use super::Backend;
use super::capabilities::is_m1prj;
use crate::analysis::{analyze, analyze_with_cst};
use crate::line_index::PositionEncoding;

/// Outcome of reconciling freshly-computed pull diagnostics against the cached
/// snapshot for a document (see [`Backend::reconcile_diag`], #259): the
/// `result_id` to report, and whether the client already holds this exact set
/// (so the handler can answer `Unchanged`).
pub(super) struct DiagSync {
    pub(super) id: String,
    pub(super) unchanged: bool,
}

pub(super) struct DiagnosticSnapshot {
    items: Vec<Diagnostic>,
    version: Option<i32>,
    revision: Option<u64>,
    environment: u64,
    available: bool,
}

pub(super) fn retry_diagnostics() -> tower_lsp::jsonrpc::Error {
    tower_lsp::jsonrpc::Error {
        code: tower_lsp::jsonrpc::ErrorCode::ServerError(-32802),
        message: "diagnostic inputs changed".into(),
        data: Some(serde_json::json!({"retriggerRequest": true})),
    }
}

impl Backend {
    /// Reconcile freshly-computed pull diagnostics for `uri` against the cached
    /// snapshot, returning the `result_id` to report and whether the client's
    /// `previous_result_id` still labels the current set (#259).
    ///
    /// - If `items` equal the cached set, the cached `result_id` is reused;
    ///   `unchanged` is `true` when that id also matches `previous`, so the
    ///   handler can answer `Unchanged` and skip re-sending the items.
    /// - Otherwise a fresh id is minted and the snapshot replaced; `unchanged`
    ///   is `false`, so the handler sends a full report.
    ///
    /// Storing only on change keeps result ids stable across no-op polls, which
    /// is what lets a poll short-circuit to `Unchanged`.
    pub(super) fn reconcile_diag(
        &self,
        uri: &Url,
        items: &[Diagnostic],
        previous: Option<&str>,
    ) -> DiagSync {
        if let Some(entry) = self.diag_prev.get(uri)
            && entry.1 == items
        {
            let id = entry.0.clone();
            let unchanged = previous == Some(id.as_str());
            return DiagSync { id, unchanged };
        }
        let id = self
            .diag_seq
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            .to_string();
        self.diag_prev
            .insert(uri.clone(), (id.clone(), items.to_vec()));
        DiagSync {
            id,
            unchanged: false,
        }
    }

    /// Turn the store's project-scope [`TypeDiagnostic`](m1_typecheck::diagnostics::TypeDiagnostic)s
    /// into LSP [`Diagnostic`]s, applying the configured diagnostics filter once.
    ///
    /// This is the single pipeline shared by the push path (the `.m1prj` branch
    /// of [`diagnostics_for`](Self::diagnostics_for)) and the published path
    /// ([`publish_project_diagnostics`](Self::publish_project_diagnostics)): read
    /// the diagnostics filter from config, compute project diagnostics with the
    /// opt-in T089 rate-inversion check gated on `select` naming it, drop
    /// subjects the filter rejects, and map each through
    /// [`convert::type_diagnostic`](crate::convert::type_diagnostic). Keeping it
    /// in one place stops the two call sites diverging when the filter/convert
    /// logic changes (a new opt-in code, a different subject filter, …).
    ///
    /// `li`/`enc` are the line index and position encoding already computed at
    /// each call site — project diagnostics carry a zero byte-range, so any line
    /// index resolves them to line 0, but the encoding still governs column math.
    pub(super) fn project_lsp_diagnostics(
        &self,
        li: &crate::line_index::LineIndex,
        enc: PositionEncoding,
    ) -> Vec<Diagnostic> {
        let filter = self.config.read().unwrap().diagnostics.clone();
        let prj = self
            .store
            .with_project(|p| p.map(|lp| lp.m1prj_path.clone()));
        self.store
            .project_diagnostics_with(filter.select.contains("T089"))
            .iter()
            .filter(|d| filter.allows_subject(d.code.as_str(), d.subject.as_deref()))
            .map(|d| crate::convert::type_diagnostic(d, li, enc, prj.as_deref()))
            .collect()
    }

    /// Compute the full diagnostic set for `uri`, sourcing the text from the open
    /// buffer if present, else reading it from disk (tolerant decode). Returns
    /// an unavailable snapshot when neither source yields text. This is
    /// the single source of truth shared by the push path ([`publish`]) and the
    /// pull handlers (`textDocument/diagnostic`, `workspace/diagnostic`, #140) so
    /// all three report identically.
    ///
    /// The `.m1prj` is XML, not M1 script — running the script analysis on it
    /// would emit bogus syntax diagnostics. Instead, when it is the active
    /// project's file, surface the project-scope audit (T041/T050/…) anchored to
    /// it (#139); any other `.m1prj` reports nothing.
    pub(super) async fn diagnostics_for(&self, uri: &Url) -> DiagnosticSnapshot {
        let state = self.state.lock().await;
        let environment = state.environment;
        let doc = self.doc_context(uri);
        let version = doc.as_ref().map(|d| d.version);
        let revision = doc.as_ref().map(|d| d.revision);
        let source = match doc {
            Some(doc) => Some((doc.text, doc.line_index, Some(doc.cst))),
            None => uri.to_file_path().ok().and_then(|path| {
                let text = crate::disk_read::read_disk(&path)?;
                let li = crate::line_index::LineIndex::new(&text);
                Some((li.shared_text(), li, None))
            }),
        };
        let enc = self.enc();
        let filter = self.config.read().unwrap().diagnostics.clone();
        drop(state);
        let available = source.is_some();
        let items = tokio::task::block_in_place(|| {
            let Some((text, li, cst)) = source else {
                return vec![];
            };
            if is_m1prj(uri) {
                let active = self
                    .store
                    .with_project(|p| p.and_then(|lp| Url::from_file_path(&lp.m1prj_path).ok()));
                return if active.as_ref() == Some(uri) {
                    self.project_lsp_diagnostics(&li, enc)
                } else {
                    vec![]
                };
            }
            match cst {
                Some(cst) => analyze_with_cst(
                    &cst,
                    uri,
                    &text,
                    &li,
                    enc,
                    self.lint.as_ref(),
                    self.types.as_ref(),
                    &filter,
                ),
                None => analyze(
                    uri,
                    &text,
                    &li,
                    enc,
                    self.lint.as_ref(),
                    self.types.as_ref(),
                    &filter,
                ),
            }
        });
        DiagnosticSnapshot {
            items,
            version,
            revision,
            environment,
            available,
        }
    }

    fn diagnostic_is_current(
        &self,
        uri: &Url,
        snapshot: &DiagnosticSnapshot,
        state: &super::DiagnosticState,
    ) -> bool {
        state.environment == snapshot.environment
            && self.docs.get(uri).map(|d| d.revision) == snapshot.revision
    }

    pub(super) async fn publish(&self, uri: Url) {
        if self
            .client_pull_diagnostics
            .load(std::sync::atomic::Ordering::Relaxed)
        {
            return;
        }
        if is_m1prj(&uri) {
            self.publish_project_diagnostics().await;
            return;
        }
        self.store.revalidate_analysis();
        let snapshot = self.diagnostics_for(&uri).await;
        let state = self.state.lock().await;
        if snapshot.version.is_none() || !self.diagnostic_is_current(&uri, &snapshot, &state) {
            return;
        }
        // The gate orders validation and notification enqueue with state commits.
        self.client
            .publish_diagnostics(uri, snapshot.items, snapshot.version)
            .await;
    }

    pub(super) async fn publish_all(&self) {
        let uris: Vec<_> = self.docs.iter().map(|d| d.key().clone()).collect();
        for uri in uris {
            if !is_m1prj(&uri) {
                self.publish(uri).await;
            }
        }
        self.publish_project_diagnostics().await;
    }

    /// Nudge the client to re-pull every project-derived view (#232): inlay
    /// hints (`[unit]` badges), semantic tokens and code lenses are all
    /// computed from the project model, so they go stale on `.m1prj`/config
    /// reload until the client refreshes them. Each refresh is gated on the
    /// capability the client declared at initialize.
    ///
    /// The refreshes are requests (the client acknowledges each), but their
    /// replies carry nothing. Queue and coalesce them until the bounded
    /// dispatcher has capacity, rather than awaiting the calling handler.
    /// Awaiting a client round-trip
    /// inside a notification handler pins one of the serve window's
    /// concurrency slots for the round-trip's duration, and a client that
    /// never answers would pin it forever (#336).
    pub(super) fn refresh_project_views(&self) {
        use std::sync::atomic::Ordering::Relaxed;
        let inlay = self.inlay_refresh_support.load(Relaxed);
        let semtok = self.semtok_refresh_support.load(Relaxed);
        let lens = self.code_lens_refresh_support.load(Relaxed);
        if !(inlay || semtok || lens) {
            return;
        }
        use super::progress::DeferredRequest;
        if inlay {
            self.queue_client_request(DeferredRequest::InlayHints);
        }
        if semtok {
            self.queue_client_request(DeferredRequest::SemanticTokens);
        }
        if lens {
            self.queue_client_request(DeferredRequest::CodeLenses);
        }
    }

    /// Publish the project-scope diagnostics (the `.m1cfg`-coverage / name
    /// audits — T041/T050/T010/T071) anchored to the loaded `.m1prj`. These are
    /// not tied to any open script, so the editor shows them as soon as the
    /// project loads, matching what the CLI reports (#139). Publishes an empty
    /// set (clearing stale entries) when the project loaded cleanly with no
    /// findings, and clears the last publication when its project is unloaded.
    pub(super) async fn publish_project_diagnostics(&self) {
        // Every caller of this function has just (re)loaded the project model,
        // so the project-derived views need a refresh too (#232).
        self.refresh_project_views();

        // Pull-capable clients receive project-scope diagnostics via
        // `workspace/diagnostic`; after a project-model change (reload, `.m1prj`
        // or config edit) nudge them to re-pull instead of pushing — pushing here
        // too would duplicate diagnostics in VS Code (#NNN). Detached for the
        // same reason as `refresh_project_views` (#336).
        if self
            .client_pull_diagnostics
            .load(std::sync::atomic::Ordering::Relaxed)
        {
            self.queue_client_request(super::progress::DeferredRequest::Diagnostics);
            return;
        }
        let state = self.state.lock().await;
        let environment = state.environment;
        let current = self
            .store
            .with_project(|p| p.and_then(|lp| Url::from_file_path(&lp.m1prj_path).ok()));
        drop(state);
        let snapshot = match &current {
            Some(uri) => Some(self.diagnostics_for(uri).await),
            None => None,
        };
        let mut state = self.state.lock().await;
        if state.environment != environment {
            return;
        }
        if let (Some(uri), Some(snapshot)) = (&current, &snapshot)
            && !self.diagnostic_is_current(uri, snapshot, &state)
        {
            return;
        }
        if state.last_project != current
            && let Some(old) = state.last_project.take()
        {
            self.client.publish_diagnostics(old, vec![], None).await;
        }
        if let (Some(uri), Some(snapshot)) = (current, snapshot) {
            self.client
                .publish_diagnostics(uri.clone(), snapshot.items, snapshot.version)
                .await;
            state.last_project = Some(uri);
        }
    }

    /// Pull diagnostics for a single document (#140). Runs the same analysis as
    /// the push path, on demand, sourcing the text from the open buffer or disk —
    /// so a file the client has never opened still gets full coverage.
    pub(super) async fn diagnostic_impl(
        &self,
        params: DocumentDiagnosticParams,
    ) -> Result<DocumentDiagnosticReportResult> {
        let uri = &params.text_document.uri;
        // `diagnostics_for` falls back to a blocking disk read (and full
        // analyze()) for closed files, so run it on a blocking-aware worker via
        // `block_in_place` to keep the async runtime healthy (#135, #258).
        // Request boundary: probe the cross-script analysis cache for on-disk
        // staleness once, so the analyze below reuses it without re-probing
        // (#339).
        self.store.revalidate_analysis();
        let snapshot = self.diagnostics_for(uri).await;
        let state = self.state.lock().await;
        if !self.diagnostic_is_current(uri, &snapshot, &state) {
            return Err(retry_diagnostics());
        }
        let items = snapshot.items;
        // LSP 3.17 result_id/Unchanged (#259): if the recomputed set matches the
        // one the client already holds (its `previous_result_id`), answer
        // `Unchanged` instead of re-serializing every item.
        let sync = self.reconcile_diag(uri, &items, params.previous_result_id.as_deref());
        if sync.unchanged {
            return Ok(DocumentDiagnosticReportResult::Report(
                DocumentDiagnosticReport::Unchanged(RelatedUnchangedDocumentDiagnosticReport {
                    related_documents: None,
                    unchanged_document_diagnostic_report: UnchangedDocumentDiagnosticReport {
                        result_id: sync.id,
                    },
                }),
            ));
        }
        Ok(DocumentDiagnosticReportResult::Report(
            DocumentDiagnosticReport::Full(RelatedFullDocumentDiagnosticReport {
                related_documents: None,
                full_document_diagnostic_report: FullDocumentDiagnosticReport {
                    result_id: Some(sync.id),
                    items,
                },
            }),
        ))
    }

    /// Workspace-wide pull diagnostics (#140): run the analysis over every script
    /// in the loaded project (the `LoadedProject::script_files` cache) plus the
    /// active `.m1prj`, so whole-project lint and type findings are visible even
    /// for files that were never opened. Open buffers remain covered without a
    /// project; previous reports outside the current set are explicitly cleared.
    pub(super) async fn workspace_diagnostic_impl(
        &self,
        params: WorkspaceDiagnosticParams,
    ) -> Result<WorkspaceDiagnosticReportResult> {
        let state = self.state.lock().await;
        let environment = state.environment;
        let membership = state.membership;
        let mut current: std::collections::BTreeSet<Url> = self.store.with_project(|p| {
            p.into_iter()
                .flat_map(|lp| {
                    lp.script_files
                        .iter()
                        .chain(std::iter::once(&lp.m1prj_path))
                })
                .filter_map(|path| Url::from_file_path(path).ok())
                .collect()
        });
        current.extend(self.docs.iter().map(|d| d.key().clone()));
        let mut uris = current.clone();
        uris.extend(params.previous_result_ids.iter().map(|p| p.uri.clone()));
        drop(state);
        let previous: std::collections::HashMap<&Url, &str> = params
            .previous_result_ids
            .iter()
            .map(|p| (&p.uri, p.value.as_str()))
            .collect();
        let progress = self
            .progress_begin("workspace-diagnostics", "m1-lsp: checking workspace")
            .await;
        self.store.revalidate_analysis();
        let total = uris.len();
        let mut snapshots = Vec::with_capacity(total);
        for (done, uri) in uris.into_iter().enumerate() {
            if done > 0 && done % 25 == 0 {
                self.progress_report(&progress, format!("{done}/{total} scripts"))
                    .await;
            }
            let snapshot = if current.contains(&uri) {
                Some(self.diagnostics_for(&uri).await)
            } else {
                None
            };
            snapshots.push((uri, snapshot));
        }
        self.progress_end(progress).await;
        let state = self.state.lock().await;
        if environment != state.environment
            || membership != state.membership
            || snapshots.iter().any(|(uri, s)| {
                s.as_ref()
                    .is_some_and(|s| !self.diagnostic_is_current(uri, s, &state))
            })
        {
            return Err(retry_diagnostics());
        }
        let mut items = Vec::with_capacity(snapshots.len());
        for (uri, snapshot) in snapshots {
            let Some(snapshot) = snapshot.filter(|s| s.available) else {
                self.diag_prev.remove(&uri);
                items.push(WorkspaceDocumentDiagnosticReport::Full(
                    WorkspaceFullDocumentDiagnosticReport {
                        uri,
                        version: None,
                        full_document_diagnostic_report: FullDocumentDiagnosticReport {
                            result_id: None,
                            items: vec![],
                        },
                    },
                ));
                continue;
            };
            let version = snapshot.version.map(i64::from);
            let sync = self.reconcile_diag(&uri, &snapshot.items, previous.get(&uri).copied());
            if sync.unchanged {
                items.push(WorkspaceDocumentDiagnosticReport::Unchanged(
                    WorkspaceUnchangedDocumentDiagnosticReport {
                        uri,
                        version,
                        unchanged_document_diagnostic_report: UnchangedDocumentDiagnosticReport {
                            result_id: sync.id,
                        },
                    },
                ));
            } else {
                items.push(WorkspaceDocumentDiagnosticReport::Full(
                    WorkspaceFullDocumentDiagnosticReport {
                        uri,
                        version,
                        full_document_diagnostic_report: FullDocumentDiagnosticReport {
                            result_id: Some(sync.id),
                            items: snapshot.items,
                        },
                    },
                ));
            }
        }

        Ok(WorkspaceDiagnosticReportResult::Report(
            WorkspaceDiagnosticReport { items },
        ))
    }
}

#[cfg(test)]
mod project_diagnostics_pipeline_tests {
    use crate::analysis::{NoLint, NoTypes};
    use crate::backend::Backend;
    use crate::format::NoFormat;
    use crate::project_store::ProjectStore;
    use std::sync::Arc;
    use tower_lsp::{LspService, lsp_types::*};

    // A `.m1prj` with a parameter that is absent from the sibling `.m1cfg`,
    // which the project audit flags as T041 — a non-empty project-scope
    // diagnostic set the pipeline must surface.
    const M1PRJ: &str = "<?xml version=\"1.0\"?>\n<Project>\n\
         <Component Classname=\"BuiltIn.GroupCompound\" Name=\"Root\"/>\n\
         <Component Classname=\"BuiltIn.GroupCompound\" Name=\"Root.A\"/>\n\
         <Component Classname=\"BuiltIn.Parameter\" Name=\"Root.A.Missing\"><Props Type=\"u32\"/></Component>\n\
         </Project>";
    const M1CFG: &str = "<?xml version=\"1.0\"?>\n<Configuration>\n <Group Name=\"\">\n\
         </Group>\n</Configuration>";

    // The push path (`diagnostics_for` on the active `.m1prj`) and the
    // published path (`publish_project_diagnostics`) both convert the store's
    // project-scope diagnostics into LSP diagnostics through the exact same
    // pipeline. This guards that the shared `project_lsp_diagnostics` helper is
    // the one source of that conversion: `diagnostics_for` on the active
    // `.m1prj` must equal a direct call to `project_lsp_diagnostics` for the
    // same line index / encoding. If a future edit reintroduces a divergent
    // inline copy at either site, this test fails.
    #[tokio::test(flavor = "multi_thread")]
    async fn push_path_uses_shared_project_pipeline() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("Project.m1prj"), M1PRJ).unwrap();
        std::fs::write(tmp.path().join("parameters.m1cfg"), M1CFG).unwrap();

        let store = Arc::new(ProjectStore::new());
        assert!(
            store.discover_and_load(tmp.path()).unwrap(),
            "fixture project must load"
        );

        let (service, _socket) = LspService::new(|client| {
            Backend::with_backends(
                client,
                Box::new(NoLint),
                Box::new(NoTypes),
                Box::new(NoFormat),
                Arc::clone(&store),
            )
        });
        let backend = service.inner();

        let prj_path = store
            .with_project(|p| p.map(|lp| lp.m1prj_path.clone()))
            .expect("project loaded");
        let uri = Url::from_file_path(&prj_path).unwrap();

        // The push path: full diagnostics for the active `.m1prj`.
        let via_push = backend.diagnostics_for(&uri).await.items;
        assert!(
            !via_push.is_empty(),
            "fixture should surface a project-scope diagnostic (T041); got none"
        );

        // The shared helper directly, with the same line index/encoding the
        // push path computed for this file.
        let text = std::fs::read_to_string(&prj_path).unwrap_or_default();
        let li = crate::line_index::LineIndex::new(&text);
        let via_helper = backend.project_lsp_diagnostics(&li, backend.enc());

        assert_eq!(
            via_push, via_helper,
            "the .m1prj push path must produce exactly what the shared \
             project_lsp_diagnostics helper produces — they must not diverge"
        );
    }
}

#[cfg(test)]
mod workspace_removal_tests {
    use crate::backend::Backend;
    use crate::document::Document;
    use serde_json::{Value, json};
    use tower_lsp::{LspService, lsp_types::*};

    async fn report(backend: &Backend, previous: Value) -> Value {
        let params = serde_json::from_value(json!({"previousResultIds": previous})).unwrap();
        serde_json::to_value(backend.workspace_diagnostic_impl(params).await.unwrap()).unwrap()
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn workspace_pull_clears_removed_unreadable_and_unloaded_files() {
        for action in ["delete", "unreadable", "unload", "switch", "delete_open"] {
            let tmp = tempfile::tempdir().unwrap();
            let other = tempfile::tempdir().unwrap();
            let prj = tmp.path().join("Project.m1prj");
            let script = tmp.path().join("Test.m1scr");
            let xml =
                r#"<Project><Component Classname="BuiltIn.GroupCompound" Name="Root"/></Project>"#;
            std::fs::write(&prj, xml).unwrap();
            std::fs::write(&script, "local broken = ;\n").unwrap();
            let (service, _socket) = LspService::new(Backend::new);
            let backend = service.inner();
            backend.store.discover_and_load(tmp.path()).unwrap();
            let uri = Url::from_file_path(&script).unwrap();
            if action == "delete_open" {
                backend
                    .docs
                    .insert(uri.clone(), Document::new("local broken = ;\n".into(), 7));
            }
            let first = report(backend, json!([])).await;
            let items = first["items"].as_array().unwrap();
            let initial = items.iter().find(|i| i["uri"] == uri.as_str()).unwrap();
            assert!(
                !initial["items"].as_array().unwrap().is_empty(),
                "fixture must diagnose"
            );
            let previous: Vec<_> = items
                .iter()
                .map(|i| json!({"uri": i["uri"], "value": i["resultId"]}))
                .collect();
            match action {
                "delete" | "delete_open" => {
                    std::fs::remove_file(&script).unwrap();
                    backend.store.refresh_scripts();
                }
                "unreadable" => {
                    std::fs::remove_file(&script).unwrap();
                    std::fs::create_dir(&script).unwrap();
                }
                "unload" => {
                    std::fs::write(&prj, "<bad").unwrap();
                    assert!(backend.store.load_from(&prj).is_err());
                }
                "switch" => {
                    let new_prj = other.path().join("Project.m1prj");
                    std::fs::write(&new_prj, xml).unwrap();
                    backend.store.load_from(&new_prj).unwrap();
                }
                _ => unreachable!(),
            }
            let next = report(backend, json!(previous)).await;
            let returned = next["items"].as_array().unwrap();
            let entry = returned
                .iter()
                .find(|i| i["uri"] == uri.as_str())
                .unwrap_or_else(|| panic!("missing clearing report for {action}: {next}"));
            if action == "delete_open" {
                assert_eq!(entry["kind"], "unchanged");
                assert_eq!(entry["resultId"], initial["resultId"]);
                assert_eq!(entry["version"], 7);
            } else {
                assert_eq!(entry["kind"], "full", "{action}");
                assert_eq!(entry["items"], json!([]), "{action}");
                assert!(
                    !backend.diag_prev.contains_key(&uri),
                    "removed files must not accumulate tombstones"
                );
                if matches!(action, "unload" | "switch") {
                    let old_uri = Url::from_file_path(&prj).unwrap();
                    let old = returned
                        .iter()
                        .find(|i| i["uri"] == old_uri.as_str())
                        .unwrap();
                    assert_eq!(old["kind"], "full");
                    assert_eq!(old["items"], json!([]));
                }
            }
        }
    }
}

#[cfg(test)]
mod snapshot_tests {
    use super::*;
    use crate::analysis::{LintProvider, NoTypes};
    use crate::document::Document;
    use crate::format::NoFormat;
    use crate::line_index::LineIndex;
    use crate::project_store::ProjectStore;
    use std::sync::{Arc, Condvar, Mutex};
    use std::time::Duration;
    use tower_lsp::LspService;

    struct Pause {
        entered: Mutex<Option<tokio::sync::oneshot::Sender<()>>>,
        released: Mutex<bool>,
        ready: Condvar,
    }
    struct PausedLint(Arc<Pause>);
    impl LintProvider for PausedLint {
        fn lint(&self, _: &Url, _: &str, _: &LineIndex, _: PositionEncoding) -> Vec<Diagnostic> {
            let entered = self.0.entered.lock().unwrap().take();
            if let Some(entered) = entered {
                let _ = entered.send(());
                let (released, _) = self
                    .0
                    .ready
                    .wait_timeout_while(
                        self.0.released.lock().unwrap(),
                        Duration::from_secs(5),
                        |r| !*r,
                    )
                    .unwrap();
                assert!(*released, "test did not release delayed analysis");
            }
            vec![]
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn obsolete_pull_results_retry_without_repopulating_the_cache() {
        for workspace in [false, true] {
            for change in ["edit", "reopen", "config"] {
                let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
                let pause = Arc::new(Pause {
                    entered: Mutex::new(Some(entered_tx)),
                    released: Mutex::new(false),
                    ready: Condvar::new(),
                });
                let (service, _socket) = LspService::new(Backend::new);
                let backend = Arc::new(Backend::with_backends(
                    service.inner().client.clone(),
                    Box::new(PausedLint(pause.clone())),
                    Box::new(NoTypes),
                    Box::new(NoFormat),
                    Arc::new(ProjectStore::new()),
                ));
                backend
                    .client_pull_diagnostics
                    .store(true, std::sync::atomic::Ordering::Relaxed);
                let tmp = tempfile::tempdir().unwrap();
                let uri = Url::from_file_path(tmp.path().join("Test.m1scr")).unwrap();
                backend
                    .docs
                    .insert(uri.clone(), Document::new("local bad = ;\n".into(), 1));
                let worker = backend.clone();
                let worker_uri = uri.clone();
                let work = tokio::spawn(async move {
                    if workspace {
                        worker
                            .workspace_diagnostic_impl(
                                serde_json::from_value(serde_json::json!({"previousResultIds":[]}))
                                    .unwrap(),
                            )
                            .await
                            .map(|_| ())
                    } else {
                        worker
                            .diagnostic_impl(
                                serde_json::from_value(
                                    serde_json::json!({"textDocument":{"uri":worker_uri}}),
                                )
                                .unwrap(),
                            )
                            .await
                            .map(|_| ())
                    }
                });
                tokio::time::timeout(Duration::from_secs(3), entered_rx)
                    .await
                    .unwrap()
                    .unwrap();
                match change {
                    "edit" => backend.did_change_impl(serde_json::from_value(serde_json::json!({"textDocument":{"uri":uri,"version":2},"contentChanges":[{"text":"local good = 1;\n"}]})).unwrap()).await,
                    "reopen" => {
                        backend.did_close_impl(DidCloseTextDocumentParams { text_document: TextDocumentIdentifier { uri: uri.clone() } }).await;
                        backend.did_open_impl(DidOpenTextDocumentParams { text_document: TextDocumentItem { uri: uri.clone(), language_id: "m1".into(), version: 1, text: "local good = 1;\n".into() } }).await;
                    }
                    "config" => backend.did_change_configuration_impl(DidChangeConfigurationParams { settings: serde_json::json!({}) }).await,
                    _ => unreachable!(),
                }
                *pause.released.lock().unwrap() = true;
                pause.ready.notify_all();
                let error = tokio::time::timeout(Duration::from_secs(3), work)
                    .await
                    .unwrap()
                    .unwrap()
                    .unwrap_err();
                assert_eq!(
                    error.code,
                    tower_lsp::jsonrpc::ErrorCode::ServerError(-32802)
                );
                assert_eq!(error.data.unwrap()["retriggerRequest"], true);
                assert!(!backend.diag_prev.contains_key(&uri));
                let fresh = backend.diagnostics_for(&uri).await;
                let state = backend.state.lock().await;
                assert!(backend.diagnostic_is_current(&uri, &fresh, &state));
                if change != "config" {
                    assert!(fresh.items.is_empty());
                }
                assert_eq!(fresh.version, Some(if change == "edit" { 2 } else { 1 }));
            }
        }
    }
    #[tokio::test(flavor = "multi_thread")]
    async fn delayed_push_cannot_erase_or_relabel_a_reopened_document() {
        use tokio::io::{AsyncReadExt, duplex};
        use tower_service::Service;
        for text in ["local good = 1;\n", "local newer = ;\n"] {
            let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
            let pause = Arc::new(Pause {
                entered: Mutex::new(Some(entered_tx)),
                released: Mutex::new(false),
                ready: Condvar::new(),
            });
            let (mut service, socket) = LspService::new(Backend::new);
            // Initialize the actual client channel, then drive the tested Backend
            // directly so completion of the delayed push is observable without sleeps.
            service
                .call(
                    tower_lsp::jsonrpc::Request::build("initialize")
                        .id(1)
                        .params(serde_json::json!({"capabilities":{}}))
                        .finish(),
                )
                .await
                .unwrap();
            let backend = Arc::new(Backend::with_backends(
                service.inner().client.clone(),
                Box::new(PausedLint(pause.clone())),
                Box::new(NoTypes),
                Box::new(NoFormat),
                Arc::new(ProjectStore::new()),
            ));
            let (_input, server_input) = duplex(1024);
            let (server_output, mut output) = duplex(65536);
            let server = tokio::spawn(
                tower_lsp::Server::new(server_input, server_output, socket).serve(service),
            );
            let tmp = tempfile::tempdir().unwrap();
            let uri = Url::from_file_path(tmp.path().join("Test.m1scr")).unwrap();
            backend
                .docs
                .insert(uri.clone(), Document::new("local original = ;\n".into(), 1));
            let worker = backend.clone();
            let worker_uri = uri.clone();
            let old = tokio::spawn(async move {
                worker.publish(worker_uri).await;
            });
            tokio::time::timeout(Duration::from_secs(3), entered_rx)
                .await
                .unwrap()
                .unwrap();
            backend
                .did_close_impl(DidCloseTextDocumentParams {
                    text_document: TextDocumentIdentifier { uri: uri.clone() },
                })
                .await;
            backend
                .did_open_impl(DidOpenTextDocumentParams {
                    text_document: TextDocumentItem {
                        uri: uri.clone(),
                        language_id: "m1".into(),
                        version: 1,
                        text: text.into(),
                    },
                })
                .await;
            *pause.released.lock().unwrap() = true;
            pause.ready.notify_all();
            tokio::time::timeout(Duration::from_secs(3), old)
                .await
                .unwrap()
                .unwrap();
            let expected = backend.diagnostics_for(&uri).await.items;
            backend
                .client
                .log_message(MessageType::INFO, "flush completed pushes")
                .await;
            let read = async {
                let mut publications = Vec::new();
                loop {
                    let mut header = Vec::new();
                    while !header.ends_with(b"\r\n\r\n") {
                        header.push(output.read_u8().await.unwrap());
                    }
                    let header = String::from_utf8(header).unwrap();
                    let len: usize = header
                        .lines()
                        .find_map(|l| l.strip_prefix("Content-Length: "))
                        .unwrap()
                        .parse()
                        .unwrap();
                    let mut body = vec![0; len];
                    output.read_exact(&mut body).await.unwrap();
                    let message: serde_json::Value = serde_json::from_slice(&body).unwrap();
                    if message["params"]["message"] == "flush completed pushes" {
                        break;
                    }
                    if message["method"] == "textDocument/publishDiagnostics" {
                        publications.push(message["params"].clone());
                    }
                }
                publications
            };
            let publications = tokio::time::timeout(Duration::from_secs(3), read)
                .await
                .unwrap();
            server.abort();
            assert_eq!(
                publications.len(),
                2,
                "old analysis must not publish after reopen: {publications:?}"
            );
            assert_eq!(publications[0]["diagnostics"], serde_json::json!([]));
            assert!(publications[0]["version"].is_null());
            assert_eq!(publications[1]["version"], 1);
            assert_eq!(
                publications[1]["diagnostics"],
                serde_json::to_value(expected).unwrap()
            );
        }
    }
}
