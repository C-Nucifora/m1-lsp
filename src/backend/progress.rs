//! `$/progress` reporting for the long operations (workspace diagnostics over a
//! real corpus, project-wide rename, #266). Every call is a no-op when the client
//! did not advertise `window.workDoneProgress`, so call sites stay branch-free.
use tower_lsp::lsp_types::*;

use super::Backend;

impl Backend {
    /// Create a `$/progress` token and send `Begin` (#266). Returns `None`
    /// (and sends nothing) when the client did not advertise
    /// `window.workDoneProgress` — every later call is then a no-op, so call
    /// sites stay branch-free.
    pub(super) async fn progress_begin(&self, id: &str, title: &str) -> Option<NumberOrString> {
        if !self
            .progress_support
            .load(std::sync::atomic::Ordering::Relaxed)
        {
            return None;
        }
        let token = NumberOrString::String(format!("m1-lsp/{id}"));
        if self
            .client
            .send_request::<tower_lsp::lsp_types::request::WorkDoneProgressCreate>(
                WorkDoneProgressCreateParams {
                    token: token.clone(),
                },
            )
            .await
            .is_err()
        {
            return None;
        }
        self.send_progress(
            &token,
            WorkDoneProgress::Begin(WorkDoneProgressBegin {
                title: title.to_string(),
                ..Default::default()
            }),
        )
        .await;
        Some(token)
    }

    pub(super) async fn progress_report(&self, token: &Option<NumberOrString>, message: String) {
        if let Some(t) = token {
            self.send_progress(
                t,
                WorkDoneProgress::Report(WorkDoneProgressReport {
                    message: Some(message),
                    ..Default::default()
                }),
            )
            .await;
        }
    }

    pub(super) async fn progress_end(&self, token: Option<NumberOrString>) {
        if let Some(t) = token {
            self.send_progress(&t, WorkDoneProgress::End(Default::default()))
                .await;
        }
    }

    async fn send_progress(&self, token: &NumberOrString, value: WorkDoneProgress) {
        self.client
            .send_notification::<tower_lsp::lsp_types::notification::Progress>(ProgressParams {
                token: token.clone(),
                value: ProgressParamsValue::WorkDone(value),
            })
            .await;
    }
}
