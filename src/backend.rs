//! The LSP backend: document lifecycle, diagnostics publishing, formatting.
use dashmap::DashMap;
use tower_lsp::jsonrpc::Result;
use tower_lsp::lsp_types::*;
use tower_lsp::{Client, LanguageServer};

use crate::analysis::{analyze, LintProvider, NoLint, NoTypes, TypeProvider};
use crate::document::Document;
use crate::format::{format_edits, Formatter, NoFormat};
use crate::line_index::PositionEncoding;

pub struct Backend {
    client: Client,
    docs: DashMap<Url, Document>,
    encoding: std::sync::RwLock<PositionEncoding>,
    lint: Box<dyn LintProvider>,
    types: Box<dyn TypeProvider>,
    formatter: Box<dyn Formatter>,
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
        }
    }

    /// Constructor used in Tasks 7-8 / tests to inject real backends.
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
                ..Default::default()
            },
        })
    }

    async fn initialized(&self, _: InitializedParams) {
        self.client
            .log_message(MessageType::INFO, "m1-lsp ready")
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

    async fn shutdown(&self) -> Result<()> {
        Ok(())
    }
}
