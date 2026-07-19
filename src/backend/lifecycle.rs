//! Server + document lifecycle: `initialize`/`initialized`, the document-sync
//! notifications (`didOpen`/`didChange`/`didSave`/`didClose`), watched-file and
//! configuration reloads, and the fallback project discovery. The handlers that
//! own client-capability recording and the in-memory document/project state.
use tower_lsp::jsonrpc::Result;
use tower_lsp::lsp_types::*;

use super::Backend;
use super::capabilities::{client_supports_pull_diagnostics, editor_settings, server_capabilities};
use crate::document::Document;
use crate::line_index::PositionEncoding;

impl Backend {
    pub(super) async fn initialize_impl(
        &self,
        params: InitializeParams,
    ) -> Result<InitializeResult> {
        // Record whether the client supports pull diagnostics (read before the
        // encoding negotiation below moves fields out of `capabilities`). If it
        // does, the server serves diagnostics via the pull handlers ONLY and
        // suppresses the push path — otherwise a pull-capable client that keeps
        // push and pull in separate collections (VS Code) shows everything twice.
        self.client_pull_diagnostics.store(
            client_supports_pull_diagnostics(&params.capabilities),
            std::sync::atomic::Ordering::Relaxed,
        );
        self.progress_support.store(
            params
                .capabilities
                .window
                .as_ref()
                .and_then(|w| w.work_done_progress)
                .unwrap_or(false),
            std::sync::atomic::Ordering::Relaxed,
        );

        // Negotiate position encoding: the client's list is in PREFERENCE
        // order (LSP spec), so pick the first entry we support (UTF-16 or
        // UTF-8). Default to UTF-16 when none is offered/supported.
        let chosen = params
            .capabilities
            .general
            .and_then(|g| g.position_encodings)
            .and_then(|encs| {
                encs.iter().find_map(|e| {
                    if *e == PositionEncodingKind::UTF16 {
                        Some((PositionEncoding::Utf16, PositionEncodingKind::UTF16))
                    } else if *e == PositionEncodingKind::UTF8 {
                        Some((PositionEncoding::Utf8, PositionEncodingKind::UTF8))
                    } else {
                        None
                    }
                })
            })
            .unwrap_or((PositionEncoding::Utf16, PositionEncodingKind::UTF16));
        *self.encoding.write().unwrap() = chosen.0;

        // Record whether the client supports dynamic registration of file
        // watching; we only register the watcher in `initialized` if it does.
        let supports_watch = params
            .capabilities
            .workspace
            .as_ref()
            .and_then(|w| w.did_change_watched_files.as_ref())
            .and_then(|d| d.dynamic_registration)
            .unwrap_or(false);
        self.watch_dynamic
            .store(supports_watch, std::sync::atomic::Ordering::Relaxed);

        // Refresh-support capabilities (#232), read before `capabilities` is
        // partially moved below.
        {
            use std::sync::atomic::Ordering::Relaxed;
            let ws = params.capabilities.workspace.as_ref();
            self.inlay_refresh_support.store(
                ws.and_then(|w| w.inlay_hint.as_ref())
                    .and_then(|c| c.refresh_support)
                    .unwrap_or(false),
                Relaxed,
            );
            self.semtok_refresh_support.store(
                ws.and_then(|w| w.semantic_tokens.as_ref())
                    .and_then(|c| c.refresh_support)
                    .unwrap_or(false),
                Relaxed,
            );
            self.code_lens_refresh_support.store(
                ws.and_then(|w| w.code_lens.as_ref())
                    .and_then(|c| c.refresh_support)
                    .unwrap_or(false),
                Relaxed,
            );
        }

        // Record whether the client supports change annotations, so a multi-file /
        // file-renaming rename can carry a confirmation preview (#151).
        let supports_annotations = params
            .capabilities
            .workspace
            .as_ref()
            .and_then(|w| w.workspace_edit.as_ref())
            .and_then(|we| we.change_annotation_support.as_ref())
            .is_some();
        self.change_annotation_support
            .store(supports_annotations, std::sync::atomic::Ordering::Relaxed);

        // Capture editor settings (the middle config layer, beneath `m1-tools.toml`).
        // The client sends `{ "settings": { lint, format, diagnostics } }`; accept a
        // bare `{ lint, … }` object too.
        if let Some(opts) = params.initialization_options {
            *self.editor_settings.write().unwrap() = Some(editor_settings(opts));
        }

        // Discover the project from root_uri (fall back to first workspace folder).
        let root = params
            .root_uri
            .as_ref()
            .and_then(|u| u.to_file_path().ok())
            .or_else(|| {
                params
                    .workspace_folders
                    .as_ref()
                    .and_then(|fs| fs.first())
                    .and_then(|f| f.uri.to_file_path().ok())
            });
        if let Some(root) = root {
            match self.store.discover_and_load(&root) {
                Ok(true) => { /* loaded */ }
                Ok(false) => { /* project-less mode */ }
                Err(e) => {
                    self.client
                        .log_message(
                            MessageType::WARNING,
                            format!("m1-lsp: project load failed: {e}"),
                        )
                        .await;
                }
            }
            // Resolve the unified config (editor settings + `m1-tools.toml`,
            // legacy `.m1lint.toml` fallback).
            self.apply_config(&root);
        }

        Ok(InitializeResult {
            server_info: Some(ServerInfo {
                name: "m1-lsp".into(),
                version: Some(env!("CARGO_PKG_VERSION").into()),
            }),
            capabilities: server_capabilities(chosen.1),
        })
    }

