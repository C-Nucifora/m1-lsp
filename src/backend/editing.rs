//! Mutating / assist handlers: document + range + on-type formatting, completion
//! (list + resolve), signature help, the rename family (prepare / rename /
//! will-rename-files, with the post-rename project refresh), and code actions
//! (quick-fixes, fix-all, refactors, format actions).
use tower_lsp::jsonrpc::{Error, Result};
use tower_lsp::lsp_types::*;

use super::Backend;
use super::capabilities::is_m1prj;
use crate::features::{code_action, completion, rename, signature_help};
use crate::format::{format_edits, range_format_edits};

impl Backend {
    pub(super) async fn formatting_impl(
        &self,
        params: DocumentFormattingParams,
    ) -> Result<Option<Vec<TextEdit>>> {
        let uri = params.text_document.uri;
        Ok(self
            .docs
            .get(&uri)
            .and_then(|doc| format_edits(&doc, self.enc(), self.formatter.as_ref())))
    }

    pub(super) async fn range_formatting_impl(
        &self,
        params: DocumentRangeFormattingParams,
    ) -> Result<Option<Vec<TextEdit>>> {
        let uri = params.text_document.uri;
        Ok(self
            .docs
            .get(&uri)
            .and_then(|doc| range_format_edits(&doc, params.range, self.formatter.as_ref())))
    }

