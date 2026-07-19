//! Static server-capability declaration and the client-capability probes read at
//! `initialize`, plus the two URI/settings helpers used across the handlers.
//! Free functions — no `Backend` state — so the "what the server advertises"
//! concern changes independently of the request handlers.
use tower_lsp::lsp_types::*;

/// Whether the client advertised support for pull diagnostics
/// (`textDocument/diagnostic`). When true, the server must serve diagnostics via
/// the pull handlers ONLY and not also push `publishDiagnostics` — pushing to a
/// pull-capable client doubles every diagnostic in editors (VS Code) that keep
/// push and pull diagnostics in separate collections.
pub(super) fn client_supports_pull_diagnostics(caps: &ClientCapabilities) -> bool {
    caps.text_document
        .as_ref()
        .and_then(|t| t.diagnostic.as_ref())
        .is_some()
}

/// True when `uri` points at a `Project.m1prj` (or any `.m1prj`) project file.
pub(super) fn is_m1prj(uri: &Url) -> bool {
    uri.path().ends_with(".m1prj")
}

/// Extract the M1 settings object from a client `initializationOptions` /
/// `didChangeConfiguration` payload: the `settings` sub-object if present (the
/// shape the extensions send), else the value itself (a bare
/// `{ lint, format, diagnostics }`). The result is deserialized by
/// [`crate::config::M1Config::resolve`].
pub(super) fn editor_settings(v: serde_json::Value) -> serde_json::Value {
    v.get("settings").cloned().unwrap_or(v)
}

/// The static set of LSP capabilities the server advertises in `initialize`.
/// `encoding` is the position encoding negotiated with the client; everything
/// else is fixed at build time.
pub(super) fn server_capabilities(encoding: PositionEncodingKind) -> ServerCapabilities {
    ServerCapabilities {
        position_encoding: Some(encoding),
        // willRenameFiles (#250): renaming a .m1scr in the explorer updates
        // the .m1prj mapping / runs the inverse group cascade.
        workspace: Some(WorkspaceServerCapabilities {
            workspace_folders: None,
            file_operations: Some(WorkspaceFileOperationsServerCapabilities {
                will_rename: Some(FileOperationRegistrationOptions {
                    filters: vec![FileOperationFilter {
                        scheme: Some("file".to_string()),
                        pattern: FileOperationPattern {
                            glob: "**/*.m1scr".to_string(),
                            matches: Some(FileOperationPatternKind::File),
                            options: None,
                        },
                    }],
                }),
                ..Default::default()
            }),
        }),
        // INCREMENTAL (#270): didChange arrives as ranged edits which the
        // Document applies via m1_core::Edit + Cst::reparse — tree reuse per
        // keystroke instead of a from-scratch parse.
        text_document_sync: Some(TextDocumentSyncCapability::Kind(
            TextDocumentSyncKind::INCREMENTAL,
        )),
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
            // Project-symbol documentation is filled in lazily via
            // completionItem/resolve (#267) to keep the list payload small.
            resolve_provider: Some(true),
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
                legend: crate::features::semantic_tokens::legend(),
                full: Some(SemanticTokensFullOptions::Delta { delta: Some(true) }),
                range: Some(true),
                work_done_progress_options: Default::default(),
            },
        )),
        ..Default::default()
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
