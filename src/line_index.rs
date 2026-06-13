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
#[derive(Clone)]
pub struct LineIndex {
    inner: m1_workspace::LineIndex,
    text: String,
}

impl LineIndex {
    pub fn new(text: &str) -> Self {
        Self {
            inner: m1_workspace::LineIndex::new(text),
            text: text.to_string(),
        }
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
