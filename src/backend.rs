//! The LSP backend: document lifecycle, diagnostics publishing, formatting.
use std::sync::Arc;

use dashmap::DashMap;
use tower_lsp::jsonrpc::{Error, Result};
use tower_lsp::lsp_types::request::{GotoImplementationParams, GotoImplementationResponse};
use tower_lsp::lsp_types::*;
use tower_lsp::{Client, LanguageServer};

use crate::analysis::{LintProvider, NoLint, NoTypes, TypeProvider, analyze};
use crate::config::M1Config;
use crate::document::Document;
use crate::features::{
    call_hierarchy, code_action, code_lens, completion, document_symbols, folding, goto, hover,
    inlay, references, rename, semantic_tokens, signature_help, workspace_symbol,
};
use crate::format::{Formatter, NoFormat, format_edits, range_format_edits};
use crate::line_index::PositionEncoding;
use crate::project_store::ProjectStore;

pub struct Backend {
    client: Client,
    docs: DashMap<Url, Document>,
    encoding: std::sync::RwLock<PositionEncoding>,
    lint: Box<dyn LintProvider>,
    types: Box<dyn TypeProvider>,
    formatter: Box<dyn Formatter>,
    store: Arc<ProjectStore>,
    /// Whether the client supports dynamic registration of
    /// `workspace/didChangeWatchedFiles` (set during `initialize`).
    watch_dynamic: std::sync::atomic::AtomicBool,
    /// The resolved unified config (lint/format/diagnostics) currently applied to
    /// the backends. Re-resolved on root discovery, `m1-tools.toml` change, and
    /// `didChangeConfiguration`; its `diagnostics` filter is read on every publish.
    config: std::sync::RwLock<M1Config>,
    /// The last editor settings (`initializationOptions` / `didChangeConfiguration`),
    /// the middle precedence layer beneath `m1-tools.toml`.
    editor_settings: std::sync::RwLock<Option<serde_json::Value>>,
    /// The project root last used to resolve config, so `didChangeConfiguration`
    /// can re-resolve against the same workspace.
    config_root: std::sync::RwLock<Option<std::path::PathBuf>>,
}

impl Backend {
    pub fn new(client: Client) -> Self {
        Self {
            client,
            docs: DashMap::new(),
            encoding: std::sync::RwLock::new(PositionEncoding::Utf16),
            lint: Box::new(NoLint),
            types: Box::new(NoTypes),
            formatter: Box::new(NoFormat),
            store: Arc::new(ProjectStore::new()),
            watch_dynamic: std::sync::atomic::AtomicBool::new(false),
            config: std::sync::RwLock::new(M1Config::default()),
            editor_settings: std::sync::RwLock::new(None),
            config_root: std::sync::RwLock::new(None),
        }
    }

    /// Inject lint, type provider, formatter, and a shared project store (the
    /// same `Arc` the type provider holds, so reloads are visible to both
    /// diagnostics and the read features).
    pub fn with_backends(
        client: Client,
        lint: Box<dyn LintProvider>,
        types: Box<dyn TypeProvider>,
        formatter: Box<dyn Formatter>,
        store: Arc<ProjectStore>,
    ) -> Self {
        Self {
            client,
            docs: DashMap::new(),
            encoding: std::sync::RwLock::new(PositionEncoding::Utf16),
            lint,
            types,
            formatter,
            store,
            watch_dynamic: std::sync::atomic::AtomicBool::new(false),
            config: std::sync::RwLock::new(M1Config::default()),
            editor_settings: std::sync::RwLock::new(None),
            config_root: std::sync::RwLock::new(None),
        }
    }

