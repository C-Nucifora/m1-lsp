//! `$/progress` reporting for the long operations (workspace diagnostics over a
//! real corpus, project-wide rename, #266). Every call is a no-op when the client
//! did not advertise `window.workDoneProgress`, so call sites stay branch-free.
use tower_lsp::lsp_types::*;

use super::Backend;

/// tower-lsp 0.20 panics on a reply whose waiter was dropped. Keep optional
/// requests alive after timeout/cancellation, but bound those waiting tasks.
pub(super) async fn optional_request<R>(
    client: tower_lsp::Client,
    slots: std::sync::Arc<tokio::sync::Semaphore>,
    params: R::Params,
) -> Option<R::Result>
where
    R: tower_lsp::lsp_types::request::Request + 'static,
    R::Params: Send + 'static,
    R::Result: Send + 'static,
{
    let permit = slots.try_acquire_owned().ok()?;
    let mut pending = tokio::spawn(async move {
        let _permit = permit;
        client.send_request::<R>(params).await
    });
    tokio::time::timeout(std::time::Duration::from_secs(2), &mut pending)
        .await
        .ok()?
        .ok()?
        .ok()
}

/// At most one pending request per method and one dispatcher waiting for capacity.
#[derive(Default)]
pub(super) struct PendingRequests {
    requests: std::collections::VecDeque<DeferredRequest>,
    running: bool,
}

pub(super) enum DeferredRequest {
    Diagnostics,
    InlayHints,
    SemanticTokens,
    CodeLenses,
    Watch(RegistrationParams),
}

impl DeferredRequest {
    fn key(&self) -> u8 {
        match self {
            Self::Diagnostics => 0,
            Self::InlayHints => 1,
            Self::SemanticTokens => 2,
            Self::CodeLenses => 3,
            Self::Watch(_) => 4,
        }
    }

    async fn send(self, client: &tower_lsp::Client) {
        match self {
            Self::Diagnostics => {
                let _ = client
                    .send_request::<request::WorkspaceDiagnosticRefresh>(())
                    .await;
            }
            Self::InlayHints => {
                let _ = client
                    .send_request::<request::InlayHintRefreshRequest>(())
                    .await;
            }
            Self::SemanticTokens => {
                let _ = client
                    .send_request::<request::SemanticTokensRefresh>(())
                    .await;
            }
            Self::CodeLenses => {
                let _ = client.send_request::<request::CodeLensRefresh>(()).await;
            }
            Self::Watch(params) => {
                let _ = client
                    .send_request::<request::RegisterCapability>(params)
                    .await;
            }
        }
    }
}

impl Backend {
    pub(super) fn queue_client_request(&self, request: DeferredRequest) {
        let mut pending = self.pending_requests.lock().unwrap();
        if let Some(existing) = pending
            .requests
            .iter_mut()
            .find(|p| p.key() == request.key())
        {
            *existing = request;
        } else {
            pending.requests.push_back(request);
        }
        if pending.running {
            return;
        }
        pending.running = true;
        drop(pending);
        let queue = self.pending_requests.clone();
        let client = self.client.clone();
        let slots = self.optional_requests.clone();
        tokio::spawn(async move {
            loop {
                {
                    let mut pending = queue.lock().unwrap();
                    if pending.requests.is_empty() {
                        pending.running = false;
                        return;
                    }
                }
                let Ok(permit) = slots.clone().acquire_owned().await else {
                    return;
                };
                // Keep intent queued while waiting, so intervening changes coalesce.
                let request = queue.lock().unwrap().requests.pop_front().unwrap();
                let client = client.clone();
                tokio::spawn(async move {
                    let _permit = permit;
                    // Retain the tower response receiver even for late replies.
                    request.send(&client).await;
                });
            }
        });
    }

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
        let sequence = self
            .progress_seq
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let token = NumberOrString::String(format!("m1-lsp/{id}/{sequence}"));
        optional_request::<tower_lsp::lsp_types::request::WorkDoneProgressCreate>(
            self.client.clone(),
            self.optional_requests.clone(),
            WorkDoneProgressCreateParams {
                token: token.clone(),
            },
        )
        .await?;
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
