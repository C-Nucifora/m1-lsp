//! The LSP backend: the `Backend` state and the `LanguageServer` protocol
//! surface.
//!
//! This module owns the shared state ([`Backend`]) and a *thin*
//! `impl LanguageServer` whose methods delegate to focused submodules, so the
//! protocol adapter changes independently of the query/rendering/state logic:
//!
//! - `context`   — per-request document plumbing (`DocContext`, encoding, goto)
//! - `capabilities` — server-capability declaration + client-capability probes
//! - `config`    — unified-config resolution/application
//! - `lifecycle` — init, document sync, watched-file / configuration reloads
//! - `diagnostics` — the diagnostics pipeline + push/pull handlers
//! - `navigation` — read-only navigation/query handlers
//! - `tokens`    — semantic-token handlers
//! - `editing`   — formatting / completion / rename / code-action handlers
//! - `eval_requests` — the eval-aware hover + inlay handlers
//! - `progress`  — `$/progress` reporting helpers
use std::sync::Arc;

use dashmap::DashMap;
use tower_lsp::jsonrpc::Result;
use tower_lsp::lsp_types::request::{GotoImplementationParams, GotoImplementationResponse};
use tower_lsp::lsp_types::*;
use tower_lsp::{Client, LanguageServer};

use crate::analysis::{LintProvider, NoLint, NoTypes, TypeProvider};
use crate::config::M1Config;
use crate::document::Document;
use crate::eval::EvalConfig;
use crate::format::{Formatter, NoFormat};
use crate::line_index::PositionEncoding;
use crate::project_store::ProjectStore;

mod capabilities;
mod config;
mod context;
mod diagnostics;
mod editing;
mod eval_requests;
mod lifecycle;
mod navigation;
mod progress;
mod tokens;

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
    /// Whether the client supports `WorkspaceEdit.changeAnnotations` (set during
    /// `initialize`). When it does, multi-file / file-renaming renames are tagged
    /// with a confirmation annotation so the client can preview them (#151).
    change_annotation_support: std::sync::atomic::AtomicBool,
    /// Whether the client supports pull diagnostics (`textDocument/diagnostic`),
    /// set during `initialize`. When it does, the server serves diagnostics via
    /// the pull handlers ONLY and does not also push `publishDiagnostics`: pushing
    /// to a pull-capable client makes editors that keep push and pull diagnostics
    /// in separate collections (VS Code) display every diagnostic twice. Pull
    /// clients re-request open docs on change themselves; for project-model
    /// changes the server nudges them with `workspace/diagnostic/refresh`.
    client_pull_diagnostics: std::sync::atomic::AtomicBool,
    /// Whether the client supports `window/workDoneProgress` (set during
    /// `initialize`). Gates `$/progress` reporting for the long operations —
    /// workspace diagnostics over a real corpus and project-wide rename (#266).
    progress_support: std::sync::atomic::AtomicBool,
    /// Client supports `workspace/inlayHint/refresh` / `…/semanticTokens/refresh`
    /// / `…/codeLens/refresh` — nudged after every project-model reload so unit
    /// hints, token colors and rate lenses don't go stale until the user types
    /// (#232).
    inlay_refresh_support: std::sync::atomic::AtomicBool,
    semtok_refresh_support: std::sync::atomic::AtomicBool,
    code_lens_refresh_support: std::sync::atomic::AtomicBool,
    /// The resolved unified config (lint/format/diagnostics) currently applied to
    /// the backends. Re-resolved on root discovery, `m1-tools.toml` change, and
    /// `didChangeConfiguration`; its `diagnostics` filter is read on every publish.
    config: std::sync::RwLock<M1Config>,
    /// The resolved LSP-local evaluation config (`m1.eval.*`), re-read from the
    /// editor settings on the same `didChangeConfiguration` path as `config`.
    /// **Disabled by default**: with eval off, hover/inlay behave as today and
    /// no engine is ever built. This is intentionally *not* part of `M1Config`
    /// (whose `M1ToolsConfig` is tag-pinned with no `[eval]` section).
    eval_config: std::sync::RwLock<EvalConfig>,
    /// The last editor settings (`initializationOptions` / `didChangeConfiguration`),
    /// the middle precedence layer beneath `m1-tools.toml`.
    editor_settings: std::sync::RwLock<Option<serde_json::Value>>,
    /// The project root last used to resolve config, so `didChangeConfiguration`
    /// can re-resolve against the same workspace.
    config_root: std::sync::RwLock<Option<std::path::PathBuf>>,
    /// Per-document snapshot of the last full semantic-token response
    /// (`result_id` → token data), backing `semanticTokens/full/delta` (#231).
    semtok_prev: DashMap<Url, (String, Vec<SemanticToken>)>,
    /// Monotonic source of semantic-token result ids.
    semtok_seq: std::sync::atomic::AtomicU64,
    /// Per-document snapshot of the last pull-diagnostic response (`result_id` →
    /// the items that id labels), backing the LSP 3.17 `result_id`/`Unchanged`
    /// protocol on `textDocument/diagnostic` and `workspace/diagnostic` (#259).
    /// When a poll recomputes the same items the client already holds (matching
    /// `previous_result_id`), the server answers `Unchanged` instead of
    /// re-serializing the full set. Same shape as `semtok_prev`.
    diag_prev: DashMap<Url, (String, Vec<Diagnostic>)>,
    /// Monotonic source of pull-diagnostic result ids; a fresh id is minted only
    /// when a document's diagnostics actually change.
    diag_seq: std::sync::atomic::AtomicU64,
}

