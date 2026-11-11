//! Decoupling request handlers from the tower-lsp serve loop (#336).
//!
//! tower-lsp's `Server::serve` runs three arms `join!`ed inside **one** task:
//! the stdin reader, the stdout writer, and a `buffer_unordered` window that
//! polls every handler future inline — handlers are never spawned. That
//! coupling has two failure modes:
//!
//! - Any `tokio::task::block_in_place` in a handler blocks the whole serve
//!   task for the duration of the blocking work: no requests are read, no
//!   responses are written, and no other in-flight handler makes progress.
//!   `block_in_place` only migrates *other tasks* off the thread — the serve
//!   loop's other arms are not other tasks, so it protected nothing here.
//! - `Handle::block_on(client.send_*())` inside such a section deadlocks
//!   permanently once the client channel (capacity 1) holds an undrained
//!   message: the send parks until the stdout arm drains it, and the stdout
//!   arm lives in the very task the handler is blocking. This was #336's
//!   stall — `workspace/diagnostic`'s scan reporting `$/progress` behind an
//!   undrained `WorkDoneProgress::Begin`.
//!
//! [`SpawnHandlers`] wraps the `LspService` so every handler future runs as
//! its own `tokio::spawn`ed task. The serve loop then only awaits join
//! handles: its reader/writer arms always run, concurrent requests genuinely
//! run concurrently, and the existing `block_in_place` sites do what their
//! comments always claimed (block one worker thread, not the server).

use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};

use tokio::sync::watch;
use tower_lsp::jsonrpc::{Request, Response};
use tower_service::Service;

/// The `Server::concurrency_level` m1-lsp serves with. tower-lsp's default of
/// 4 is small enough that a burst from one editor (workspace diagnostics +
/// semantic tokens + code lenses across two buffers is 8+ requests) fills the
/// window; handlers awaiting a client round-trip (`window/workDoneProgress/
/// create`, the refresh nudges) hold a slot for the round-trip's duration, so
/// give the window real headroom.
pub const CONCURRENCY_LEVEL: usize = 16;

/// Runs handlers independently while reserving mutation order at admission.
pub struct SpawnHandlers<S> {
    inner: S,
    tail: watch::Receiver<bool>,
}

struct Admission {
    predecessor: Option<watch::Receiver<bool>>,
    completion: Option<watch::Sender<bool>>,
}

tokio::task_local! {
    static ADMISSION: Admission;
}

async fn wait(mut predecessor: watch::Receiver<bool>) {
    while !*predecessor.borrow_and_update() {
        predecessor
            .changed()
            .await
            .expect("state mutation ended before committing");
    }
}

/// Called inside Backend's request future, so tower-lsp can cancel the wait.
pub(crate) async fn wait_for_prior() {
    if let Ok(Some(prior)) = ADMISSION.try_with(|a| a.predecessor.clone()) {
        wait(prior).await;
    }
}

/// Release the next mutation before diagnostics and client round trips.
pub(crate) fn state_committed() {
    let _ = ADMISSION.try_with(|a| {
        if let Some(done) = &a.completion {
            done.send_replace(true);
        }
    });
}

impl<S> SpawnHandlers<S> {
    pub fn new(inner: S) -> Self {
        let (_, tail) = watch::channel(true);
        Self { inner, tail }
    }
}

