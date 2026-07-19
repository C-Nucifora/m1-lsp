//! Semantic-token handlers: full, full/delta (against the per-document snapshot
//! backing #231), and range. The result-id bookkeeping that lets the delta path
//! send only a splice when the client holds the previous snapshot.
use tower_lsp::jsonrpc::Result;
use tower_lsp::lsp_types::*;

use super::Backend;
use crate::features::semantic_tokens;

impl Backend {
    pub(super) async fn semantic_tokens_full_impl(
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

    pub(super) async fn semantic_tokens_full_delta_impl(
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

    pub(super) async fn semantic_tokens_range_impl(
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
}
