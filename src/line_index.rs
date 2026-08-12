//! Byte-offset <-> LSP position conversion — a thin adapter over
//! `m1_workspace::LineIndex` (#265).
//!
//! The encoding-aware conversions (UTF-16/UTF-8 code-unit columns, the
//! mid-codepoint clamping that guards against the #132 DoS) used to live here
//! as a divergent copy; they were hoisted into m1-workspace
//! (`LineIndex::position_in` / `offset_in`, tests included) so a fix lands in
//! one place. What remains here is only the LSP-type surface: the workspace
//! index is text-free, so this wrapper pairs it with the document text and
//! speaks `tower_lsp::lsp_types::Position`.
use tower_lsp::lsp_types::Position;

pub use m1_workspace::PositionEncoding;

/// Byte offsets of each line start in a document, plus the text itself.
///
/// The text is held as an `Arc<str>` (#344): construction copies the document
/// once, but every clone after that — `doc_context` clones the index per
/// request, `diagnostics_for` clones it per pass — is a pointer bump instead
/// of a full-buffer copy, and [`Self::shared_text`] lets those same callers
/// share the text itself rather than cloning the `String` alongside.
#[derive(Clone)]
pub struct LineIndex {
    inner: m1_workspace::LineIndex,
    text: std::sync::Arc<str>,
}

impl LineIndex {
    pub fn new(text: &str) -> Self {
        Self {
            inner: m1_workspace::LineIndex::new(text),
            text: std::sync::Arc::from(text),
        }
    }

    /// The indexed document text, shared — a pointer clone, not a copy. The
    /// text a `LineIndex` was built from is exactly the text its positions
    /// resolve against, so callers that carry (text + index) pairs can hold
    /// this instead of a second `String` copy of the buffer (#344).
    pub fn shared_text(&self) -> std::sync::Arc<str> {
        self.text.clone()
    }

    pub fn position(&self, byte: usize, enc: PositionEncoding) -> Position {
        let (line, col) = self.inner.position_in(&self.text, byte, enc);
        Position::new(line as u32, col as u32)
    }

    /// Byte offset of `pos`, computed against this index's own text — mirrors
    /// [`position`](Self::position), which likewise takes no text argument.
    pub fn offset(&self, pos: Position, enc: PositionEncoding) -> usize {
        self.inner
            .offset_in(&self.text, pos.line as usize, pos.character as usize, enc)
    }
}
