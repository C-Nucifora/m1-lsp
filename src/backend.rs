//! The LSP backend: document lifecycle, diagnostics publishing, formatting.
use std::sync::Arc;

use dashmap::DashMap;
use tower_lsp::jsonrpc::{Error, Result};
use tower_lsp::lsp_types::*;
use tower_lsp::{Client, LanguageServer};

use crate::analysis::{LintProvider, NoLint, NoTypes, TypeProvider, analyze};
use crate::document::Document;
use crate::features::{completion, document_symbols, goto, hover, inlay, rename, semantic_tokens};
use crate::format::{Formatter, NoFormat, format_edits};
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
        }
    }

    /// v1 constructor (lint + formatter); defaults the type provider to NoTypes
    /// and a fresh project store. Kept for back-compat.
    pub fn with_backends(
        client: Client,
        lint: Box<dyn LintProvider>,
        formatter: Box<dyn Formatter>,
    ) -> Self {
        Self {
            client,
            docs: DashMap::new(),
            encoding: std::sync::RwLock::new(PositionEncoding::Utf16),
            lint,
            types: Box::new(NoTypes),
            formatter,
            store: Arc::new(ProjectStore::new()),
        }
    }

    /// v2 constructor: inject lint, type provider, formatter, and a shared
    /// project store (the same `Arc` the type provider holds, so reloads are
    /// visible to both diagnostics and the read features).
    pub fn with_backends_v2(
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
        }
    }

    fn enc(&self) -> PositionEncoding {
        *self.encoding.read().unwrap()
    }

    async fn publish(&self, uri: Url) {
        if let Some(doc) = self.docs.get(&uri) {
            let diags = analyze(
                &uri,
                &doc.text,
                &doc.line_index,
                self.enc(),
                self.lint.as_ref(),
                self.types.as_ref(),
            );
            let version = Some(doc.version);
            drop(doc);
            self.client.publish_diagnostics(uri, diags, version).await;
        }
    }
}

#[tower_lsp::async_trait]
impl LanguageServer for Backend {
    async fn initialize(&self, params: InitializeParams) -> Result<InitializeResult> {
        // Negotiate position encoding: prefer UTF-16, accept UTF-8 if offered.
        let chosen = params
            .capabilities
            .general
            .and_then(|g| g.position_encodings)
            .map(|encs| {
                if encs.contains(&PositionEncodingKind::UTF8) {
                    (PositionEncoding::Utf8, PositionEncodingKind::UTF8)
                } else {
                    (PositionEncoding::Utf16, PositionEncodingKind::UTF16)
                }
            })
            .unwrap_or((PositionEncoding::Utf16, PositionEncodingKind::UTF16));
        *self.encoding.write().unwrap() = chosen.0;

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
                hover_provider: Some(HoverProviderCapability::Simple(true)),
                definition_provider: Some(OneOf::Left(true)),
                document_symbol_provider: Some(OneOf::Left(true)),
                completion_provider: Some(CompletionOptions::default()),
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
        let watchers = vec![
            FileSystemWatcher {
                glob_pattern: GlobPattern::String("**/*.m1prj".into()),
                kind: None,
            },
            FileSystemWatcher {
                glob_pattern: GlobPattern::String("**/*.m1cfg".into()),
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

    async fn hover(&self, params: HoverParams) -> Result<Option<Hover>> {
        let tdp = params.text_document_position_params;
        let uri = tdp.text_document.uri;
        let Some(doc) = self.docs.get(&uri) else {
            return Ok(None);
        };
        let byte = doc.line_index.offset(tdp.position, &doc.text, self.enc());
        let cst = m1_core::parse(&doc.text);
        let file_name = uri
            .to_file_path()
            .ok()
            .and_then(|p| p.file_name().map(|s| s.to_string_lossy().into_owned()));
        let li = &doc.line_index;
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
        let Some(doc) = self.docs.get(&uri) else {
            return Ok(None);
        };
        let byte = doc.line_index.offset(tdp.position, &doc.text, self.enc());
        let cst = m1_core::parse(&doc.text);
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
        let Some(doc) = self.docs.get(&uri) else {
            return Ok(None);
        };
        let cst = m1_core::parse(&doc.text);
        let syms = document_symbols::document_symbols(cst.root(), &doc.line_index, self.enc());
        Ok(Some(DocumentSymbolResponse::Nested(syms)))
    }

    async fn completion(&self, params: CompletionParams) -> Result<Option<CompletionResponse>> {
        let uri = params.text_document_position.text_document.uri;
        let Some(doc) = self.docs.get(&uri) else {
            return Ok(None);
        };
        let cst = m1_core::parse(&doc.text);
        let file_name = uri
            .to_file_path()
            .ok()
            .and_then(|p| p.file_name().map(|s| s.to_string_lossy().into_owned()));
        let items = self
            .store
            .with_project(|p| completion::completions(cst.root(), p, file_name.as_deref()));
        Ok(Some(CompletionResponse::Array(items)))
    }

    async fn inlay_hint(&self, params: InlayHintParams) -> Result<Option<Vec<InlayHint>>> {
        let uri = params.text_document.uri;
        let Some(doc) = self.docs.get(&uri) else {
            return Ok(None);
        };
        let cst = m1_core::parse(&doc.text);
        let hints = inlay::inlay_hints(cst.root(), params.range, &doc.line_index, self.enc());
        Ok(Some(hints))
    }

    async fn prepare_rename(
        &self,
        params: TextDocumentPositionParams,
    ) -> Result<Option<PrepareRenameResponse>> {
        let uri = params.text_document.uri;
        let Some(doc) = self.docs.get(&uri) else {
            return Ok(None);
        };
        let byte = doc
            .line_index
            .offset(params.position, &doc.text, self.enc());
        let cst = m1_core::parse(&doc.text);
        Ok(rename::prepare_rename(
            cst.root(),
            byte,
            &doc.line_index,
            self.enc(),
        ))
    }

    async fn rename(&self, params: RenameParams) -> Result<Option<WorkspaceEdit>> {
        let new_name = params.new_name;
        if !rename::is_valid_identifier(&new_name) {
            return Err(Error::invalid_params(format!(
                "'{new_name}' is not a valid M1 local name (letters, digits, underscore; no leading digit or spaces)"
            )));
        }
        let tdp = params.text_document_position;
        let uri = tdp.text_document.uri;
        let Some(doc) = self.docs.get(&uri) else {
            return Ok(None);
        };
        let byte = doc.line_index.offset(tdp.position, &doc.text, self.enc());
        let cst = m1_core::parse(&doc.text);
        Ok(rename::rename(
            cst.root(),
            byte,
            &new_name,
            uri.clone(),
            &doc.line_index,
            self.enc(),
        ))
    }

    async fn semantic_tokens_full(
        &self,
        params: SemanticTokensParams,
    ) -> Result<Option<SemanticTokensResult>> {
        let uri = params.text_document.uri;
        let Some(doc) = self.docs.get(&uri) else {
            return Ok(None);
        };
        let cst = m1_core::parse(&doc.text);
        let file_name = uri
            .to_file_path()
            .ok()
            .and_then(|p| p.file_name().map(|s| s.to_string_lossy().into_owned()));
        let li = &doc.line_index;
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
        if !touches_project {
            return;
        }
        // Reload from the known .m1prj path if any, else rediscover from a changed file's dir.
        let reloaded = self
            .store
            .with_project(|p| p.map(|lp| lp.m1prj_path.clone()));
        let result = match reloaded {
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