    pub(super) async fn initialized_impl(&self, _: InitializedParams) {
        // Only register dynamic file watching if the client advertised support
        // for it; registering otherwise fails silently on such clients.
        if self
            .watch_dynamic
            .load(std::sync::atomic::Ordering::Relaxed)
        {
            let watchers = vec![
                FileSystemWatcher {
                    glob_pattern: GlobPattern::String("**/*.m1prj".into()),
                    kind: None,
                },
                FileSystemWatcher {
                    glob_pattern: GlobPattern::String("**/*.m1cfg".into()),
                    kind: None,
                },
                // .m1dbc CAN databases feed the project model (augment_dbc) and
                // are already reload triggers in project_store::is_watched —
                // without this registration the events never arrived (#276).
                FileSystemWatcher {
                    glob_pattern: GlobPattern::String("**/*.m1dbc".into()),
                    kind: None,
                },
                FileSystemWatcher {
                    glob_pattern: GlobPattern::String("**/.m1lint.toml".into()),
                    kind: None,
                },
                FileSystemWatcher {
                    glob_pattern: GlobPattern::String("**/.m1fmt.toml".into()),
                    kind: None,
                },
                FileSystemWatcher {
                    glob_pattern: GlobPattern::String("**/m1-tools.toml".into()),
                    kind: None,
                },
                // Script create/delete changes the workspace script set that
                // cross-file references and rename walk; refresh the cached list.
                FileSystemWatcher {
                    glob_pattern: GlobPattern::String("**/*.m1scr".into()),
                    kind: None,
                },
            ];
            let options =
                match serde_json::to_value(DidChangeWatchedFilesRegistrationOptions { watchers }) {
                    Ok(v) => v,
                    Err(e) => {
                        self.client
                            .log_message(
                                MessageType::ERROR,
                                format!(
                                    "m1-lsp: failed to serialize file-watcher options, \
                                     dynamic file-watching disabled: {e}"
                                ),
                            )
                            .await;
                        return;
                    }
                };
            let reg = Registration {
                id: "m1-lsp-watch-project".into(),
                method: "workspace/didChangeWatchedFiles".into(),
                register_options: Some(options),
            };
            let _ = self.client.register_capability(vec![reg]).await;
        } else {
            self.client
                .log_message(
                    MessageType::INFO,
                    "m1-lsp: client does not support dynamic file-watching; \
                     .m1prj/.m1cfg auto-reload disabled",
                )
                .await;
        }
        self.client
            .log_message(MessageType::INFO, "m1-lsp ready (v2)")
            .await;
        // Surface the project-scope audit (T041/T050/…) now that the client is
        // ready to receive diagnostics (#139).
        self.publish_project_diagnostics().await;
    }

    pub(super) async fn did_open_impl(&self, params: DidOpenTextDocumentParams) {
        let d = params.text_document;
        self.docs
            .insert(d.uri.clone(), Document::new(d.text, d.version));
        // A new/updated buffer can change script reads/writes — drop the cached
        // call graph so the next call-hierarchy request rebuilds from live text.
        self.store.invalidate_call_graph();
        // Some clients open a file without ever sending a `rootUri`/workspace
        // folder at `initialize`, leaving the server project-less. Fall back to
        // discovering the project from the opened file itself (#73).
        self.ensure_project_loaded(&d.uri).await;
        self.publish(d.uri).await;
    }

