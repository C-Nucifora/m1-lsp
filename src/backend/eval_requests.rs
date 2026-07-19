//! The two eval-aware handlers: `textDocument/hover` and `textDocument/inlayHint`.
//! Both are byte-identical to the pre-eval behaviour when eval is disabled (the
//! default); when enabled they read the cached [`Trace`](crate::eval::Trace) via
//! `with_eval` and enrich the result with computed values, surfacing any
//! fail-loud source fallback once per rebuild as a window warning (2026-07-19 B3).
use tower_lsp::jsonrpc::Result;
use tower_lsp::lsp_types::*;

use super::Backend;
use crate::features::{hover, inlay};

impl Backend {
    pub(super) async fn hover_impl(&self, params: HoverParams) -> Result<Option<Hover>> {
        let tdp = params.text_document_position_params;
        let Some(doc) = self.doc_context(&tdp.text_document.uri) else {
            return Ok(None);
        };
        let byte = doc.byte(tdp.position);
        let cst = doc.parse();

        // Eval is opt-in and off by default. When disabled, take the plain
        // `hover` path — byte-identical to before the eval integration (no engine
        // is ever built and no trace is consulted). When enabled, read the cached
        // `Trace` via `with_eval` (built once per project/config; never per hover)
        // and enrich a channel hover with its evaluated value.
        let eval_cfg = self.eval_config.read().unwrap().clone();
        if !eval_cfg.enabled {
            return Ok(self.store.with_project(|p| {
                hover::hover(
                    cst.root(),
                    byte,
                    p.map(|lp| &lp.project),
                    doc.file_name.as_deref(),
                    &doc.line_index,
                    doc.enc,
                )
            }));
        }

        // Expression-level hover (E5) keys `Trace::exprs` by the *saved* script's
        // byte offsets; those only line up with the open buffer when it is
        // unmodified-since-load, so gate it on a buffer == disk check. Channel hover
        // (E4) is path-keyed and works regardless.
        let expr_offsets_valid = self.buffer_matches_disk(&tdp.text_document.uri, &doc.text);

        // The trace build (a whole-project offline run on a cache miss) can be
        // non-trivial, so run it off the async worker via `block_in_place` (#135),
        // mirroring the call-graph/diagnostic read paths. Subsequent hovers hit the
        // cache and do no work.
        let mut fallback_issues: Vec<String> = Vec::new();
        let result = tokio::task::block_in_place(|| {
            self.store.with_eval(
                &eval_cfg,
                |lp| crate::eval::evaluate(lp, &eval_cfg),
                |opt, issues| {
                    fallback_issues.extend(issues.iter().cloned());
                    match opt {
                        Some((lp, trace, provenance)) => hover::hover_with_eval(
                            cst.root(),
                            byte,
                            Some(&lp.project),
                            doc.file_name.as_deref(),
                            &doc.line_index,
                            doc.enc,
                            Some(hover::EvalContext {
                                trace: trace.as_ref(),
                                provenance,
                                tick: eval_cfg.tick,
                                expr_offsets_valid,
                            }),
                        ),
                        // No project loaded: no symbols to resolve, no trace — fall
                        // back to the plain (project-less) hover for keyword/local
                        // docs.
                        None => hover::hover(
                            cst.root(),
                            byte,
                            None,
                            doc.file_name.as_deref(),
                            &doc.line_index,
                            doc.enc,
                        ),
                    }
                },
            )
        });
        // A configured scenario/log that failed loud (and fell back to the
        // offline default) must be USER-VISIBLE, not a silent downgrade: surface
        // each rebuild's issues once as a window warning (2026-07-19 review B3).
        self.notify_eval_issues(fallback_issues).await;
        Ok(result)
    }

    pub(super) async fn inlay_hint_impl(
        &self,
        params: InlayHintParams,
    ) -> Result<Option<Vec<InlayHint>>> {
        let Some(doc) = self.doc_context(&params.text_document.uri) else {
            return Ok(None);
        };
        let cst = doc.parse();

        // Inline computed-value hints (E6) are opt-in: they require eval enabled
        // *and* `inlay_values` on. Otherwise take the plain path — byte-identical
        // to before the eval integration (no trace is ever consulted), so the
        // existing type/unit/param hints are exactly as today.
        let eval_cfg = self.eval_config.read().unwrap().clone();
        if !eval_cfg.enabled || !eval_cfg.inlay_values {
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
            return Ok(Some(hints));
        }

        // Value hints on: read the cached `Trace` via `with_eval` (built once per
        // project/config; never per request) and add `= value` hints. The build (a
        // whole-project offline run on a cache miss) can be non-trivial, so run it
        // off the async worker via `block_in_place`, mirroring the hover read path.
        let mut fallback_issues: Vec<String> = Vec::new();
        let hints = tokio::task::block_in_place(|| {
            self.store.with_eval(
                &eval_cfg,
                |lp| crate::eval::evaluate(lp, &eval_cfg),
                |opt, issues| {
                    fallback_issues.extend(issues.iter().cloned());
                    match opt {
                        Some((lp, trace, provenance)) => inlay::inlay_hints_with_eval(
                            cst.root(),
                            params.range,
                            &doc.line_index,
                            doc.enc,
                            Some(&lp.project),
                            doc.file_name.as_deref(),
                            Some(inlay::EvalInlayContext {
                                trace: trace.as_ref(),
                                provenance,
                                tick: eval_cfg.tick,
                            }),
                        ),
                        // No project loaded: no symbols to resolve, no trace — fall
                        // back to the plain (project-less) hints.
                        None => inlay::inlay_hints(
                            cst.root(),
                            params.range,
                            &doc.line_index,
                            doc.enc,
                            None,
                            doc.file_name.as_deref(),
                        ),
                    }
                },
            )
        });
        // Surface a configured source's fail-loud fallback once per rebuild —
        // never silently (2026-07-19 review B3).
        self.notify_eval_issues(fallback_issues).await;
        Ok(Some(hints))
    }

    /// Surface eval-source fallback issues as a user-visible window WARNING —
    /// e.g. "configured scenario failed to parse; using the offline default".
    /// Called with the fresh issues of exactly the request that rebuilt the
    /// cached trace ([`crate::project_store::ProjectStore::with_eval`]), so the
    /// user is warned once per rebuild, not once per hover — and never silently
    /// downgraded from a configured scenario/log to offline defaults.
    async fn notify_eval_issues(&self, issues: Vec<String>) {
        for line in issues {
            self.client
                .show_message(MessageType::WARNING, format!("m1-lsp eval: {line}"))
                .await;
        }
    }
}