impl Backend {
    pub fn new(client: Client) -> Self {
        Self::with_backends(
            client,
            Box::new(NoLint),
            Box::new(NoTypes),
            Box::new(NoFormat),
            Arc::new(ProjectStore::new()),
        )
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
            change_annotation_support: std::sync::atomic::AtomicBool::new(false),
            client_pull_diagnostics: std::sync::atomic::AtomicBool::new(false),
            progress_support: std::sync::atomic::AtomicBool::new(false),
            inlay_refresh_support: std::sync::atomic::AtomicBool::new(false),
            semtok_refresh_support: std::sync::atomic::AtomicBool::new(false),
            code_lens_refresh_support: std::sync::atomic::AtomicBool::new(false),
            config: std::sync::RwLock::new(M1Config::default()),
            eval_config: std::sync::RwLock::new(EvalConfig::default()),
            editor_settings: std::sync::RwLock::new(None),
            config_root: std::sync::RwLock::new(None),
            semtok_prev: DashMap::new(),
            semtok_seq: std::sync::atomic::AtomicU64::new(0),
            diag_prev: DashMap::new(),
            diag_seq: std::sync::atomic::AtomicU64::new(0),
        }
    }
}

#[tower_lsp::async_trait]
impl LanguageServer for Backend {
    async fn initialize(&self, params: InitializeParams) -> Result<InitializeResult> {
        self.initialize_impl(params).await
    }

    async fn initialized(&self, params: InitializedParams) {
        self.initialized_impl(params).await
    }

    async fn did_open(&self, params: DidOpenTextDocumentParams) {
        self.did_open_impl(params).await
    }

    async fn did_change(&self, params: DidChangeTextDocumentParams) {
        self.did_change_impl(params).await
    }

    async fn did_save(&self, params: DidSaveTextDocumentParams) {
        self.did_save_impl(params).await
    }

    async fn did_close(&self, params: DidCloseTextDocumentParams) {
        self.did_close_impl(params).await
    }

    async fn formatting(&self, params: DocumentFormattingParams) -> Result<Option<Vec<TextEdit>>> {
        self.formatting_impl(params).await
    }

    async fn range_formatting(
        &self,
        params: DocumentRangeFormattingParams,
    ) -> Result<Option<Vec<TextEdit>>> {
        self.range_formatting_impl(params).await
    }

    async fn on_type_formatting(
        &self,
        params: DocumentOnTypeFormattingParams,
    ) -> Result<Option<Vec<TextEdit>>> {
        self.on_type_formatting_impl(params).await
    }

    async fn symbol(
        &self,
        params: WorkspaceSymbolParams,
    ) -> Result<Option<Vec<SymbolInformation>>> {
        self.symbol_impl(params).await
    }

    async fn hover(&self, params: HoverParams) -> Result<Option<Hover>> {
        self.hover_impl(params).await
    }

    async fn goto_definition(
        &self,
        params: GotoDefinitionParams,
    ) -> Result<Option<GotoDefinitionResponse>> {
        self.goto_definition_impl(params).await
    }

    async fn goto_declaration(
        &self,
        params: request::GotoDeclarationParams,
    ) -> Result<Option<request::GotoDeclarationResponse>> {
        self.goto_declaration_impl(params).await
    }

    async fn goto_type_definition(
        &self,
        params: request::GotoTypeDefinitionParams,
    ) -> Result<Option<request::GotoTypeDefinitionResponse>> {
        self.goto_type_definition_impl(params).await
    }

    async fn document_link(&self, params: DocumentLinkParams) -> Result<Option<Vec<DocumentLink>>> {
        self.document_link_impl(params).await
    }

