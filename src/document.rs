//! In-memory document: full text plus its line index.
use crate::line_index::LineIndex;

pub struct Document {
    pub text: String,
    pub line_index: LineIndex,
    pub version: i32,
}

impl Document {
    pub fn new(text: String, version: i32) -> Self {
        let line_index = LineIndex::new(&text);
        Self {
            text,
            line_index,
            version,
        }
    }
}