    pub(super) async fn did_change_impl(&self, params: DidChangeTextDocumentParams) {
        // INCREMENTAL sync (#270): apply each ranged change in order (per the
        // LSP, every range refers to the state after the previous change);
        // `range: None` remains the full-replacement fallback. Each ranged
        // change reparses incrementally, reusing untouched subtrees.
        let uri = params.text_document.uri;
        let enc = self.enc();
        if let Some(mut doc) = self.docs.get_mut(&uri) {
            for change in params.content_changes {
                doc.apply_change(change.range, &change.text, enc);
            }
            doc.version = params.text_document.version;
        } else if let Some(change) = params.content_changes.into_iter().last() {
            // No open document (shouldn't happen): only a full change can
            // seed one.
            if change.range.is_none() {
                self.docs.insert(
                    uri.clone(),
                    Document::new(change.text, params.text_document.version),
                );
            }
        }
        // The edited buffer can change script reads/writes — drop the cached
        // call graph (rebuilt on the next call-hierarchy request).
        self.store.invalidate_call_graph();
        self.publish(uri).await;
    }

    pub(super) async fn did_save_impl(&self, params: DidSaveTextDocumentParams) {
        // Disk now matches the buffer; the graph reads buffers first, so this is
        // belt-and-braces, but keeps the cache honest for any disk-sourced script.
        self.store.invalidate_call_graph();
        self.publish(params.text_document.uri).await;
    }

    pub(super) async fn did_close_impl(&self, params: DidCloseTextDocumentParams) {
        let uri = params.text_document.uri;
        self.docs.remove(&uri);
        self.semtok_prev.remove(&uri);
        self.diag_prev.remove(&uri);
        // The graph would now read this file from disk instead of the buffer.
        self.store.invalidate_call_graph();
        self.client.publish_diagnostics(uri, vec![], None).await;
    }

    pub(super) async fn did_change_watched_files_impl(&self, params: DidChangeWatchedFilesParams) {
        let touches_project = params.changes.iter().any(|c| {
            c.uri
                .to_file_path()
                .map(|p| {
                    self.store.is_watched(&p)
                        || matches!(
                            p.extension().and_then(|x| x.to_str()),
                            Some("m1prj") | Some("m1cfg")
                        )
                })
                .unwrap_or(false)
        });
        // An `m1-tools.toml` (or legacy `.m1lint.toml`) change re-resolves the
        // unified config from the file's directory.
        let config_change = params.changes.iter().find_map(|c| {
            let p = c.uri.to_file_path().ok()?;
            let name = p.file_name().and_then(|n| n.to_str())?;
            matches!(name, ".m1lint.toml" | ".m1fmt.toml" | "m1-tools.toml").then_some(p)
        });
        if let Some(p) = &config_change
            && let Some(dir) = p.parent()
        {
            self.apply_config(dir);
        }
        // A created/deleted `.m1scr` changes the cached workspace script set
        // (an edit to an existing one doesn't); refresh it cheaply, no reparse.
        let scripts_changed = params.changes.iter().any(|c| {
            c.uri
                .to_file_path()
                .ok()
                .map(|p| p.extension().and_then(|x| x.to_str()) == Some("m1scr"))
                .unwrap_or(false)
        });
        if scripts_changed {
            self.store.refresh_scripts();
        }
        if !touches_project && config_change.is_none() && !scripts_changed {
            return;
        }
        // Reload the project from the known .m1prj path if any, else rediscover.
        let result = if touches_project {
            let reloaded = self
                .store
                .with_project(|p| p.map(|lp| lp.m1prj_path.clone()));
            match reloaded {
                Some(path) => self.store.load_from(&path),
                None => {
                    // A new project appeared; rediscover from the first changed file's directory.
                    let dir = params
                        .changes
                        .first()
                        .and_then(|c| c.uri.to_file_path().ok())
                        .and_then(|p| p.parent().map(|d| d.to_path_buf()));
                    match dir {
                        Some(d) => self.store.discover_and_load(&d),
                        None => Ok(false),
                    }
                }
            }
        } else {
            Ok(false)
        };
        if let Err(e) = result {
            self.client
                .log_message(MessageType::WARNING, format!("m1-lsp: reload failed: {e}"))
                .await;
        }
        // Re-publish for all open docs so T001 refreshes.
        let uris: Vec<Url> = self.docs.iter().map(|e| e.key().clone()).collect();
        for uri in uris {
            self.publish(uri).await;
        }
        // A `.m1cfg`/`.m1prj` edit can change cfg coverage or names — re-audit (#139).
        self.publish_project_diagnostics().await;
    }