    async fn selection_range(
        &self,
        params: SelectionRangeParams,
    ) -> Result<Option<Vec<SelectionRange>>> {
        self.selection_range_impl(params).await
    }

    async fn goto_implementation(
        &self,
        params: GotoImplementationParams,
    ) -> Result<Option<GotoImplementationResponse>> {
        self.goto_implementation_impl(params).await
    }

    async fn document_symbol(
        &self,
        params: DocumentSymbolParams,
    ) -> Result<Option<DocumentSymbolResponse>> {
        self.document_symbol_impl(params).await
    }

    async fn completion(&self, params: CompletionParams) -> Result<Option<CompletionResponse>> {
        self.completion_impl(params).await
    }

    async fn completion_resolve(&self, item: CompletionItem) -> Result<CompletionItem> {
        self.completion_resolve_impl(item).await
    }

    async fn signature_help(&self, params: SignatureHelpParams) -> Result<Option<SignatureHelp>> {
        self.signature_help_impl(params).await
    }

    async fn inlay_hint(&self, params: InlayHintParams) -> Result<Option<Vec<InlayHint>>> {
        self.inlay_hint_impl(params).await
    }

    async fn code_lens(&self, params: CodeLensParams) -> Result<Option<Vec<CodeLens>>> {
        self.code_lens_impl(params).await
    }

    async fn prepare_call_hierarchy(
        &self,
        params: CallHierarchyPrepareParams,
    ) -> Result<Option<Vec<CallHierarchyItem>>> {
        self.prepare_call_hierarchy_impl(params).await
    }

    async fn incoming_calls(
        &self,
        params: CallHierarchyIncomingCallsParams,
    ) -> Result<Option<Vec<CallHierarchyIncomingCall>>> {
        self.incoming_calls_impl(params).await
    }

    async fn outgoing_calls(
        &self,
        params: CallHierarchyOutgoingCallsParams,
    ) -> Result<Option<Vec<CallHierarchyOutgoingCall>>> {
        self.outgoing_calls_impl(params).await
    }

    async fn prepare_rename(
        &self,
        params: TextDocumentPositionParams,
    ) -> Result<Option<PrepareRenameResponse>> {
        self.prepare_rename_impl(params).await
    }

    async fn will_rename_files(&self, params: RenameFilesParams) -> Result<Option<WorkspaceEdit>> {
        self.will_rename_files_impl(params).await
    }

    async fn rename(&self, params: RenameParams) -> Result<Option<WorkspaceEdit>> {
        self.rename_impl(params).await
    }

    async fn semantic_tokens_full(
        &self,
        params: SemanticTokensParams,
    ) -> Result<Option<SemanticTokensResult>> {
        self.semantic_tokens_full_impl(params).await
    }

    async fn semantic_tokens_full_delta(
        &self,
        params: SemanticTokensDeltaParams,
    ) -> Result<Option<SemanticTokensFullDeltaResult>> {
        self.semantic_tokens_full_delta_impl(params).await
    }

    async fn semantic_tokens_range(
        &self,
        params: SemanticTokensRangeParams,
    ) -> Result<Option<SemanticTokensRangeResult>> {
        self.semantic_tokens_range_impl(params).await
    }

    async fn references(&self, params: ReferenceParams) -> Result<Option<Vec<Location>>> {
        self.references_impl(params).await
    }

    async fn document_highlight(
        &self,
        params: DocumentHighlightParams,
    ) -> Result<Option<Vec<DocumentHighlight>>> {
        self.document_highlight_impl(params).await
    }

    async fn folding_range(&self, params: FoldingRangeParams) -> Result<Option<Vec<FoldingRange>>> {
        self.folding_range_impl(params).await
    }

    async fn code_action(&self, params: CodeActionParams) -> Result<Option<CodeActionResponse>> {
        self.code_action_impl(params).await
    }

    async fn did_change_watched_files(&self, params: DidChangeWatchedFilesParams) {
        self.did_change_watched_files_impl(params).await
    }

    async fn did_change_configuration(&self, params: DidChangeConfigurationParams) {
        self.did_change_configuration_impl(params).await
    }

    async fn diagnostic(
        &self,
        params: DocumentDiagnosticParams,
    ) -> Result<DocumentDiagnosticReportResult> {
        self.diagnostic_impl(params).await
    }

    async fn workspace_diagnostic(
        &self,
        params: WorkspaceDiagnosticParams,
    ) -> Result<WorkspaceDiagnosticReportResult> {
        self.workspace_diagnostic_impl(params).await
    }

    async fn shutdown(&self) -> Result<()> {
        Ok(())
    }
}
