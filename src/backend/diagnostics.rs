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
    /// `None` only when neither source yields text (the file vanished). This is
    /// the single source of truth shared by the push path ([`publish`]) and the
    /// pull handlers (`textDocument/diagnostic`, `workspace/diagnostic`, #140) so
    /// all three report identically.
    ///
    /// The `.m1prj` is XML, not M1 script — running the script analysis on it
    /// would emit bogus syntax diagnostics. Instead, when it is the active
    /// project's file, surface the project-scope audit (T041/T050/…) anchored to
    /// it (#139); any other `.m1prj` reports nothing.
    pub(super) fn diagnostics_for(&self, uri: &Url) -> Option<Vec<Diagnostic>> {
        // Open buffers carry the incrementally-maintained CST (#270), reused for
        // the syntax pass instead of re-parsing every keystroke; closed files (the
        // pull path's coverage) are read from disk and parsed fresh.
        let (text, lindex, warm_cst) = match self.docs.get(uri) {
            Some(doc) => (
                doc.text.clone(),
                doc.line_index.clone(),
                Some(doc.cst.clone()),
            ),
            None => {
                let path = uri.to_file_path().ok()?;
                let text = crate::disk_read::read_disk(&path)?;
                let li = crate::line_index::LineIndex::new(&text);
                (text, li, None)
            }
        };
        let enc = self.enc();
        if is_m1prj(uri) {
            let active = self
                .store
                .with_project(|p| p.and_then(|lp| Url::from_file_path(&lp.m1prj_path).ok()));
            return Some(if active.as_ref() == Some(uri) {
                self.project_lsp_diagnostics(&lindex, enc)
            } else {
                vec![]
            });
        }
        let filter = self.config.read().unwrap().diagnostics.clone();
        Some(match warm_cst {
            Some(cst) => analyze_with_cst(
                &cst,
                uri,
                &text,
                &lindex,
                enc,
                self.lint.as_ref(),
                self.types.as_ref(),
                &filter,
            ),
            None => analyze(
                uri,
                &text,
                &lindex,
                enc,
                self.lint.as_ref(),
                self.types.as_ref(),
                &filter,
            ),
        })
    }

    pub(super) async fn publish(&self, uri: Url) {
        // Pull-capable clients re-request `textDocument/diagnostic` for a document
        // on open/change themselves; also pushing would duplicate every diagnostic
        // in clients that keep push and pull in separate collections (VS Code).
        if self
            .client_pull_diagnostics
            .load(std::sync::atomic::Ordering::Relaxed)
        {
            return;
        }
        // Push is only for open buffers; the version comes from the open doc.
        // (Closed-file coverage is the pull path's job, #140.)
        let Some(version) = self.docs.get(&uri).map(|d| d.version) else {
            return;
        };
        let diags = self.diagnostics_for(&uri).unwrap_or_default();
        self.client
            .publish_diagnostics(uri, diags, Some(version))
            .await;
    }

    /// Nudge the client to re-pull every project-derived view (#232): inlay
    /// hints (`[unit]` badges), semantic tokens and code lenses are all
    /// computed from the project model, so they go stale on `.m1prj`/config
    /// reload until the client refreshes them. Each refresh is gated on the
    /// capability the client declared at initialize.
    ///
    /// The refreshes are requests (the client acknowledges each), but their
    /// replies carry nothing — so they run on a detached task rather than
    /// being awaited in the calling handler. Awaiting a client round-trip
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
        let client = self.client.clone();
        tokio::spawn(async move {
            if inlay {
                let _ = client.inlay_hint_refresh().await;
            }
            if semtok {
                let _ = client.semantic_tokens_refresh().await;
            }
            if lens {
                let _ = client.code_lens_refresh().await;
            }
        });
    }

    /// Publish the project-scope diagnostics (the `.m1cfg`-coverage / name
    /// audits — T041/T050/T010/T071) anchored to the loaded `.m1prj`. These are
    /// not tied to any open script, so the editor shows them as soon as the
    /// project loads, matching what the CLI reports (#139). Publishes an empty
    /// set (clearing stale entries) when the project loaded cleanly with no
    /// findings; a no-op when no project is loaded.
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
            let client = self.client.clone();
            tokio::spawn(async move {
                let _ = client.workspace_diagnostic_refresh().await;
            });
            return;
        }
        let Some(prj_path) = self
            .store
            .with_project(|p| p.map(|lp| lp.m1prj_path.clone()))
        else {
            return;
        };
        let Ok(uri) = Url::from_file_path(&prj_path) else {
            return;
        };
        // Project diagnostics carry a zero byte-range (no script location), which
        // maps to line 0 regardless of the index contents; build it from the
        // open buffer if any, else the file on disk.
        let text = self
            .docs
            .get(&uri)
            .map(|d| d.text.clone())
            .or_else(|| crate::disk_read::read_disk(&prj_path))
            .unwrap_or_default();
        let li = crate::line_index::LineIndex::new(&text);
        let enc = self.enc();
        let diags = self.project_lsp_diagnostics(&li, enc);
        let version = self.docs.get(&uri).map(|d| d.version);
        self.client.publish_diagnostics(uri, diags, version).await;
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
        let items = tokio::task::block_in_place(|| self.diagnostics_for(uri).unwrap_or_default());
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
    /// for files that were never opened. A no-op (empty report) when no project
    /// is loaded.
    pub(super) async fn workspace_diagnostic_impl(
        &self,
        params: WorkspaceDiagnosticParams,
    ) -> Result<WorkspaceDiagnosticReportResult> {
        // Snapshot the paths to report: every discovered script, and the project
        // file itself (for the project-scope audit).
        let mut paths = self
            .store
            .with_project(|p| p.map(|lp| lp.script_files.clone()))
            .unwrap_or_default();
        if let Some(prj) = self
            .store
            .with_project(|p| p.map(|lp| lp.m1prj_path.clone()))
        {
            paths.push(prj);
        }

        // The result ids the client says it already holds, by URI (LSP 3.17),
        // so a per-document poll can short-circuit to `Unchanged` (#259).
        let previous: std::collections::HashMap<&Url, &str> = params
            .previous_result_ids
            .iter()
            .map(|p| (&p.uri, p.value.as_str()))
            .collect();

        // On a real corpus this walks ~200 scripts and takes seconds — report
        // progress so the editor shows what is happening instead of a frozen
        // spinner (#266).
        let progress = self
            .progress_begin("workspace-diagnostics", "m1-lsp: checking workspace")
            .await;
        let total = paths.len();

        // `diagnostics_for` does blocking disk reads (and full analyze()) for
        // closed files — the common case here, since closed-file coverage is the
        // pull path's job (#140) — once per script in the loop. Run the whole
        // collection under a single `block_in_place` guard so the blocking work
        // doesn't starve the async runtime (#135, #258).
        let handle = tokio::runtime::Handle::current();
        let items = tokio::task::block_in_place(|| {
            let mut items = Vec::with_capacity(paths.len());
            for (done, path) in paths.into_iter().enumerate() {
                if done % 25 == 0 && done > 0 {
                    handle.block_on(
                        self.progress_report(&progress, format!("{done}/{total} scripts")),
                    );
                }
                let Ok(uri) = Url::from_file_path(&path) else {
                    continue;
                };
                let Some(diags) = self.diagnostics_for(&uri) else {
                    continue;
                };
                // Report the in-editor version for open buffers so the client can
                // reconcile against its edits; `None` for closed files.
                let version = self.docs.get(&uri).map(|d| d.version as i64);
                let sync = self.reconcile_diag(&uri, &diags, previous.get(&uri).copied());
                // Unchanged since the client's last result id: skip the items.
                if sync.unchanged {
                    items.push(WorkspaceDocumentDiagnosticReport::Unchanged(
                        WorkspaceUnchangedDocumentDiagnosticReport {
                            uri,
                            version,
                            unchanged_document_diagnostic_report:
                                UnchangedDocumentDiagnosticReport { result_id: sync.id },
                        },
                    ));
                    continue;
                }
                items.push(WorkspaceDocumentDiagnosticReport::Full(
                    WorkspaceFullDocumentDiagnosticReport {
                        uri,
                        version,
                        full_document_diagnostic_report: FullDocumentDiagnosticReport {
                            result_id: Some(sync.id),
                            items: diags,
                        },
                    },
                ));
            }
            items
        });
        self.progress_end(progress).await;

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
        let via_push = backend
            .diagnostics_for(&uri)
            .expect("active .m1prj yields a diagnostic set");
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
