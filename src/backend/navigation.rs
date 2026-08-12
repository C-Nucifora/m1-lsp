//! Read-only navigation/query handlers: workspace + document symbols, the goto
//! family (definition / declaration / type-definition / implementation),
//! references, document highlights, document links, selection ranges, folding,
//! code lenses, and call hierarchy. Cursor-position resolution over the project
//! model — no state mutation.
use tower_lsp::jsonrpc::Result;
use tower_lsp::lsp_types::request::{GotoImplementationParams, GotoImplementationResponse};
use tower_lsp::lsp_types::*;

use super::Backend;
use crate::features::{
    call_hierarchy, code_lens, document_link, document_symbols, folding, goto, references,
    selection_range, workspace_symbol,
};

impl Backend {
    #[allow(deprecated)]
    pub(super) async fn symbol_impl(
        &self,
        params: WorkspaceSymbolParams,
    ) -> Result<Option<Vec<SymbolInformation>>> {
        let q = params.query;
        Ok(self
            .store
            .with_project(|p| p.map(|lp| workspace_symbol::workspace_symbols(lp, &q))))
    }

    pub(super) async fn goto_definition_impl(
        &self,
        params: GotoDefinitionParams,
    ) -> Result<Option<GotoDefinitionResponse>> {
        Ok(self
            .resolve_goto(&params.text_document_position_params)
            .map(GotoDefinitionResponse::Scalar))
    }

    /// textDocument/declaration: for project symbols this is the same `.m1prj`
    /// `<Component>` (or backing file) site as definition — the LSP-canonical home
    /// for the jump (#168). Declaration == definition here, so both share
    /// the private `resolve_goto` resolver.
    pub(super) async fn goto_declaration_impl(
        &self,
        params: request::GotoDeclarationParams,
    ) -> Result<Option<request::GotoDeclarationResponse>> {
        Ok(self
            .resolve_goto(&params.text_document_position_params)
            .map(request::GotoDeclarationResponse::Scalar))
    }

    /// textDocument/typeDefinition: from an enum-typed channel/parameter, jump to
    /// its `<Type>` block in the `.m1prj` (#168).
    pub(super) async fn goto_type_definition_impl(
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
    pub(super) async fn document_link_impl(
        &self,
        params: DocumentLinkParams,
    ) -> Result<Option<Vec<DocumentLink>>> {
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
    pub(super) async fn selection_range_impl(
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

    pub(super) async fn goto_implementation_impl(
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

    pub(super) async fn document_symbol_impl(
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

    pub(super) async fn code_lens_impl(
        &self,
        params: CodeLensParams,
    ) -> Result<Option<Vec<CodeLens>>> {
        let uri = params.text_document.uri;
        // The logging/security badges (#171/#172) resolve the channels the
        // script writes, which needs its text: prefer the open buffer — whose
        // incrementally-maintained CST (#270) is reused instead of re-parsing
        // the whole file per request (#343) — and fall back to disk.
        let doc = self.doc_context(&uri);
        let disk_text = if doc.is_none() {
            uri.to_file_path()
                .ok()
                .and_then(|p| crate::disk_read::read_disk(&p))
        } else {
            None
        };
        let text = doc.as_ref().map(|d| &*d.text).or(disk_text.as_deref());
        let cst = doc.as_ref().map(|d| d.cst.as_ref());
        Ok(self
            .store
            .with_project(|p| p.map(|lp| code_lens::code_lens(lp, &uri, text, cst))))
    }

    pub(super) async fn prepare_call_hierarchy_impl(
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

    pub(super) async fn incoming_calls_impl(
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

    pub(super) async fn outgoing_calls_impl(
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

    pub(super) async fn references_impl(
        &self,
        params: ReferenceParams,
    ) -> Result<Option<Vec<Location>>> {
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

    pub(super) async fn document_highlight_impl(
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

    pub(super) async fn folding_range_impl(
        &self,
        params: FoldingRangeParams,
    ) -> Result<Option<Vec<FoldingRange>>> {
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
}
