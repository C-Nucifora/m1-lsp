//! Byte-offset <-> LSP position conversion, encoding-aware.
use tower_lsp::lsp_types::Position;

/// LSP position encoding negotiated with the client.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PositionEncoding {
    Utf16,
    Utf8,
}

/// Byte offsets of each line start in a document.
#[derive(Clone)]
pub struct LineIndex {
    line_starts: Vec<usize>,
    text: String,
}

impl LineIndex {
    pub fn new(text: &str) -> Self {
        let mut line_starts = vec![0usize];
        for (i, b) in text.bytes().enumerate() {
            if b == b'\n' {
                line_starts.push(i + 1);
            }
        }
        Self {
            line_starts,
            text: text.to_string(),
        }
    }

    fn line_of(&self, byte: usize) -> usize {
        match self.line_starts.binary_search(&byte) {
            Ok(line) => line,
            Err(next) => next - 1,
        }
    }

    pub fn position(&self, byte: usize, enc: PositionEncoding) -> Position {
        let mut byte = byte.min(self.text.len());
        // Round down to a UTF-8 char boundary. A diagnostic byte range can end
        // mid-codepoint (e.g. an unterminated string/comment containing a
        // multibyte char like `é`/`°`/an emoji); slicing `text[..byte]` at a
        // non-boundary would panic and, propagating out of the publish future,
        // abort the entire language server (#132). Line starts are always
        // boundaries (they follow `\n`), so only the upper bound needs flooring.
        while byte > 0 && !self.text.is_char_boundary(byte) {
            byte -= 1;
        }
        let line = self.line_of(byte);
        let line_start = self.line_starts[line];
        let slice = &self.text[line_start..byte];
        let col = match enc {
            PositionEncoding::Utf8 => slice.len() as u32,
            PositionEncoding::Utf16 => slice.chars().map(|c| c.len_utf16() as u32).sum(),
        };
        Position::new(line as u32, col)
    }

    pub fn offset(&self, pos: Position, _text: &str, enc: PositionEncoding) -> usize {
        let line = pos.line as usize;
        if line >= self.line_starts.len() {
            return self.text.len();
        }
        let line_start = self.line_starts[line];
        let line_end = self
            .line_starts
            .get(line + 1)
            .copied()
            .unwrap_or(self.text.len());
        let line_text = &self.text[line_start..line_end];
        let mut col = pos.character;
        let mut off = line_start;
        for c in line_text.chars() {
            if col == 0 {
                break;
            }
            let units = match enc {
                PositionEncoding::Utf8 => c.len_utf8() as u32,
                PositionEncoding::Utf16 => c.len_utf16() as u32,
            };
            if units > col {
                break;
            }
            col -= units;
            off += c.len_utf8();
        }
        off
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn idx(s: &str) -> LineIndex {
        LineIndex::new(s)
    }

    #[test]
    fn ascii_offsets_map_to_positions() {
        let s = "ab\ncde\n";
        let li = idx(s);
        // 'a'@0 -> 0:0 ; 'c'@3 -> 1:0 ; 'e'@5 -> 1:2 ; end@7 -> 2:0
        assert_eq!(li.position(0, PositionEncoding::Utf16), Position::new(0, 0));
        assert_eq!(li.position(3, PositionEncoding::Utf16), Position::new(1, 0));
        assert_eq!(li.position(5, PositionEncoding::Utf16), Position::new(1, 2));
        assert_eq!(li.position(7, PositionEncoding::Utf16), Position::new(2, 0));
    }

    #[test]
    fn utf16_counts_code_units_not_bytes() {
        // "é" is 2 bytes in UTF-8, 1 UTF-16 code unit. "𝄞" is 4 bytes, 2 units.
        let s = "é𝄞x"; // bytes: 2 + 4 + 1 = 7
        let li = idx(s);
        // byte 7 (end) -> column 1(é)+2(𝄞)+1(x) = 4 UTF-16 units
        assert_eq!(li.position(7, PositionEncoding::Utf16), Position::new(0, 4));
        // same byte in UTF-8 encoding counts bytes -> column 7
        assert_eq!(li.position(7, PositionEncoding::Utf8), Position::new(0, 7));
    }

    #[test]
    fn position_to_byte_round_trips_ascii() {
        let s = "ab\ncde\n";
        let li = idx(s);
        for b in [0usize, 1, 3, 5, 7] {
            let p = li.position(b, PositionEncoding::Utf16);
            assert_eq!(li.offset(p, s, PositionEncoding::Utf16), b);
        }
    }

    #[test]
    fn empty_document() {
        let li = idx("");
        assert_eq!(li.position(0, PositionEncoding::Utf16), Position::new(0, 0));
    }

    #[test]
    fn position_with_mid_codepoint_byte_does_not_panic() {
        // #132: a diagnostic byte range can end inside a multibyte char (e.g. an
        // unterminated comment containing `𝄞`). `position` must clamp to a char
        // boundary instead of panicking and aborting the whole server.
        let s = "/* é 𝄞"; // '𝄞' occupies bytes 6..10
        let li = idx(s);
        // Byte 7 is inside the 4-byte '𝄞' — must behave like byte 6, not panic.
        assert_eq!(
            li.position(7, PositionEncoding::Utf16),
            li.position(6, PositionEncoding::Utf16)
        );
        assert_eq!(li.position(7, PositionEncoding::Utf16), Position::new(0, 5));
        // Past-end offsets must also be safe.
        let _ = li.position(s.len(), PositionEncoding::Utf16);
        let _ = li.position(9999, PositionEncoding::Utf8);
    }
}