    pub(super) async fn did_change_configuration_impl(&self, params: DidChangeConfigurationParams) {
        // New editor settings (the middle config layer). Re-resolve against the
        // current workspace root and re-publish so the change takes effect live.
        *self.editor_settings.write().unwrap() = Some(editor_settings(params.settings));
        self.reapply_config();
        // A settings change can re-point or re-enable the eval source (`m1.eval.*`),
        // and a scenario/log file's *content* may have changed on disk without the
        // config value changing — so drop the cached trace here too (E3). The
        // resolved `EvalConfig` hash already forces a rebuild when the value
        // changes; this also covers the same-value-different-file-content case.
        // The next hover/inlay request rebuilds against the new config.
        self.store.invalidate_call_graph();
        let uris: Vec<Url> = self.docs.iter().map(|e| e.key().clone()).collect();
        for uri in uris {
            self.publish(uri).await;
        }
        // `publish` no-ops for pull-diagnostics clients (VS Code), so the loop
        // above leaves their on-screen diagnostics stale until the next edit
        // (#281). Mirror the watched-files path: nudge pull clients to re-pull
        // (and refresh project-derived views), so a settings change — e.g.
        // newly ignoring a code — takes effect immediately.
        self.publish_project_diagnostics().await;
    }

    /// Fallback project discovery (#73). `initialize` loads the project from the
    /// client's `rootUri`/workspace folder, but some clients (or certain
    /// single-file open flows) never send one, leaving the store empty — so
    /// hover/definition/rename silently degrade. When a `.m1scr` is opened and no
    /// project is loaded yet, walk up from that file to find `Project.m1prj` and
    /// load it. A no-op once a project is loaded, and harmless when none exists.
    pub(super) async fn ensure_project_loaded(&self, uri: &Url) {
        if self.store.project_loaded() {
            return;
        }
        let Some(dir) = uri
            .to_file_path()
            .ok()
            .and_then(|p| p.parent().map(|d| d.to_path_buf()))
        else {
            return;
        };
        match self.store.discover_and_load(&dir) {
            Ok(true) => {
                self.client
                    .log_message(
                        MessageType::INFO,
                        "m1-lsp: project loaded (didOpen fallback)",
                    )
                    .await;
                // Resolve the unified config now that we have a workspace root.
                self.apply_config(&dir);
                // Surface the project-scope audit for the just-loaded project (#139).
                self.publish_project_diagnostics().await;
            }
            Ok(false) => { /* no project found from this file; stay project-less */ }
            Err(e) => {
                self.client
                    .log_message(
                        MessageType::WARNING,
                        format!("m1-lsp: project load failed (didOpen fallback): {e}"),
                    )
                    .await;
            }
        }
    }
}

#[cfg(test)]
mod did_close_tests {
    use crate::backend::Backend;
    use tower_lsp::{LanguageServer, LspService, lsp_types::*};

    // Regression guard: `did_close` must remove the closed document's entry from
    // `diag_prev`. Before the fix the handler cleared `docs` and `semtok_prev` but
    // left `diag_prev` alone, leaking a stale cache entry on every close. Each
    // close/reopen cycle accumulated an orphaned entry that was never reclaimed.
    //
    // The handler calls `publish_diagnostics` which hits the client socket; that
    // path uses `block_in_place` internally, so the test requires the multi-thread
    // runtime.
    #[tokio::test(flavor = "multi_thread")]
    async fn did_close_removes_diag_prev_entry() {
        let (service, _socket) = LspService::new(Backend::new);
        let backend = service.inner();

        let uri = Url::parse("file:///test.m1scr").unwrap();

        // Seed diag_prev as if a pull-diagnostic poll had previously run.
        backend
            .diag_prev
            .insert(uri.clone(), ("result-1".to_owned(), vec![]));
        assert!(
            backend.diag_prev.contains_key(&uri),
            "precondition: diag_prev must contain the URI before close"
        );

        backend
            .did_close(DidCloseTextDocumentParams {
                text_document: TextDocumentIdentifier { uri: uri.clone() },
            })
            .await;

        assert!(
            !backend.diag_prev.contains_key(&uri),
            "did_close must remove the URI from diag_prev to prevent a cache leak"
        );
    }
}
