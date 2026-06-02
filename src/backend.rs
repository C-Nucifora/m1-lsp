//! The LSP backend: document lifecycle, diagnostics publishing, formatting.
use std::sync::Arc;

use dashmap::DashMap;
use tower_lsp::jsonrpc::{Error, Result};
use tower_lsp::lsp_types::*;
use tower_lsp::{Client, LanguageServer};

use crate::analysis::{LintProvider, NoLint, NoTypes, TypeProvider, analyze};
use crate::document::Document;
use crate::features::{
    code_action, completion, document_symbols, folding, goto, hover, inlay, references, rename,
    semantic_tokens, signature_help, workspace_symbol,
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
        }
    }

    fn enc(&self) -> PositionEncoding {
        *self.encoding.read().unwrap()
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
            // Pick up a project-level (or user-global) `.m1lint.toml` (#9).
            self.lint.reload_config(&root);
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
                rename_provider: Some(OneOf::Right(RenameOptions {
                    prepare_provider: Some(true),
                    work_done_progress_options: Default::default(),
                })),
                semantic_tokens_provider: Some(
                    SemanticTokensServerCapabilities::SemanticTokensOptions(
                        SemanticTokensOptions {
                            legend: semantic_tokens::legend(),
                            full: Some(SemanticTokensFullOptions::Bool(true)),
                            range: None,
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
        let byte = lindex.offset(tdp.position, &text, self.enc());
        let cst = m1_core::parse(&text);
        let file_name = uri
            .to_file_path()
            .ok()
            .and_then(|p| p.file_name().map(|s| s.to_string_lossy().into_owned()));
        let items = self.store.with_project(|p| {
            completion::completions(cst.root(), p, file_name.as_deref(), &text, byte)
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
        // Renaming from within the project file (XML), not a script.
        if is_m1prj(&uri) {
            let result = self.store.with_project(|p| match p {
                Some(lp) => {
                    rename::execute_m1prj(&text, byte, &new_name, uri.clone(), enc, lp, &open_text)
                }
                None => Err("no project is loaded".to_string()),
            });
            return result.map(Some).map_err(Error::invalid_params);
        }
        let cst = m1_core::parse(&text);
        let file_name = uri
            .to_file_path()
            .ok()
            .and_then(|p| p.file_name().map(|s| s.to_string_lossy().into_owned()));
        let result = self.store.with_project(|p| {
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
        });
        // An Err is surfaced to the user (Ok(None) would make the client
        // silently do nothing); a successful edit may span several files.
        result.map(Some).map_err(Error::invalid_params)
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
        if let Some(locs) = self.store.with_project(|p| {
            p.map(|lp| references::project_references(lp, &uri, &text, byte, enc, &open_text))
        }) {
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
        // A `.m1lint.toml` change reloads the lint ruleset, rediscovered from
        // the file's directory (#9).
        let lint_change = params.changes.iter().find_map(|c| {
            let p = c.uri.to_file_path().ok()?;
            (p.file_name().and_then(|n| n.to_str()) == Some(".m1lint.toml")).then_some(p)
        });
        if let Some(p) = &lint_change
            && let Some(dir) = p.parent()
        {
            self.lint.reload_config(dir);
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
        if !touches_project && lint_change.is_none() {
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

    async fn shutdown(&self) -> Result<()> {
        Ok(())
    }
}
