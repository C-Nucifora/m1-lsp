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
    call_hierarchy, code_action, code_lens, completion, document_link, document_symbols, folding,
    goto, hover, inlay, references, rename, selection_range, semantic_tokens, signature_help,
    workspace_symbol,
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
}

/// Everything a request handler needs about one open document, gathered once: the
/// cloned text + line index (released from the `DashMap` guard), the negotiated
/// position encoding, and the file basename used for group-relative resolution.
/// Replaces the get-doc / `enc()` / byte-offset / `file_name` plumbing that every
/// cursor-position handler repeated. The CST is parsed by the caller via
/// [`DocContext::parse`] — a `Node` borrows the tree, which must outlive the borrow.
struct DocContext {
    text: String,
    line_index: crate::line_index::LineIndex,
    enc: PositionEncoding,
    file_name: Option<String>,
}

impl DocContext {
    /// Byte offset of an LSP `position` within this document.
    fn byte(&self, position: Position) -> usize {
        self.line_index.offset(position, &self.text, self.enc)
    }

    /// Parse the document text into a CST. The caller holds the returned `Cst` so
    /// `Node`s borrowed from it stay valid.
    fn parse(&self) -> m1_core::Cst {
        m1_core::parse(&self.text)
    }
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
            inlay_refresh_support: std::sync::atomic::AtomicBool::new(false),
            semtok_refresh_support: std::sync::atomic::AtomicBool::new(false),
            code_lens_refresh_support: std::sync::atomic::AtomicBool::new(false),
            config: std::sync::RwLock::new(M1Config::default()),
            editor_settings: std::sync::RwLock::new(None),
            config_root: std::sync::RwLock::new(None),
            semtok_prev: DashMap::new(),
            semtok_seq: std::sync::atomic::AtomicU64::new(0),
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

    /// Next semantic-token `result_id` (#231).
    fn next_semtok_id(&self) -> String {
        self.semtok_seq
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            .to_string()
    }

    fn enc(&self) -> PositionEncoding {
        *self.encoding.read().unwrap()
    }

    /// The current text and line index of an open document, cloned out so the
    /// `DashMap` entry guard is released before parsing. `None` when the document
    /// isn't open. Every request handler that needs the buffer goes through this.
    fn get_doc(&self, uri: &Url) -> Option<(String, crate::line_index::LineIndex)> {
        self.docs
            .get(uri)
            .map(|d| (d.text.clone(), d.line_index.clone()))
    }