    pub(super) async fn on_type_formatting_impl(
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

    pub(super) async fn completion_impl(
        &self,
        params: CompletionParams,
    ) -> Result<Option<CompletionResponse>> {
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

    pub(super) async fn completion_resolve_impl(
        &self,
        mut item: CompletionItem,
    ) -> Result<CompletionItem> {
        self.store
            .with_project(|p| completion::resolve_item(&mut item, p));
        Ok(item)
    }

    pub(super) async fn signature_help_impl(
        &self,
        params: SignatureHelpParams,
    ) -> Result<Option<SignatureHelp>> {
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

    pub(super) async fn prepare_rename_impl(
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

    pub(super) async fn will_rename_files_impl(
        &self,
        params: RenameFilesParams,
    ) -> Result<Option<WorkspaceEdit>> {
        // Renaming a `.m1scr` in the explorer is the inverse gesture of a
        // symbol rename (#250): update the explicit `Filename=` attribute, or
        // run the group cascade when the new basename implies a different
        // group segment. Convention-breaking renames get a warning instead of
        // silently dangling references.
        let enc = *self.encoding.read().unwrap();
        let open_text = |u: &Url| self.docs.get(u).map(|d| d.text.clone());
        let mut all_ops: Vec<DocumentChangeOperation> = Vec::new();
        let mut warnings: Vec<String> = Vec::new();
        tokio::task::block_in_place(|| {
            self.store.with_project(|p| {
                let Some(lp) = p else { return };
                for f in &params.files {
                    let (Ok(old_uri), Ok(new_uri)) =
                        (Url::parse(&f.old_uri), Url::parse(&f.new_uri))
                    else {
                        continue;
                    };
                    match rename::execute_file_rename(&old_uri, &new_uri, enc, lp, &open_text) {
                        Ok(Some(edit)) => {
                            if let Some(DocumentChanges::Operations(ops)) = edit.document_changes {
                                all_ops.extend(ops);
                            }
                        }
                        Ok(None) => {}
                        Err(msg) => warnings.push(msg),
                    }
                }
            })
        });
        for msg in warnings {
            self.client
                .show_message(MessageType::WARNING, format!("m1-lsp: {msg}"))
                .await;
        }
        if all_ops.is_empty() {
            return Ok(None);
        }
        Ok(Some(WorkspaceEdit {
            document_changes: Some(DocumentChanges::Operations(all_ops)),
            ..Default::default()
        }))
    }

    pub(super) async fn rename_impl(&self, params: RenameParams) -> Result<Option<WorkspaceEdit>> {
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
        // Project-wide rename reads + parses every script — seconds of silent
        // wall-clock on a real corpus; a begin/end pair is enough (#266).
        let progress = self.progress_begin("rename", "m1-lsp: renaming").await;
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
        self.progress_end(progress).await;
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

    pub(super) async fn code_action_impl(
        &self,
        params: CodeActionParams,
    ) -> Result<Option<CodeActionResponse>> {
        let uri = params.text_document.uri;
        // Single lookup: fetch the document once and derive everything from it.
        // A second `docs.get` for the format-action block would silently drop
        // "Format Document"/"Format Selection" if the document was closed or
        // replaced between the two calls (#287).
        let Some(raw_doc) = self.docs.get(&uri) else {
            return Ok(None);
        };
        let enc = self.enc();
        let text = raw_doc.text.clone();
        let line_index = raw_doc.line_index.clone();
        // Compute format edits while still holding the guard so the text and
        // line-index used here are coherent with the rest of this request.
        let fmt_doc_edits = format_edits(&raw_doc, enc, self.formatter.as_ref());
        let fmt_sel_edits = if params.range.start.line < params.range.end.line {
            range_format_edits(&raw_doc, params.range, self.formatter.as_ref())
        } else {
            None
        };
        drop(raw_doc);

        // The project model backs the T020 "did you mean" enum-member fix (#159).
        let mut actions = self.store.with_project(|p| {
            code_action::code_actions(
                &text,
                &line_index,
                enc,
                &uri,
                &params.context.diagnostics,
                p.map(|lp| &lp.project),
            )
        });
        // Whole-file "fix all auto-fixable lint issues" via the shared m1-lint
        // fixer — covers every fixable rule (L003/L007/L011/L018…), not just the
        // hand-ported few (#158).
        if let Some(fixed) = self.lint.fix(&text)
            && fixed != text
        {
            actions.push(code_action::fix_all_lint_action(
                &uri,
                &text,
                &line_index,
                enc,
                fixed,
            ));
        }
        // Selection-driven refactors, offered independently of diagnostics (#174):
        // "Extract to local" on a selected expression, "Inline local" on a local.
        actions.extend(code_action::refactors(
            &text,
            &line_index,
            enc,
            &uri,
            params.range,
        ));
        // Source-level format actions, offered independently of diagnostics (#161)
        // so the menu can format clean code. "Format Document" appears when
        // formatting would change the file; "Format Selection" when the request
        // range spans more than one line.
        if let Some(edits) = fmt_doc_edits {
            actions.push(code_action::format_action("Format Document", &uri, edits));
        }
        if let Some(edits) = fmt_sel_edits {
            actions.push(code_action::format_action("Format Selection", &uri, edits));
        }
        Ok((!actions.is_empty()).then_some(actions))
    }
}

#[cfg(test)]
mod code_action_format_tests {
    use crate::analysis::{NoLint, NoTypes};
    use crate::backend::Backend;
    use crate::format::Formatter;
    use crate::project_store::ProjectStore;
    use std::sync::Arc;
    use tower_lsp::{LanguageServer, LspService, lsp_types::*};

    // A trivial formatter that appends a newline so it always produces a
    // change, making `format_edits` return `Some(edits)` in every test run.
    struct AlwaysAddsNewline;
    impl Formatter for AlwaysAddsNewline {
        fn format(&self, src: &str) -> Option<String> {
            Some(format!("{src}\n"))
        }
    }

    // Regression guard for #287: the `code_action` handler previously called
    // `docs.get(&uri)` a second time for the format-action block. If the
    // document was closed or replaced between the two lookups the format
    // actions ("Format Document" / "Format Selection") silently disappeared
    // from the response. The fix fetches the document once and reuses it.
    //
    // This test opens a document and requests code actions from a backend
    // configured with a formatter that always produces a change. It asserts
    // that "Format Document" is present in the response, confirming the
    // format-action path reached a live document.
    //
    // `code_action` indirectly triggers `block_in_place` through the LSP
    // client, so the test needs the multi-thread runtime.
    #[tokio::test(flavor = "multi_thread")]
    async fn format_document_action_present_when_doc_is_open() {
        let (service, _socket) = LspService::new(|client| {
            Backend::with_backends(
                client,
                Box::new(NoLint),
                Box::new(NoTypes),
                Box::new(AlwaysAddsNewline),
                Arc::new(ProjectStore::new()),
            )
        });
        let backend = service.inner();
        let uri = Url::parse("file:///test.m1scr").unwrap();

        // Open the document so the handler finds it via docs.get.
        backend
            .did_open(DidOpenTextDocumentParams {
                text_document: TextDocumentItem {
                    uri: uri.clone(),
                    language_id: "m1scr".to_owned(),
                    version: 1,
                    text: "x = 1\n".to_owned(),
                },
            })
            .await;

        let result = backend
            .code_action(CodeActionParams {
                text_document: TextDocumentIdentifier { uri: uri.clone() },
                range: Range::new(Position::new(0, 0), Position::new(0, 0)),
                context: CodeActionContext {
                    diagnostics: vec![],
                    only: None,
                    trigger_kind: None,
                },
                work_done_progress_params: WorkDoneProgressParams::default(),
                partial_result_params: PartialResultParams::default(),
            })
            .await
            .expect("code_action must not error");

        let actions = result.expect("code_action must return Some when doc is open");

        let has_format_document = actions.iter().any(
            |a| matches!(a, CodeActionOrCommand::CodeAction(ca) if ca.title == "Format Document"),
        );
        assert!(
            has_format_document,
            "\"Format Document\" must appear in code actions when the doc is open and \
             the formatter produces a change; got: {actions:?}"
        );
    }
}
