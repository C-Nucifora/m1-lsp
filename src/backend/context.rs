//! Per-request document plumbing shared by every cursor-position handler: the
//! [`DocContext`] snapshot (text + line index + encoding + basename + CST), the
//! negotiated position encoding, semantic-token id minting, the buffer-vs-disk
//! gate for expression-level hover, and the shared goto resolver.
use tower_lsp::lsp_types::{Location, TextDocumentPositionParams, Url};

use super::Backend;
use crate::features::goto;
use crate::line_index::PositionEncoding;

/// Everything a request handler needs about one open document, gathered once: the
/// cloned text + line index (released from the `DashMap` guard), the negotiated
/// position encoding, and the file basename used for group-relative resolution.
/// Replaces the get-doc / `enc()` / byte-offset / `file_name` plumbing that every
/// cursor-position handler repeated. The CST is parsed by the caller via
/// [`DocContext::parse`] — a `Node` borrows the tree, which must outlive the borrow.
pub(super) struct DocContext {
    /// The document text, shared with `line_index` (a pointer clone of the
    /// same buffer, #344) — previously a second full `String` copy per request.
    pub(super) text: std::sync::Arc<str>,
    pub(super) line_index: crate::line_index::LineIndex,
    pub(super) enc: PositionEncoding,
    pub(super) file_name: Option<String>,
    /// The document's incrementally-maintained tree (#270). Shared, not
    /// re-parsed: `parse()` is a pointer clone.
    pub(super) cst: std::sync::Arc<m1_core::Cst>,
}

impl DocContext {
    /// Byte offset of an LSP `position` within this document.
    pub(super) fn byte(&self, position: tower_lsp::lsp_types::Position) -> usize {
        self.line_index.offset(position, self.enc)
    }

    /// Parse the document text into a CST. The caller holds the returned `Cst` so
    /// `Node`s borrowed from it stay valid.
    pub(super) fn parse(&self) -> std::sync::Arc<m1_core::Cst> {
        self.cst.clone()
    }
}

impl Backend {
    pub(super) fn enc(&self) -> PositionEncoding {
        *self.encoding.read().unwrap()
    }

    /// Next semantic-token `result_id` (#231).
    pub(super) fn next_semtok_id(&self) -> String {
        self.semtok_seq
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            .to_string()
    }

    /// Bundle an open document's text / line index / encoding / basename for a
    /// request handler ([`DocContext`]). `None` when the document isn't open — the
    /// caller returns its empty response.
    pub(super) fn doc_context(&self, uri: &Url) -> Option<DocContext> {
        let doc = self.docs.get(uri)?;
        Some(DocContext {
            // The index was built from exactly this document's text, so share
            // its buffer instead of copying the `String` again (#344).
            text: doc.line_index.shared_text(),
            line_index: doc.line_index.clone(),
            enc: self.enc(),
            file_name: crate::features::locate::file_name_of(uri),
            cst: doc.cst.clone(),
        })
    }

    /// Whether the open buffer at `uri` is byte-for-byte the file currently on
    /// disk — the gate for expression-level hover (E5).
    ///
    /// [`m1_eval::Trace::exprs`] is keyed by `(script_name, byte_offset)`, and those
    /// offsets are the evaluator's view of the **saved** script (the project is
    /// loaded from disk). An unsaved edit shifts the buffer's offsets relative to
    /// the saved file, so an expr lookup keyed on a buffer offset would mis-key; we
    /// only allow it when buffer == disk. Channel hover (E4) is path-keyed and never
    /// depends on this. The disk read uses the same tolerant (UTF-8 → Windows-1252)
    /// decode the project loader uses, so a Windows-1252 script compares correctly.
    /// A missing/unreadable file or a non-file URI conservatively returns `false`
    /// (expr-hover off rather than risk drifted offsets).
    pub(super) fn buffer_matches_disk(&self, uri: &Url, buffer_text: &str) -> bool {
        let Ok(path) = uri.to_file_path() else {
            return false;
        };
        crate::disk_read::read_disk(&path).is_some_and(|disk| disk == buffer_text)
    }

    /// Resolve the goto target at a cursor position, shared by
    /// `textDocument/definition` and `textDocument/declaration` (declaration ==
    /// definition for M1 symbols, #168). Project symbols
    /// (channels/params/functions/DBC) resolve via the project; a bare `local`
    /// resolves in-file and works even with no project loaded (#141). `None` when
    /// the document isn't open or nothing resolves.
    pub(super) fn resolve_goto(&self, tdp: &TextDocumentPositionParams) -> Option<Location> {
        let uri = &tdp.text_document.uri;
        let doc = self.doc_context(uri)?;
        let byte = doc.byte(tdp.position);
        let cst = doc.parse();
        self.store
            .with_project(|p| {
                p.and_then(|lp| goto::goto(cst.root(), byte, lp, doc.file_name.as_deref()))
            })
            .or_else(|| goto::goto_local(cst.root(), byte, uri, &doc.line_index, doc.enc))
    }
}

#[cfg(test)]
mod buffer_matches_disk_tests {
    use crate::backend::Backend;
    use tower_lsp::{LspService, lsp_types::Url};

    // E5 gate: expression-level hover keys `Trace::exprs` by the *saved* script's
    // byte offsets, so it is only safe when the open buffer equals the file on
    // disk. `buffer_matches_disk` is that gate.
    #[test]
    fn matches_when_buffer_equals_disk_and_not_otherwise() {
        let (service, _socket) = LspService::new(Backend::new);
        let backend = service.inner();

        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("Demo.Update.m1scr");
        let on_disk = "Output = 1;\n";
        std::fs::write(&path, on_disk).unwrap();
        let uri = Url::from_file_path(&path).unwrap();

        // Buffer identical to disk: the saved offsets line up.
        assert!(
            backend.buffer_matches_disk(&uri, on_disk),
            "an unmodified buffer matches its on-disk file"
        );
        // An edited (unsaved) buffer drifts the offsets — gate must be false.
        assert!(
            !backend.buffer_matches_disk(&uri, "Output = 999;\n"),
            "a modified buffer must not match disk"
        );
    }

    #[test]
    fn false_for_missing_file_or_non_file_uri() {
        let (service, _socket) = LspService::new(Backend::new);
        let backend = service.inner();

        // A path with no file on disk: read fails, gate is conservatively false.
        let tmp = tempfile::tempdir().unwrap();
        let missing = Url::from_file_path(tmp.path().join("nope.m1scr")).unwrap();
        assert!(
            !backend.buffer_matches_disk(&missing, "anything"),
            "a missing file yields no match (expr-hover off, not a drifted lookup)"
        );

        // A non-file URI cannot be read from disk at all.
        let non_file = Url::parse("untitled:Untitled-1").unwrap();
        assert!(
            !backend.buffer_matches_disk(&non_file, "anything"),
            "a non-file URI cannot match a disk file"
        );
    }
}