    /// Bundle an open document's text / line index / encoding / basename for a
    /// request handler ([`DocContext`]). `None` when the document isn't open — the
    /// caller returns its empty response, as the raw [`get_doc`](Self::get_doc) did.
    fn doc_context(&self, uri: &Url) -> Option<DocContext> {
        let (text, line_index) = self.get_doc(uri)?;
        Some(DocContext {
            text,
            line_index,
            enc: self.enc(),
            file_name: crate::features::locate::file_name_of(uri),
        })
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
        // The rename may have changed which parameters are covered / names valid.
        self.publish_project_diagnostics().await;
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
    fn diagnostics_for(&self, uri: &Url) -> Option<Vec<Diagnostic>> {
        let (text, lindex) = match self.get_doc(uri) {
            Some(doc) => doc,
            None => {
                let path = uri.to_file_path().ok()?;
                let text = crate::disk_read::read_disk(&path)?;
                let li = crate::line_index::LineIndex::new(&text);
                (text, li)
            }
        };
        let enc = self.enc();
        if is_m1prj(uri) {
            let active = self
                .store
                .with_project(|p| p.and_then(|lp| Url::from_file_path(&lp.m1prj_path).ok()));
            return Some(if active.as_ref() == Some(uri) {
                let filter = self.config.read().unwrap().diagnostics.clone();
                self.store
                    .project_diagnostics()
                    .iter()
                    .filter(|d| filter.allows_subject(d.code.as_str(), d.subject.as_deref()))
                    .map(|d| crate::convert::type_diagnostic(d, &lindex, enc))
                    .collect()
            } else {
                vec![]
            });
        }
        Some(analyze(
            uri,
            &text,
            &lindex,
            enc,
            self.lint.as_ref(),
            self.types.as_ref(),
            &self.config.read().unwrap().diagnostics,
        ))
    }

    async fn publish(&self, uri: Url) {
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

    /// Publish the project-scope diagnostics (the `.m1cfg`-coverage / name
    /// audits — T041/T050/T010/T071) anchored to the loaded `.m1prj`. These are
    /// not tied to any open script, so the editor shows them as soon as the
    /// project loads, matching what the CLI reports (#139). Publishes an empty
    /// set (clearing stale entries) when the project loaded cleanly with no
    /// findings; a no-op when no project is loaded.
    /// Nudge the client to re-pull every project-derived view (#232): inlay
    /// hints (`[unit]` badges), semantic tokens and code lenses are all
    /// computed from the project model, so they go stale on `.m1prj`/config
    /// reload until the client refreshes them. Each refresh is gated on the
    /// capability the client declared at initialize.
    async fn refresh_project_views(&self) {
        use std::sync::atomic::Ordering::Relaxed;
        if self.inlay_refresh_support.load(Relaxed) {
            let _ = self.client.inlay_hint_refresh().await;
        }
        if self.semtok_refresh_support.load(Relaxed) {
            let _ = self.client.semantic_tokens_refresh().await;
        }
        if self.code_lens_refresh_support.load(Relaxed) {
            let _ = self.client.code_lens_refresh().await;
        }
    }

    async fn publish_project_diagnostics(&self) {
        // Every caller of this function has just (re)loaded the project model,
        // so the project-derived views need a refresh too (#232).
        self.refresh_project_views().await;

        // Pull-capable clients receive project-scope diagnostics via
        // `workspace/diagnostic`; after a project-model change (reload, `.m1prj`
        // or config edit) nudge them to re-pull instead of pushing — pushing here
        // too would duplicate diagnostics in VS Code (#NNN).
        if self
            .client_pull_diagnostics
            .load(std::sync::atomic::Ordering::Relaxed)
        {
            let _ = self.client.workspace_diagnostic_refresh().await;
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
        let filter = self.config.read().unwrap().diagnostics.clone();
        let diags: Vec<Diagnostic> = self
            .store
            .project_diagnostics()
            .iter()
            .filter(|d| filter.allows_subject(d.code.as_str(), d.subject.as_deref()))
            .map(|d| crate::convert::type_diagnostic(d, &li, enc))
            .collect();
        let version = self.docs.get(&uri).map(|d| d.version);
        self.client.publish_diagnostics(uri, diags, version).await;
    }
}

/// Whether the client advertised support for pull diagnostics
/// (`textDocument/diagnostic`). When true, the server must serve diagnostics via
/// the pull handlers ONLY and not also push `publishDiagnostics` — pushing to a
/// pull-capable client doubles every diagnostic in editors (VS Code) that keep
/// push and pull diagnostics in separate collections.
fn client_supports_pull_diagnostics(caps: &ClientCapabilities) -> bool {
    caps.text_document
        .as_ref()
        .and_then(|t| t.diagnostic.as_ref())
        .is_some()
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

/// The static set of LSP capabilities the server advertises in `initialize`.
/// `encoding` is the position encoding negotiated with the client; everything
/// else is fixed at build time.
fn server_capabilities(encoding: PositionEncodingKind) -> ServerCapabilities {
    ServerCapabilities {
        position_encoding: Some(encoding),
        text_document_sync: Some(TextDocumentSyncCapability::Kind(TextDocumentSyncKind::FULL)),
        document_formatting_provider: Some(OneOf::Left(true)),
        document_range_formatting_provider: Some(OneOf::Left(true)),
        // #234: re-indent the just-closed block when `}` is typed — pasted
        // code in a different style snaps to Allman/tab layout live.
        document_on_type_formatting_provider: Some(DocumentOnTypeFormattingOptions {
            first_trigger_character: "}".to_string(),
            more_trigger_character: None,
        }),
        workspace_symbol_provider: Some(OneOf::Left(true)),
        hover_provider: Some(HoverProviderCapability::Simple(true)),
        definition_provider: Some(OneOf::Left(true)),
        // Go to Declaration (== definition for project symbols) and Go to
        // Type Definition (enum-typed channel → its <Type> block) (#168).
        declaration_provider: Some(DeclarationCapability::Simple(true)),
        type_definition_provider: Some(TypeDefinitionProviderCapability::Simple(true)),
        // Hyperlink `Filename="…"` attributes in Project.m1prj (#175).
        document_link_provider: Some(DocumentLinkOptions {
            resolve_provider: Some(false),
            work_done_progress_options: Default::default(),
        }),
        // Hierarchical "expand selection" (#173).
        selection_range_provider: Some(SelectionRangeProviderCapability::Simple(true)),
        implementation_provider: Some(ImplementationProviderCapability::Simple(true)),
        references_provider: Some(OneOf::Left(true)),
        document_highlight_provider: Some(OneOf::Left(true)),
        folding_range_provider: Some(FoldingRangeProviderCapability::Simple(true)),
        // Advertise the kinds we emit so editors can wire fix-all-on-save
        // (the whole-file m1-lint fixer, #158) and group quick-fixes.
        code_action_provider: Some(CodeActionProviderCapability::Options(CodeActionOptions {
            code_action_kinds: Some(vec![
                CodeActionKind::QUICKFIX,
                CodeActionKind::REFACTOR_EXTRACT,
                CodeActionKind::REFACTOR_INLINE,
                CodeActionKind::SOURCE_FIX_ALL,
                CodeActionKind::SOURCE,
            ]),
            resolve_provider: Some(false),
            work_done_progress_options: Default::default(),
        })),
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
        // Pull diagnostics (#140): answer `textDocument/diagnostic` and
        // `workspace/diagnostic` so pull-capable clients (Neovim's
        // vim.diagnostic, Helix) and unopened files get full coverage,
        // not just the push path's open buffers. No inter-file deps — a
        // script's diagnostics depend only on itself plus the static
        // project model, so editing one script can't change another's.
        diagnostic_provider: Some(DiagnosticServerCapabilities::Options(DiagnosticOptions {
            identifier: Some("m1-lsp".into()),
            inter_file_dependencies: false,
            workspace_diagnostics: true,
            work_done_progress_options: Default::default(),
        })),
        code_lens_provider: Some(CodeLensOptions {
            resolve_provider: Some(false),
        }),
        call_hierarchy_provider: Some(CallHierarchyServerCapability::Simple(true)),
        rename_provider: Some(OneOf::Right(RenameOptions {
            prepare_provider: Some(true),
            work_done_progress_options: Default::default(),
        })),
        semantic_tokens_provider: Some(SemanticTokensServerCapabilities::SemanticTokensOptions(
            SemanticTokensOptions {
                legend: semantic_tokens::legend(),
                full: Some(SemanticTokensFullOptions::Delta { delta: Some(true) }),
                range: Some(true),
                work_done_progress_options: Default::default(),
            },
        )),
        ..Default::default()
    }
}

#[tower_lsp::async_trait]
impl LanguageServer for Backend {
    async fn initialize(&self, params: InitializeParams) -> Result<InitializeResult> {
        // Record whether the client supports pull diagnostics (read before the
        // encoding negotiation below moves fields out of `capabilities`). If it
        // does, the server serves diagnostics via the pull handlers ONLY and
        // suppresses the push path — otherwise a pull-capable client that keeps
        // push and pull in separate collections (VS Code) shows everything twice.
        self.client_pull_diagnostics.store(
            client_supports_pull_diagnostics(&params.capabilities),
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
        // Surface the project-scope audit (T041/T050/…) now that the client is
        // ready to receive diagnostics (#139).
        self.publish_project_diagnostics().await;
    }

    async fn did_open(&self, params: DidOpenTextDocumentParams) {
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

    async fn did_change(&self, params: DidChangeTextDocumentParams) {
        // FULL sync: the last content change holds the entire new text.
        if let Some(change) = params.content_changes.into_iter().last() {
            let uri = params.text_document.uri;
            self.docs.insert(
                uri.clone(),
                Document::new(change.text, params.text_document.version),
            );
            // The edited buffer can change script reads/writes — drop the cached
            // call graph (rebuilt on the next call-hierarchy request).
            self.store.invalidate_call_graph();
            self.publish(uri).await;
        }
    }

    async fn did_save(&self, params: DidSaveTextDocumentParams) {
        // Disk now matches the buffer; the graph reads buffers first, so this is
        // belt-and-braces, but keeps the cache honest for any disk-sourced script.
        self.store.invalidate_call_graph();
        self.publish(params.text_document.uri).await;
    }

    async fn did_close(&self, params: DidCloseTextDocumentParams) {
        let uri = params.text_document.uri;
        self.docs.remove(&uri);
        self.semtok_prev.remove(&uri);
        // The graph would now read this file from disk instead of the buffer.
        self.store.invalidate_call_graph();
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

    async fn on_type_formatting(
        &self,
        params: DocumentOnTypeFormattingParams,
    ) -> Result<Option<Vec<TextEdit>>> {
        // Triggered after `}` (#234): range-format the line that was just
        // closed. `range_format_edits` snaps to the deepest statement spanning
        // it (m1-fmt #98), so this re-indents exactly the closed construct.
        let pos = params.text_document_position.position;
        let uri = params.text_document_position.text_document.uri;
        let range = tower_lsp::lsp_types::Range::new(
            Position::new(pos.line, 0),
            Position::new(pos.line, pos.character),
        );
        Ok(self
            .docs
            .get(&uri)
            .and_then(|doc| range_format_edits(&doc, range, self.formatter.as_ref())))
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
        let Some(doc) = self.doc_context(&tdp.text_document.uri) else {
            return Ok(None);
        };
        let byte = doc.byte(tdp.position);
        let cst = doc.parse();
        Ok(self.store.with_project(|p| {
            hover::hover(
                cst.root(),
                byte,
                p.map(|lp| &lp.project),
                doc.file_name.as_deref(),
                &doc.line_index,
                doc.enc,
            )
        }))
    }

    async fn goto_definition(
        &self,
        params: GotoDefinitionParams,
    ) -> Result<Option<GotoDefinitionResponse>> {
        let tdp = params.text_document_position_params;
        let uri = tdp.text_document.uri;
        let Some(doc) = self.doc_context(&uri) else {
            return Ok(None);
        };
        let byte = doc.byte(tdp.position);
        let cst = doc.parse();
        // Project symbols (channels/params/functions/DBC) resolve via the
        // project; a bare `local` resolves in-file and works even with no project
        // loaded (#141).
        let loc = self
            .store
            .with_project(|p| {
                p.and_then(|lp| goto::goto(cst.root(), byte, lp, doc.file_name.as_deref()))
            })
            .or_else(|| goto::goto_local(cst.root(), byte, &uri, &doc.line_index, doc.enc));
        Ok(loc.map(GotoDefinitionResponse::Scalar))
    }

    /// textDocument/declaration: for project symbols this is the same `.m1prj`
    /// `<Component>` (or backing file) site as definition — the LSP-canonical home
    /// for the jump (#168).
    async fn goto_declaration(
        &self,
        params: request::GotoDeclarationParams,
    ) -> Result<Option<request::GotoDeclarationResponse>> {
        let tdp = params.text_document_position_params;
        let uri = tdp.text_document.uri;
        let Some(doc) = self.doc_context(&uri) else {
            return Ok(None);
        };
        let byte = doc.byte(tdp.position);
        let cst = doc.parse();
        let loc = self
            .store
            .with_project(|p| {
                p.and_then(|lp| goto::goto(cst.root(), byte, lp, doc.file_name.as_deref()))
            })
            .or_else(|| goto::goto_local(cst.root(), byte, &uri, &doc.line_index, doc.enc));
        Ok(loc.map(request::GotoDeclarationResponse::Scalar))
    }

    /// textDocument/typeDefinition: from an enum-typed channel/parameter, jump to
    /// its `<Type>` block in the `.m1prj` (#168).
    async fn goto_type_definition(
        &self,
        params: request::GotoTypeDefinitionParams,
    ) -> Result<Option<request::GotoTypeDefinitionResponse>> {
        let tdp = params.text_document_position_params;
        let Some(doc) = self.doc_context(&tdp.text_document.uri) else {
            return Ok(None);
        };
        let byte = doc.byte(tdp.position);
        let cst = doc.parse();
        let loc = self.store.with_project(|p| {
            p.and_then(|lp| {
                goto::goto_type_definition(cst.root(), byte, lp, doc.file_name.as_deref())
            })
        });
        Ok(loc.map(request::GotoTypeDefinitionResponse::Scalar))
    }

    /// textDocument/documentLink: hyperlink `Filename="…"` attributes in an open
    /// `Project.m1prj` to the script they name, relative to the project dir (#175).
    async fn document_link(&self, params: DocumentLinkParams) -> Result<Option<Vec<DocumentLink>>> {
        let uri = params.text_document.uri;
        let Some(doc) = self.doc_context(&uri) else {
            return Ok(None);
        };
        let Some(root) = uri
            .to_file_path()
            .ok()
            .and_then(|p| p.parent().map(|d| d.to_path_buf()))
        else {
            return Ok(None);
        };
        let links = document_link::document_links(&doc.text, &doc.line_index, doc.enc, &root);
        Ok((!links.is_empty()).then_some(links))
    }

    /// textDocument/selectionRange: hierarchical "expand selection" — one range
    /// chain per requested position (#173).
    async fn selection_range(
        &self,
        params: SelectionRangeParams,
    ) -> Result<Option<Vec<SelectionRange>>> {
        let Some(doc) = self.doc_context(&params.text_document.uri) else {
            return Ok(None);
        };
        let cst = doc.parse();
        let ranges: Vec<SelectionRange> = params
            .positions
            .iter()
            .filter_map(|pos| {
                let byte = doc.byte(*pos);
                selection_range::selection_range(cst.root(), byte, &doc.line_index, doc.enc)
            })
            .collect();
        Ok((ranges.len() == params.positions.len()).then_some(ranges))
    }

    async fn goto_implementation(
        &self,
        params: GotoImplementationParams,
    ) -> Result<Option<GotoImplementationResponse>> {
        let tdp = params.text_document_position_params;
        let uri = tdp.text_document.uri;
        let Some(doc) = self.doc_context(&uri) else {
            return Ok(None);
        };
        let byte = doc.byte(tdp.position);
        let enc = doc.enc;
        // "Implementation" of a channel = where it is written (produced). With a
        // project loaded, search every `.m1scr`; open buffers win over disk.
        let open_text = |u: &Url| self.docs.get(u).map(|d| d.text.clone());
        // Canonicalising the write sites across files needs the project model held
        // for the whole loop (#143); run it under the read guard via
        // `block_in_place` to keep the async runtime healthy (#135).
        let locs = tokio::task::block_in_place(|| {
            self.store.with_project(|p| {
                p.and_then(|lp| {
                    references::project_implementations(
                        &lp.project,
                        &lp.script_files,
                        &uri,
                        &doc.text,
                        byte,
                        enc,
                        &open_text,
                    )
                })
            })
        });
        Ok(locs.map(GotoDefinitionResponse::Array))
    }

    async fn document_symbol(
        &self,
        params: DocumentSymbolParams,
    ) -> Result<Option<DocumentSymbolResponse>> {
        let Some(doc) = self.doc_context(&params.text_document.uri) else {
            return Ok(None);
        };
        let cst = doc.parse();
        let syms = document_symbols::document_symbols(cst.root(), &doc.line_index, doc.enc);
        Ok(Some(DocumentSymbolResponse::Nested(syms)))
    }

    async fn completion(&self, params: CompletionParams) -> Result<Option<CompletionResponse>> {
        let tdp = params.text_document_position;
        let Some(doc) = self.doc_context(&tdp.text_document.uri) else {
            return Ok(None);
        };
        let byte = doc.byte(tdp.position);
        let cst = doc.parse();
        let items = self.store.with_project(|p| {
            completion::completions(
                cst.root(),
                p,
                doc.file_name.as_deref(),
                &doc.text,
                byte,
                &doc.line_index,
                doc.enc,
            )
        });
        Ok(Some(CompletionResponse::Array(items)))
    }

    async fn signature_help(&self, params: SignatureHelpParams) -> Result<Option<SignatureHelp>> {
        let tdp = params.text_document_position_params;
        let Some(doc) = self.doc_context(&tdp.text_document.uri) else {
            return Ok(None);
        };
        let byte = doc.byte(tdp.position);
        let cst = doc.parse();
        Ok(self.store.with_project(|p| {
            signature_help::signature_help(
                cst.root(),
                byte,
                p.map(|lp| &lp.project),
                doc.file_name.as_deref(),
            )
        }))
    }

    async fn inlay_hint(&self, params: InlayHintParams) -> Result<Option<Vec<InlayHint>>> {
        let Some(doc) = self.doc_context(&params.text_document.uri) else {
            return Ok(None);
        };
        let cst = doc.parse();
        let hints = self.store.with_project(|p| {
            inlay::inlay_hints(
                cst.root(),
                params.range,
                &doc.line_index,
                doc.enc,
                p.map(|lp| &lp.project),
                doc.file_name.as_deref(),
            )
        });
        Ok(Some(hints))
    }

    async fn code_lens(&self, params: CodeLensParams) -> Result<Option<Vec<CodeLens>>> {
        let uri = params.text_document.uri;
        // The logging/security badges (#171/#172) resolve the channels the
        // script writes, which needs its text: prefer the open buffer, fall
        // back to disk.
        let text = self.docs.get(&uri).map(|d| d.text.clone()).or_else(|| {
            uri.to_file_path()
                .ok()
                .and_then(|p| crate::disk_read::read_disk(&p))
        });
        Ok(self
            .store
            .with_project(|p| p.map(|lp| code_lens::code_lens(lp, &uri, text.as_deref()))))
    }

    async fn prepare_call_hierarchy(
        &self,
        params: CallHierarchyPrepareParams,
    ) -> Result<Option<Vec<CallHierarchyItem>>> {
        let uri = params.text_document_position_params.text_document.uri;
        let Some(doc) = self.doc_context(&uri) else {
            return Ok(None);
        };
        let byte = doc.byte(params.text_document_position_params.position);
        let enc = doc.enc;
        let open_text = |u: &Url| self.docs.get(u).map(|d| d.text.clone());
        // Reads + parses every script under the live project; run off the async
        // worker via `block_in_place` (#135). The graph is built once per
        // call-hierarchy interaction and cached in the store (it is invalidated on
        // any buffer edit), so prepare/incoming/outgoing share one build.
        Ok(tokio::task::block_in_place(|| {
            self.store.with_call_graph(
                |lp| call_hierarchy::CallGraph::build(lp, enc, &open_text),
                |pg| pg.and_then(|(lp, g)| call_hierarchy::prepare(lp, g, &uri, &doc.text, byte)),
            )
        }))
    }

    async fn incoming_calls(
        &self,
        params: CallHierarchyIncomingCallsParams,
    ) -> Result<Option<Vec<CallHierarchyIncomingCall>>> {
        let enc = self.enc();
        let open_text = |u: &Url| self.docs.get(u).map(|d| d.text.clone());
        // Uses the cached graph from this interaction's `prepare` (rebuilt only if
        // a buffer changed); see `prepare_call_hierarchy`.
        Ok(tokio::task::block_in_place(|| {
            self.store.with_call_graph(
                |lp| call_hierarchy::CallGraph::build(lp, enc, &open_text),
                |pg| pg.and_then(|(_, g)| call_hierarchy::incoming(g, &params.item)),
            )
        }))
    }

    async fn outgoing_calls(
        &self,
        params: CallHierarchyOutgoingCallsParams,
    ) -> Result<Option<Vec<CallHierarchyOutgoingCall>>> {
        let enc = self.enc();
        let open_text = |u: &Url| self.docs.get(u).map(|d| d.text.clone());
        // Uses the cached graph from this interaction's `prepare` (rebuilt only if
        // a buffer changed); see `prepare_call_hierarchy`.
        Ok(tokio::task::block_in_place(|| {
            self.store.with_call_graph(
                |lp| call_hierarchy::CallGraph::build(lp, enc, &open_text),
                |pg| pg.and_then(|(lp, g)| call_hierarchy::outgoing(lp, g, &params.item)),
            )
        }))
    }

    async fn prepare_rename(
        &self,
        params: TextDocumentPositionParams,
    ) -> Result<Option<PrepareRenameResponse>> {
        let uri = params.text_document.uri;
        let Some(doc) = self.doc_context(&uri) else {
            return Ok(None);
        };
        let byte = doc.byte(params.position);
        // The `.m1prj` is XML, not a script: offer rename on a component's Name.
        if is_m1prj(&uri) {
            return Ok(self.store.with_project(|p| {
                rename::prepare_m1prj(&doc.text, byte, doc.enc, p.map(|lp| &lp.project))
            }));
        }
        let cst = doc.parse();
        Ok(self.store.with_project(|p| {
            rename::prepare(
                cst.root(),
                byte,
                &doc.line_index,
                doc.enc,
                p.map(|lp| &lp.project),
                doc.file_name.as_deref(),
            )
        }))
    }

    async fn rename(&self, params: RenameParams) -> Result<Option<WorkspaceEdit>> {
        let new_name = params.new_name;
        let tdp = params.text_document_position;
        let uri = tdp.text_document.uri;
        let Some(doc) = self.doc_context(&uri) else {
            return Ok(None);
        };
        let byte = doc.byte(tdp.position);
        let enc = doc.enc;
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
                        &doc.text,
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
            let cst = doc.parse();
            tokio::task::block_in_place(|| {
                self.store.with_project(|p| {
                    rename::execute(
                        cst.root(),
                        byte,
                        &new_name,
                        uri.clone(),
                        &doc.line_index,
                        enc,
                        p,
                        doc.file_name.as_deref(),
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
                // Tag multi-file / file-renaming edits with a confirmation
                // annotation so capable clients can preview them (#151).
                let supported = self
                    .change_annotation_support
                    .load(std::sync::atomic::Ordering::Relaxed);
                Ok(Some(rename::annotate_for_confirmation(
                    edit, &new_name, supported,
                )))
            }
            Err(e) => Err(Error::invalid_params(e)),
        }
    }

    async fn semantic_tokens_full(
        &self,
        params: SemanticTokensParams,
    ) -> Result<Option<SemanticTokensResult>> {
        let Some(doc) = self.doc_context(&params.text_document.uri) else {
            return Ok(None);
        };
        let cst = doc.parse();
        let tokens = self.store.with_project(|p| {
            semantic_tokens::semantic_tokens(
                cst.root(),
                p.map(|lp| &lp.project),
                doc.file_name.as_deref(),
                &doc.line_index,
                doc.enc,
            )
        });
        let id = self.next_semtok_id();
        self.semtok_prev.insert(
            params.text_document.uri.clone(),
            (id.clone(), tokens.clone()),
        );
        Ok(Some(SemanticTokensResult::Tokens(SemanticTokens {
            result_id: Some(id),
            data: tokens,
        })))
    }

    async fn semantic_tokens_full_delta(
        &self,
        params: SemanticTokensDeltaParams,
    ) -> Result<Option<SemanticTokensFullDeltaResult>> {
        let uri = params.text_document.uri;
        let Some(doc) = self.doc_context(&uri) else {
            return Ok(None);
        };
        let cst = doc.parse();
        let tokens = self.store.with_project(|p| {
            semantic_tokens::semantic_tokens(
                cst.root(),
                p.map(|lp| &lp.project),
                doc.file_name.as_deref(),
                &doc.line_index,
                doc.enc,
            )
        });
        let id = self.next_semtok_id();
        let prev = self
            .semtok_prev
            .insert(uri.clone(), (id.clone(), tokens.clone()));
        // Only diff against the snapshot the client says it holds; anything
        // else (restart, eviction) falls back to a full response.
        let matching_prev = prev
            .filter(|(prev_id, _)| *prev_id == params.previous_result_id)
            .map(|(_, data)| data);
        Ok(Some(match matching_prev {
            Some(prev_data) => SemanticTokensFullDeltaResult::TokensDelta(SemanticTokensDelta {
                result_id: Some(id),
                edits: crate::semtok_delta::single_splice_edit(&prev_data, &tokens),
            }),
            None => SemanticTokensFullDeltaResult::Tokens(SemanticTokens {
                result_id: Some(id),
                data: tokens,
            }),
        }))
    }

    async fn semantic_tokens_range(
        &self,
        params: SemanticTokensRangeParams,
    ) -> Result<Option<SemanticTokensRangeResult>> {
        let Some(doc) = self.doc_context(&params.text_document.uri) else {
            return Ok(None);
        };
        let cst = doc.parse();
        let tokens = self.store.with_project(|p| {
            semantic_tokens::semantic_tokens_range(
                cst.root(),
                p.map(|lp| &lp.project),
                doc.file_name.as_deref(),
                &doc.line_index,
                doc.enc,
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
        let Some(doc) = self.doc_context(&uri) else {
            return Ok(None);
        };
        let byte = doc.byte(tdp.position);
        let enc = doc.enc;
        // With a project loaded, search every `.m1scr` for a project symbol
        // (#29); locals stay file-local. Open buffers win over on-disk text.
        let open_text = |u: &Url| self.docs.get(u).map(|d| d.text.clone());
        // Canonicalising occurrences across files needs the project model held for
        // the whole read+parse loop (group-relative resolution, #143), so run it
        // under the read guard via `block_in_place` to keep the async runtime
        // healthy (#135). `with_project` returns `None` only when no project is
        // loaded; an inner `None` means a project is loaded but nothing matched.
        let result = tokio::task::block_in_place(|| {
            self.store.with_project(|p| {
                p.map(|lp| {
                    references::project_references(
                        &lp.project,
                        &lp.script_files,
                        &uri,
                        &doc.text,
                        byte,
                        enc,
                        &open_text,
                    )
                })
            })
        });
        if let Some(locs) = result {
            return Ok(locs);
        }
        // Project-less mode: single-file references.
        let cst = doc.parse();
        Ok(references::references(
            cst.root(),
            byte,
            uri.clone(),
            &doc.line_index,
            enc,
        ))
    }

    async fn document_highlight(
        &self,
        params: DocumentHighlightParams,
    ) -> Result<Option<Vec<DocumentHighlight>>> {
        let tdp = params.text_document_position_params;
        let Some(doc) = self.doc_context(&tdp.text_document.uri) else {
            return Ok(None);
        };
        let byte = doc.byte(tdp.position);
        let cst = doc.parse();
        // Project-aware so a channel spelled two ways in one file highlights as one (#143).
        Ok(self.store.with_project(|p| {
            references::document_highlights_scoped(
                p.map(|lp| &lp.project),
                doc.file_name.as_deref(),
                cst.root(),
                byte,
                &doc.line_index,
                doc.enc,
            )
        }))
    }

    async fn folding_range(&self, params: FoldingRangeParams) -> Result<Option<Vec<FoldingRange>>> {
        let Some(doc) = self.doc_context(&params.text_document.uri) else {
            return Ok(None);
        };
        let cst = doc.parse();
        Ok(Some(folding::folding_ranges(
            cst.root(),
            &doc.line_index,
            doc.enc,
        )))
    }

    async fn code_action(&self, params: CodeActionParams) -> Result<Option<CodeActionResponse>> {
        let uri = params.text_document.uri;
        let Some(doc) = self.doc_context(&uri) else {
            return Ok(None);
        };
        let enc = doc.enc;
        // The project model backs the T020 "did you mean" enum-member fix (#159).
        let mut actions = self.store.with_project(|p| {
            code_action::code_actions(
                &doc.text,
                &doc.line_index,
                enc,
                &uri,
                &params.context.diagnostics,
                p.map(|lp| &lp.project),
            )
        });
        // Whole-file "fix all auto-fixable lint issues" via the shared m1-lint
        // fixer — covers every fixable rule (L003/L007/L011/L018…), not just the
        // hand-ported few (#158).
        if let Some(fixed) = self.lint.fix(&doc.text)
            && fixed != doc.text
        {
            actions.push(code_action::fix_all_lint_action(
                &uri,
                &doc.text,
                &doc.line_index,
                enc,
                fixed,
            ));
        }
        // Selection-driven refactors, offered independently of diagnostics (#174):
        // "Extract to local" on a selected expression, "Inline local" on a local.
        actions.extend(code_action::refactors(
            &doc.text,
            &doc.line_index,
            enc,
            &uri,
            params.range,
        ));
        // Source-level format actions, offered independently of diagnostics (#161)
        // so the menu can format clean code. "Format Document" appears when
        // formatting would change the file; "Format Selection" when the request
        // range spans more than one line.
        if let Some(doc) = self.docs.get(&uri) {
            if let Some(edits) = format_edits(&doc, enc, self.formatter.as_ref()) {
                actions.push(code_action::format_action("Format Document", &uri, edits));
            }
            if params.range.start.line < params.range.end.line
                && let Some(edits) = range_format_edits(&doc, params.range, self.formatter.as_ref())
            {
                actions.push(code_action::format_action("Format Selection", &uri, edits));
            }
        }
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
        // A `.m1cfg`/`.m1prj` edit can change cfg coverage or names — re-audit (#139).
        self.publish_project_diagnostics().await;
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

    /// Pull diagnostics for a single document (#140). Runs the same analysis as
    /// the push path, on demand, sourcing the text from the open buffer or disk —
    /// so a file the client has never opened still gets full coverage.
    async fn diagnostic(
        &self,
        params: DocumentDiagnosticParams,
    ) -> Result<DocumentDiagnosticReportResult> {
        let items = self
            .diagnostics_for(&params.text_document.uri)
            .unwrap_or_default();
        Ok(DocumentDiagnosticReportResult::Report(
            DocumentDiagnosticReport::Full(RelatedFullDocumentDiagnosticReport {
                related_documents: None,
                full_document_diagnostic_report: FullDocumentDiagnosticReport {
                    result_id: None,
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
    async fn workspace_diagnostic(
        &self,
        _params: WorkspaceDiagnosticParams,
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

        let mut items = Vec::with_capacity(paths.len());
        for path in paths {
            let Ok(uri) = Url::from_file_path(&path) else {
                continue;
            };
            let Some(diags) = self.diagnostics_for(&uri) else {
                continue;
            };
            // Report the in-editor version for open buffers so the client can
            // reconcile against its edits; `None` for closed files.
            let version = self.docs.get(&uri).map(|d| d.version as i64);
            items.push(WorkspaceDocumentDiagnosticReport::Full(
                WorkspaceFullDocumentDiagnosticReport {
                    uri,
                    version,
                    full_document_diagnostic_report: FullDocumentDiagnosticReport {
                        result_id: None,
                        items: diags,
                    },
                },
            ));
        }

        Ok(WorkspaceDiagnosticReportResult::Report(
            WorkspaceDiagnosticReport { items },
        ))
    }

    async fn shutdown(&self) -> Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod pull_diagnostics_tests {
    use super::client_supports_pull_diagnostics;
    use tower_lsp::lsp_types::{
        ClientCapabilities, DiagnosticClientCapabilities, TextDocumentClientCapabilities,
    };

    // Regression guard: a client that advertises `textDocument/diagnostic` (pull)
    // must be detected so the server suppresses the push path. Pushing as well
    // doubles every diagnostic in VS Code (push + pull land in separate
    // collections — observed 292 instead of 146 on the EV-M1 corpus).
    #[test]
    fn pull_capability_is_detected() {
        let mut caps = ClientCapabilities::default();
        assert!(
            !client_supports_pull_diagnostics(&caps),
            "no textDocument capabilities => legacy push client"
        );

        caps.text_document = Some(TextDocumentClientCapabilities::default());
        assert!(
            !client_supports_pull_diagnostics(&caps),
            "textDocument without `diagnostic` => legacy push client"
        );

        caps.text_document = Some(TextDocumentClientCapabilities {
            diagnostic: Some(DiagnosticClientCapabilities::default()),
            ..Default::default()
        });
        assert!(
            client_supports_pull_diagnostics(&caps),
            "textDocument.diagnostic present => pull client (push must be suppressed)"
        );
    }
}