impl<S> Service<Request> for SpawnHandlers<S>
where
    S: Service<Request, Response = Option<Response>>,
    S::Future: Send + 'static,
    S::Error: Send + 'static,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, req: Request) -> Self::Future {
        let prior = self.tail.clone();
        let mutation = matches!(
            req.method(),
            "textDocument/didOpen"
                | "textDocument/didChange"
                | "textDocument/didSave"
                | "textDocument/didClose"
                | "workspace/didChangeWatchedFiles"
                | "workspace/didChangeConfiguration"
        );
        let completion = mutation.then(|| {
            let (done, tail) = watch::channel(false);
            self.tail = tail;
            done
        });
        // Preserve tower-lsp's synchronous admission and cancellation registration.
        let fut = self.inner.call(req);
        if let Some(done) = completion {
            // Admitted notifications must commit even if their outer future is dropped.
            // Invalid params and other rejected notifications still release their link
            // after the predecessor, without requiring a Backend callback.
            let task = tokio::spawn(async move {
                wait(prior).await;
                let result = ADMISSION
                    .scope(
                        Admission {
                            predecessor: None,
                            completion: Some(done.clone()),
                        },
                        fut,
                    )
                    .await;
                done.send_replace(true);
                result
            });
            Box::pin(async move { task.await.expect("state mutation task panicked") })
        } else {
            Box::pin(async move {
                tokio::spawn(ADMISSION.scope(
                    Admission {
                        predecessor: Some(prior),
                        completion: None,
                    },
                    fut,
                ))
                .await
                .expect("request handler task panicked")
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    struct HeldService {
        mutation: watch::Receiver<bool>,
        effects: watch::Receiver<bool>,
        log: Arc<Mutex<Vec<String>>>,
    }
    impl Service<Request> for HeldService {
        type Response = Option<Response>;
        type Error = std::convert::Infallible;
        type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;
        fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
            Poll::Ready(Ok(()))
        }
        fn call(&mut self, req: Request) -> Self::Future {
            let name = req.method().to_owned();
            let mutation = self.mutation.clone();
            let effects = self.effects.clone();
            let log = self.log.clone();
            Box::pin(async move {
                if name == "textDocument/didOpen" {
                    wait(mutation).await;
                    log.lock().unwrap().push("open".into());
                    state_committed();
                    wait(effects).await;
                } else if name == "textDocument/didChange" {
                    // Simulate tower rejecting parameters without entering Backend.
                } else if name == "textDocument/didSave" {
                    log.lock().unwrap().push("save".into());
                    state_committed();
                } else {
                    wait_for_prior().await;
                    log.lock().unwrap().push("request".into());
                }
                Ok(None)
            })
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn dropped_notifications_and_rejected_params_preserve_the_chain() {
        let (release, mutation) = watch::channel(false);
        let (finish, effects) = watch::channel(false);
        let log = Arc::new(Mutex::new(Vec::new()));
        let mut service = SpawnHandlers::new(HeldService {
            mutation,
            effects,
            log: log.clone(),
        });
        for method in [
            "textDocument/didOpen",
            "textDocument/didChange",
            "textDocument/didSave",
        ] {
            drop(service.call(Request::build(method).finish()));
        }
        let mut request =
            tokio::spawn(service.call(Request::build("textDocument/hover").id(1).finish()));
        assert!(
            tokio::time::timeout(Duration::from_millis(30), &mut request)
                .await
                .is_err()
        );
        assert!(log.lock().unwrap().is_empty());
        release.send_replace(true);
        tokio::time::timeout(Duration::from_secs(2), request)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(*log.lock().unwrap(), ["open", "save", "request"]);
        // didOpen's effects are still blocked; they did not hold the next mutation.
        finish.send_replace(true);
    }

    struct HeldBackend {
        release: Arc<tokio::sync::Notify>,
    }
    #[tower_lsp::async_trait]
    impl tower_lsp::LanguageServer for HeldBackend {
        async fn initialize(
            &self,
            _: tower_lsp::lsp_types::InitializeParams,
        ) -> tower_lsp::jsonrpc::Result<tower_lsp::lsp_types::InitializeResult> {
            Ok(tower_lsp::lsp_types::InitializeResult::default())
        }
        async fn shutdown(&self) -> tower_lsp::jsonrpc::Result<()> {
            Ok(())
        }
        async fn did_open(&self, _: tower_lsp::lsp_types::DidOpenTextDocumentParams) {
            self.release.notified().await;
            state_committed();
        }
        async fn hover(
            &self,
            _: tower_lsp::lsp_types::HoverParams,
        ) -> tower_lsp::jsonrpc::Result<Option<tower_lsp::lsp_types::Hover>> {
            wait_for_prior().await;
            Ok(None)
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn request_waiting_for_a_mutation_remains_cancellable() {
        let release = Arc::new(tokio::sync::Notify::new());
        let (inner, _socket) = tower_lsp::LspService::new(|_| HeldBackend {
            release: release.clone(),
        });
        let mut service = SpawnHandlers::new(inner);
        service
            .call(
                Request::build("initialize")
                    .id(1)
                    .params(json!({"capabilities":{}}))
                    .finish(),
            )
            .await
            .unwrap();
        drop(service.call(Request::build("textDocument/didOpen").params(json!({"textDocument":{"uri":"file:///held.m1scr","languageId":"m1","version":1,"text":""}})).finish()));
        let request = tokio::spawn(service.call(Request::build("textDocument/hover").id(2).params(json!({"textDocument":{"uri":"file:///held.m1scr"},"position":{"line":0,"character":0}})).finish()));
        service
            .call(
                Request::build("$/cancelRequest")
                    .params(json!({"id":2}))
                    .finish(),
            )
            .await
            .unwrap();
        let response = tokio::time::timeout(Duration::from_secs(2), request)
            .await
            .unwrap()
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(
            serde_json::to_value(response).unwrap()["error"]["code"],
            -32800
        );
        release.notify_one();
    }
}