    /// Resolve the unified config for `root` (editor settings layered under any
    /// `m1-tools.toml`) and apply it: lint thresholds/rules, formatter options,
    /// and the cross-source diagnostic filter. Records `root` so a later
    /// `didChangeConfiguration` can re-resolve against the same workspace.
    fn apply_config(&self, root: &std::path::Path) {
        let editor = self.editor_settings.read().unwrap().clone();
        let cfg = M1Config::resolve(editor.as_ref(), root);
        self.lint.set_lint_config(&cfg.lint);
        self.formatter.set_format_options(&cfg.format);
        *self.config.write().unwrap() = cfg;
        *self.config_root.write().unwrap() = Some(root.to_path_buf());
    }

    /// Re-resolve config against the last known root (used by
    /// `didChangeConfiguration`, which carries new editor settings but no root).
    fn reapply_config(&self) {
        let root = self.config_root.read().unwrap().clone();
        if let Some(root) = root {
            self.apply_config(&root);
        }
    }

    fn enc(&self) -> PositionEncoding {
        *self.encoding.read().unwrap()
    }

    /// Fallback project discovery (#73). `initialize` loads the project from the
    /// client's `rootUri`/workspace folder, but some clients (or certain
    /// single-file open flows) never send one, leaving the store empty — so
    /// hover/definition/rename silently degrade. When a `.m1scr` is opened and no
    /// project is loaded yet, walk up from that file to find `Project.m1prj` and
    /// load it. A no-op once a project is loaded, and harmless when none exists.
    async fn ensure_project_loaded(&self, uri: &Url) {
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

    /// Refresh the in-memory project model after a rename that rewrote
    /// `Project.m1prj`. The client applies the workspace edit to a buffer it may
    /// never save (and never tells us via file-watching), so the cached symbol
    /// table would otherwise keep the old name — making the just-renamed symbol
    /// read as undefined until the server restarts. We derive the post-rename
    /// `.m1prj` text from the edit we just computed, reload from it, and
    /// re-publish so diagnostics reflect the new name immediately.
    async fn refresh_after_rename(&self, edit: &WorkspaceEdit) {
        let Some(prj_path) = self
            .store
            .with_project(|p| p.map(|lp| lp.m1prj_path.clone()))
        else {
            return;
        };
        let Ok(prj_uri) = Url::from_file_path(&prj_path) else {
            return;
        };
        let orig = self
            .docs
            .get(&prj_uri)
            .map(|d| d.text.clone())
            .or_else(|| crate::disk_read::read_disk(&prj_path));
        let Some(orig) = orig else {
            return;
        };
        let Some(new_text) = rename::apply_workspace_edit_to(edit, &prj_uri, &orig, self.enc())
        else {
            // The rename didn't touch the project file (e.g. a local-only rename).
            return;
        };
        if let Err(e) = self.store.reload_from_m1prj_text(&new_text) {
            self.client
                .log_message(
                    MessageType::WARNING,
                    format!("m1-lsp: post-rename project refresh failed: {e}"),
                )
                .await;
            return;
        }
        let uris: Vec<Url> = self.docs.iter().map(|e| e.key().clone()).collect();
        for uri in uris {
            self.publish(uri).await;
        }
    }

    async fn publish(&self, uri: Url) {
        // Snapshot the doc and drop the shard guard before parsing, so a
        // concurrent did_change on the same shard isn't blocked for the parse.
        let Some((text, lindex, version)) = self
            .docs
            .get(&uri)
            .map(|d| (d.text.clone(), d.line_index.clone(), d.version))
        else {
            return;
        };
        // The `.m1prj` is XML, not an M1 script — don't run the script analysis
        // on it (it would emit bogus syntax diagnostics). It can still be opened
        // as a document so a channel/parameter can be renamed from its
        // declaration; just publish no diagnostics for it.
        if is_m1prj(&uri) {
            self.client
                .publish_diagnostics(uri, vec![], Some(version))
                .await;
            return;
        }
        let diags = analyze(
            &uri,
            &text,
            &lindex,
            self.enc(),
            self.lint.as_ref(),
            self.types.as_ref(),
            &self.config.read().unwrap().diagnostics,
        );
        self.client
            .publish_diagnostics(uri, diags, Some(version))
            .await;
    }
}

/// True when `uri` points at a `Project.m1prj` (or any `.m1prj`) project file.
fn is_m1prj(uri: &Url) -> bool {
    uri.path().ends_with(".m1prj")
}

/// Extract the M1 settings object from a client `initializationOptions` /
/// `didChangeConfiguration` payload: the `settings` sub-object if present (the
/// shape the extensions send), else the value itself (a bare
/// `{ lint, format, diagnostics }`). The result is deserialized by
/// [`crate::config::M1Config::resolve`].
fn editor_settings(v: serde_json::Value) -> serde_json::Value {
    v.get("settings").cloned().unwrap_or(v)
}

#[tower_lsp::async_trait]
impl LanguageServer for Backend {
    async fn initialize(&self, params: InitializeParams) -> Result<InitializeResult> {
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
            capabilities: ServerCapabilities {
                position_encoding: Some(chosen.1),
                text_document_sync: Some(TextDocumentSyncCapability::Kind(
                    TextDocumentSyncKind::FULL,
                )),
                document_formatting_provider: Some(OneOf::Left(true)),
                document_range_formatting_provider: Some(OneOf::Left(true)),
                workspace_symbol_provider: Some(OneOf::Left(true)),
                hover_provider: Some(HoverProviderCapability::Simple(true)),
                definition_provider: Some(OneOf::Left(true)),
                implementation_provider: Some(ImplementationProviderCapability::Simple(true)),
                references_provider: Some(OneOf::Left(true)),
                document_highlight_provider: Some(OneOf::Left(true)),
                folding_range_provider: Some(FoldingRangeProviderCapability::Simple(true)),
                code_action_provider: Some(CodeActionProviderCapability::Simple(true)),
                document_symbol_provider: Some(OneOf::Left(true)),
                completion_provider: Some(CompletionOptions {
                    trigger_characters: Some(vec![".".into()]),
                    ..Default::default()
                }),
                signature_help_provider: Some(SignatureHelpOptions {
                    trigger_characters: Some(vec!["(".into(), ",".into()]),
                    retrigger_characters: None,
                    work_done_progress_options: Default::default(),
                }),
                inlay_hint_provider: Some(OneOf::Left(true)),
                code_lens_provider: Some(CodeLensOptions {
                    resolve_provider: Some(false),
                }),
                call_hierarchy_provider: Some(CallHierarchyServerCapability::Simple(true)),
                rename_provider: Some(OneOf::Right(RenameOptions {
                    prepare_provider: Some(true),
                    work_done_progress_options: Default::default(),
                })),
                semantic_tokens_provider: Some(
                    SemanticTokensServerCapabilities::SemanticTokensOptions(
                        SemanticTokensOptions {
                            legend: semantic_tokens::legend(),
                            full: Some(SemanticTokensFullOptions::Bool(true)),
                            range: Some(true),
                            work_done_progress_options: Default::default(),
                        },
                    ),
                ),
                ..Default::default()
            },
        })
    }

    async fn initialized(&self, _: InitializedParams) {
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
                FileSystemWatcher {
                    glob_pattern: GlobPattern::String("**/.m1lint.toml".into()),
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
            let reg = Registration {
                id: "m1-lsp-watch-project".into(),
                method: "workspace/didChangeWatchedFiles".into(),
                register_options: Some(
                    serde_json::to_value(DidChangeWatchedFilesRegistrationOptions { watchers })
                        .unwrap(),
                ),
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
    }

    async fn did_open(&self, params: DidOpenTextDocumentParams) {
        let d = params.text_document;
        self.docs
            .insert(d.uri.clone(), Document::new(d.text, d.version));
        // Some clients open a file without ever sending a `rootUri`/workspace
        // folder at `initialize`, leaving the server project-less. Fall back to
        // discovering the project from the opened file itself (#73).
        self.ensure_project_loaded(&d.uri).await;
        self.publish(d.uri).await;
    }

    async fn did_change(&self, params: DidChangeTextDocumentParams) {
        // FULL sync: the last content change holds the entire new text.
        if let Some(change) = params.content_changes.into_iter().last() {
            let uri = params.text_document.uri;
            self.docs.insert(
                uri.clone(),
                Document::new(change.text, params.text_document.version),
            );
            self.publish(uri).await;
        }
    }

    async fn did_save(&self, params: DidSaveTextDocumentParams) {
        self.publish(params.text_document.uri).await;
    }

    async fn did_close(&self, params: DidCloseTextDocumentParams) {
        let uri = params.text_document.uri;
        self.docs.remove(&uri);
        self.client.publish_diagnostics(uri, vec![], None).await;
    }

    async fn formatting(&self, params: DocumentFormattingParams) -> Result<Option<Vec<TextEdit>>> {
        let uri = params.text_document.uri;
        Ok(self
            .docs
            .get(&uri)
            .and_then(|doc| format_edits(&doc, self.enc(), self.formatter.as_ref())))
    }

    async fn range_formatting(
        &self,
        params: DocumentRangeFormattingParams,
    ) -> Result<Option<Vec<TextEdit>>> {
        let uri = params.text_document.uri;
        Ok(self
            .docs
            .get(&uri)
            .and_then(|doc| range_format_edits(&doc, params.range, self.formatter.as_ref())))
    }

    #[allow(deprecated)]
    async fn symbol(
        &self,
        params: WorkspaceSymbolParams,
    ) -> Result<Option<Vec<SymbolInformation>>> {
        let q = params.query;
        Ok(self
            .store
            .with_project(|p| p.map(|lp| workspace_symbol::workspace_symbols(lp, &q))))
    }

    async fn hover(&self, params: HoverParams) -> Result<Option<Hover>> {
        let tdp = params.text_document_position_params;
        let uri = tdp.text_document.uri;
        let Some((text, lindex)) = self
            .docs
            .get(&uri)
            .map(|d| (d.text.clone(), d.line_index.clone()))
        else {
            return Ok(None);
        };
        let byte = lindex.offset(tdp.position, &text, self.enc());
        let cst = m1_core::parse(&text);
        let file_name = uri
            .to_file_path()
            .ok()
            .and_then(|p| p.file_name().map(|s| s.to_string_lossy().into_owned()));
        let li = &lindex;
        let enc = self.enc();
        Ok(self.store.with_project(|p| {
            hover::hover(
                cst.root(),
                byte,
                p.map(|lp| &lp.project),
                file_name.as_deref(),
                li,
                enc,
            )
        }))
    }

    async fn goto_definition(
        &self,
        params: GotoDefinitionParams,
    ) -> Result<Option<GotoDefinitionResponse>> {
        let tdp = params.text_document_position_params;
        let uri = tdp.text_document.uri;
        let Some((text, lindex)) = self
            .docs
            .get(&uri)
            .map(|d| (d.text.clone(), d.line_index.clone()))
        else {
            return Ok(None);
        };
        let byte = lindex.offset(tdp.position, &text, self.enc());
        let cst = m1_core::parse(&text);
        let file_name = uri
            .to_file_path()
            .ok()
            .and_then(|p| p.file_name().map(|s| s.to_string_lossy().into_owned()));
        Ok(self.store.with_project(|p| {
            p.and_then(|lp| goto::goto(cst.root(), byte, lp, file_name.as_deref()))
                .map(GotoDefinitionResponse::Scalar)
        }))
    }

    async fn goto_implementation(
        &self,
        params: GotoImplementationParams,
    ) -> Result<Option<GotoImplementationResponse>> {
        let tdp = params.text_document_position_params;
        let uri = tdp.text_document.uri;
        let Some((text, lindex)) = self
            .docs
            .get(&uri)
            .map(|d| (d.text.clone(), d.line_index.clone()))
        else {
            return Ok(None);
        };
        let byte = lindex.offset(tdp.position, &text, self.enc());
        let enc = self.enc();
        // "Implementation" of a channel = where it is written (produced). With a
        // project loaded, search every `.m1scr`; open buffers win over disk.
        let open_text = |u: &Url| self.docs.get(u).map(|d| d.text.clone());
        // Snapshot the script-path list, drop the project RwLock guard, then run
        // the read+parse-every-script loop off the async worker (#135).
        let script_files = self
            .store
            .with_project(|p| p.map(|lp| lp.script_files.clone()));
        let locs = script_files.and_then(|scripts| {
            tokio::task::block_in_place(|| {
                references::project_implementations(&scripts, &uri, &text, byte, enc, &open_text)
            })
        });
        Ok(locs.map(GotoDefinitionResponse::Array))
    }

    async fn document_symbol(
        &self,
        params: DocumentSymbolParams,
    ) -> Result<Option<DocumentSymbolResponse>> {
        let uri = params.text_document.uri;
        let Some((text, lindex)) = self
            .docs
            .get(&uri)
            .map(|d| (d.text.clone(), d.line_index.clone()))
        else {
            return Ok(None);
        };
        let cst = m1_core::parse(&text);
        let syms = document_symbols::document_symbols(cst.root(), &lindex, self.enc());
        Ok(Some(DocumentSymbolResponse::Nested(syms)))
    }

    async fn completion(&self, params: CompletionParams) -> Result<Option<CompletionResponse>> {
        let tdp = params.text_document_position;
        let uri = tdp.text_document.uri;
        let Some((text, lindex)) = self
            .docs
            .get(&uri)
            .map(|d| (d.text.clone(), d.line_index.clone()))
        else {
            return Ok(None);
        };
        let enc = self.enc();
        let byte = lindex.offset(tdp.position, &text, enc);
        let cst = m1_core::parse(&text);
        let file_name = uri
            .to_file_path()
            .ok()
            .and_then(|p| p.file_name().map(|s| s.to_string_lossy().into_owned()));
        let items = self.store.with_project(|p| {
            completion::completions(
                cst.root(),
                p,
                file_name.as_deref(),
                &text,
                byte,
                &lindex,
                enc,
            )
        });
        Ok(Some(CompletionResponse::Array(items)))
    }

    async fn signature_help(&self, params: SignatureHelpParams) -> Result<Option<SignatureHelp>> {
        let tdp = params.text_document_position_params;
        let uri = tdp.text_document.uri;
        let Some((text, lindex)) = self
            .docs
            .get(&uri)
            .map(|d| (d.text.clone(), d.line_index.clone()))
        else {
            return Ok(None);
        };
        let byte = lindex.offset(tdp.position, &text, self.enc());
        let cst = m1_core::parse(&text);
        let file_name = uri
            .to_file_path()
            .ok()
            .and_then(|p| p.file_name().map(|s| s.to_string_lossy().into_owned()));
        Ok(self.store.with_project(|p| {
            signature_help::signature_help(
                cst.root(),
                byte,
                p.map(|lp| &lp.project),
                file_name.as_deref(),
            )
        }))
    }

    async fn inlay_hint(&self, params: InlayHintParams) -> Result<Option<Vec<InlayHint>>> {
        let uri = params.text_document.uri;
        let Some((text, lindex)) = self
            .docs
            .get(&uri)
            .map(|d| (d.text.clone(), d.line_index.clone()))
        else {
            return Ok(None);
        };
        let cst = m1_core::parse(&text);
        let hints = inlay::inlay_hints(cst.root(), params.range, &lindex, self.enc());
        Ok(Some(hints))
    }

    async fn code_lens(&self, params: CodeLensParams) -> Result<Option<Vec<CodeLens>>> {
        let uri = params.text_document.uri;
        Ok(self
            .store
            .with_project(|p| p.map(|lp| code_lens::code_lens(lp, &uri))))
    }

    async fn prepare_call_hierarchy(
        &self,
        params: CallHierarchyPrepareParams,
    ) -> Result<Option<Vec<CallHierarchyItem>>> {
        let uri = params.text_document_position_params.text_document.uri;
        let Some((text, lindex)) = self
            .docs
            .get(&uri)
            .map(|d| (d.text.clone(), d.line_index.clone()))
        else {
            return Ok(None);
        };
        let byte = lindex.offset(
            params.text_document_position_params.position,
            &text,
            self.enc(),
        );
        let enc = self.enc();
        let open_text = |u: &Url| self.docs.get(u).map(|d| d.text.clone());
        // Reads + parses every script under the live project; run off the async
        // worker via `block_in_place` (#135).
        Ok(tokio::task::block_in_place(|| {
            self.store.with_project(|p| {
                p.and_then(|lp| call_hierarchy::prepare(lp, &uri, &text, byte, enc, &open_text))
            })
        }))
    }

    async fn incoming_calls(
        &self,
        params: CallHierarchyIncomingCallsParams,
    ) -> Result<Option<Vec<CallHierarchyIncomingCall>>> {
        let enc = self.enc();
        let open_text = |u: &Url| self.docs.get(u).map(|d| d.text.clone());
        // Reads + parses every script under the live project; run off the async
        // worker via `block_in_place` (#135).
        Ok(tokio::task::block_in_place(|| {
            self.store.with_project(|p| {
                p.and_then(|lp| call_hierarchy::incoming(lp, &params.item, enc, &open_text))
            })
        }))
    }

    async fn outgoing_calls(
        &self,
        params: CallHierarchyOutgoingCallsParams,
    ) -> Result<Option<Vec<CallHierarchyOutgoingCall>>> {
        let enc = self.enc();
        let open_text = |u: &Url| self.docs.get(u).map(|d| d.text.clone());
        // Reads + parses every script under the live project; run off the async
        // worker via `block_in_place` (#135).
        Ok(tokio::task::block_in_place(|| {
            self.store.with_project(|p| {
                p.and_then(|lp| call_hierarchy::outgoing(lp, &params.item, enc, &open_text))
            })
        }))
    }

    async fn prepare_rename(
        &self,
        params: TextDocumentPositionParams,
    ) -> Result<Option<PrepareRenameResponse>> {
        let uri = params.text_document.uri;
        let Some((text, lindex)) = self
            .docs
            .get(&uri)
            .map(|d| (d.text.clone(), d.line_index.clone()))
        else {
            return Ok(None);
        };
        let byte = lindex.offset(params.position, &text, self.enc());
        let enc = self.enc();
        // The `.m1prj` is XML, not a script: offer rename on a component's Name.
        if is_m1prj(&uri) {
            return Ok(self.store.with_project(|p| {
                rename::prepare_m1prj(&text, byte, enc, p.map(|lp| &lp.project))
            }));
        }
        let cst = m1_core::parse(&text);
        let file_name = uri
            .to_file_path()
            .ok()
            .and_then(|p| p.file_name().map(|s| s.to_string_lossy().into_owned()));
        Ok(self.store.with_project(|p| {
            rename::prepare(
                cst.root(),
                byte,
                &lindex,
                enc,
                p.map(|lp| &lp.project),
                file_name.as_deref(),
            )
        }))
    }

    async fn rename(&self, params: RenameParams) -> Result<Option<WorkspaceEdit>> {
        let new_name = params.new_name;
        let tdp = params.text_document_position;
        let uri = tdp.text_document.uri;
        let Some((text, lindex)) = self
            .docs
            .get(&uri)
            .map(|d| (d.text.clone(), d.line_index.clone()))
        else {
            return Ok(None);
        };
        let byte = lindex.offset(tdp.position, &text, self.enc());
        let enc = self.enc();
        // Open buffers win over on-disk copies so an in-flight edit is seen.
        let open_text = |u: &Url| self.docs.get(u).map(|d| d.text.clone());
        // A project-wide rename reads + parses every script. Those functions
        // borrow the live project for the duration, so we keep the RwLock guard
        // around the call but run it under `block_in_place` so the blocking
        // read+parse doesn't stall an async worker (#135).
        let result = if is_m1prj(&uri) {
            tokio::task::block_in_place(|| {
                self.store.with_project(|p| match p {
                    Some(lp) => rename::execute_m1prj(
                        &text,
                        byte,
                        &new_name,
                        uri.clone(),
                        enc,
                        lp,
                        &open_text,
                    ),
                    None => Err("no project is loaded".to_string()),
                })
            })
        } else {
            let cst = m1_core::parse(&text);
            let file_name = uri
                .to_file_path()
                .ok()
                .and_then(|p| p.file_name().map(|s| s.to_string_lossy().into_owned()));
            tokio::task::block_in_place(|| {
                self.store.with_project(|p| {
                    rename::execute(
                        cst.root(),
                        byte,
                        &new_name,
                        uri.clone(),
                        &lindex,
                        enc,
                        p,
                        file_name.as_deref(),
                        &open_text,
                    )
                })
            })
        };
        // An Err is surfaced to the user (Ok(None) would make the client
        // silently do nothing); a successful edit may span several files.
        match result {
            Ok(edit) => {
                // Refresh the project model from the edit so the renamed symbol is
                // live immediately, without waiting for a client file-watch event.
                self.refresh_after_rename(&edit).await;
                Ok(Some(edit))
            }
            Err(e) => Err(Error::invalid_params(e)),
        }
    }

    async fn semantic_tokens_full(
        &self,
        params: SemanticTokensParams,
    ) -> Result<Option<SemanticTokensResult>> {
        let uri = params.text_document.uri;
        let Some((text, lindex)) = self
            .docs
            .get(&uri)
            .map(|d| (d.text.clone(), d.line_index.clone()))
        else {
            return Ok(None);
        };
        let cst = m1_core::parse(&text);
        let file_name = uri
            .to_file_path()
            .ok()
            .and_then(|p| p.file_name().map(|s| s.to_string_lossy().into_owned()));
        let li = &lindex;
        let enc = self.enc();
        let tokens = self.store.with_project(|p| {
            semantic_tokens::semantic_tokens(
                cst.root(),
                p.map(|lp| &lp.project),
                file_name.as_deref(),
                li,
                enc,
            )
        });
        Ok(Some(SemanticTokensResult::Tokens(SemanticTokens {
            result_id: None,
            data: tokens,
        })))
    }

    async fn semantic_tokens_range(
        &self,
        params: SemanticTokensRangeParams,
    ) -> Result<Option<SemanticTokensRangeResult>> {
        let uri = params.text_document.uri;
        let Some((text, lindex)) = self
            .docs
            .get(&uri)
            .map(|d| (d.text.clone(), d.line_index.clone()))
        else {
            return Ok(None);
        };
        let cst = m1_core::parse(&text);
        let file_name = uri
            .to_file_path()
            .ok()
            .and_then(|p| p.file_name().map(|s| s.to_string_lossy().into_owned()));
        let li = &lindex;
        let enc = self.enc();
        let tokens = self.store.with_project(|p| {
            semantic_tokens::semantic_tokens_range(
                cst.root(),
                p.map(|lp| &lp.project),
                file_name.as_deref(),
                li,
                enc,
                params.range.start.line,
                params.range.end.line,
            )
        });
        Ok(Some(SemanticTokensRangeResult::Tokens(SemanticTokens {
            result_id: None,
            data: tokens,
        })))
    }

    async fn references(&self, params: ReferenceParams) -> Result<Option<Vec<Location>>> {
        let tdp = params.text_document_position;
        let uri = tdp.text_document.uri;
        let Some((text, lindex)) = self
            .docs
            .get(&uri)
            .map(|d| (d.text.clone(), d.line_index.clone()))
        else {
            return Ok(None);
        };
        let byte = lindex.offset(tdp.position, &text, self.enc());
        let enc = self.enc();
        // With a project loaded, search every `.m1scr` for a project symbol
        // (#29); locals stay file-local. Open buffers win over on-disk text.
        let open_text = |u: &Url| self.docs.get(u).map(|d| d.text.clone());
        // Snapshot the small script-path list, then drop the project RwLock guard
        // before the read+parse-every-script loop, and run that blocking work via
        // `block_in_place` so it doesn't stall an async worker (#135).
        let script_files = self
            .store
            .with_project(|p| p.map(|lp| lp.script_files.clone()));
        if let Some(scripts) = script_files {
            let locs = tokio::task::block_in_place(|| {
                references::project_references(&scripts, &uri, &text, byte, enc, &open_text)
            });
            return Ok(locs);
        }
        // Project-less mode: single-file references.
        let cst = m1_core::parse(&text);
        Ok(references::references(
            cst.root(),
            byte,
            uri.clone(),
            &lindex,
            enc,
        ))
    }

    async fn document_highlight(
        &self,
        params: DocumentHighlightParams,
    ) -> Result<Option<Vec<DocumentHighlight>>> {
        let tdp = params.text_document_position_params;
        let uri = tdp.text_document.uri;
        let Some((text, lindex)) = self
            .docs
            .get(&uri)
            .map(|d| (d.text.clone(), d.line_index.clone()))
        else {
            return Ok(None);
        };
        let byte = lindex.offset(tdp.position, &text, self.enc());
        let cst = m1_core::parse(&text);
        Ok(references::document_highlights(
            cst.root(),
            byte,
            &lindex,
            self.enc(),
        ))
    }

    async fn folding_range(&self, params: FoldingRangeParams) -> Result<Option<Vec<FoldingRange>>> {
        let uri = params.text_document.uri;
        let Some((text, lindex)) = self
            .docs
            .get(&uri)
            .map(|d| (d.text.clone(), d.line_index.clone()))
        else {
            return Ok(None);
        };
        let cst = m1_core::parse(&text);
        Ok(Some(folding::folding_ranges(
            cst.root(),
            &lindex,
            self.enc(),
        )))
    }

    async fn code_action(&self, params: CodeActionParams) -> Result<Option<CodeActionResponse>> {
        let uri = params.text_document.uri;
        let Some((text, lindex)) = self
            .docs
            .get(&uri)
            .map(|d| (d.text.clone(), d.line_index.clone()))
        else {
            return Ok(None);
        };
        let actions = code_action::code_actions(
            &text,
            &lindex,
            self.enc(),
            &uri,
            &params.context.diagnostics,
        );
        Ok((!actions.is_empty()).then_some(actions))
    }

    async fn did_change_watched_files(&self, params: DidChangeWatchedFilesParams) {
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
            matches!(name, ".m1lint.toml" | "m1-tools.toml").then_some(p)
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
        if !touches_project && config_change.is_none() {
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
    }

    async fn did_change_configuration(&self, params: DidChangeConfigurationParams) {
        // New editor settings (the middle config layer). Re-resolve against the
        // current workspace root and re-publish so the change takes effect live.
        *self.editor_settings.write().unwrap() = Some(editor_settings(params.settings));
        self.reapply_config();
        let uris: Vec<Url> = self.docs.iter().map(|e| e.key().clone()).collect();
        for uri in uris {
            self.publish(uri).await;
        }
    }

    async fn shutdown(&self) -> Result<()> {
        Ok(())
    }
}
