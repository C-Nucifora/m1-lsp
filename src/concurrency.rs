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

use tower_lsp::jsonrpc::{Request, Response};
use tower_service::Service;

/// The `Server::concurrency_level` m1-lsp serves with. tower-lsp's default of
/// 4 is small enough that a burst from one editor (workspace diagnostics +
/// semantic tokens + code lenses across two buffers is 8+ requests) fills the
/// window; handlers awaiting a client round-trip (`window/workDoneProgress/
/// create`, the refresh nudges) hold a slot for the round-trip's duration, so
/// give the window real headroom.
pub const CONCURRENCY_LEVEL: usize = 16;

/// Runs each handler future as its own tokio task (see the module docs).
pub struct SpawnHandlers<S> {
    inner: S,
}

impl<S> SpawnHandlers<S> {
    pub fn new(inner: S) -> Self {
        Self { inner }
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
        let fut = self.inner.call(req);
        // A panic inside a handler re-panics here, matching the pre-wrapper
        // behaviour (an unwinding handler took down the serve loop).
        Box::pin(async move {
            tokio::spawn(fut)
                .await
                .expect("request handler task panicked")
        })
    }
}
