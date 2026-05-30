//! Shared diagnostic types emitted by every M1 tool.

/// A 0-based source position. `column` is a **byte** offset within `line`
/// (UTF-16/LSP encoding conversion is the responsibility of m1-lsp).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Position {
    pub line: u32,
    pub column: u32,
}

/// A half-open range between two [`Position`]s.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Range {
    pub start: Position,
    pub end: Position,
}

/// Diagnostic severity.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    Error,
    Warning,
    Info,
    Hint,
}

/// Stable diagnostic code. Grows as new producers are added.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Code {
    SyntaxError,
    MissingToken,
}

/// A single diagnostic with both line/column and byte ranges.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Diagnostic {
    pub range: Range,
    pub byte_range: std::ops::Range<usize>,
    pub severity: Severity,
    pub code: Code,
    pub message: String,
}

impl Diagnostic {
    /// Build a diagnostic from explicit ranges.
    pub fn new(
        severity: Severity,
        code: Code,
        range: Range,
        byte_range: std::ops::Range<usize>,
        message: impl Into<String>,
    ) -> Diagnostic {
        Diagnostic {
            range,
            byte_range,
            severity,
            code,
            message: message.into(),
        }
    }

    /// Build an [`Severity::Error`] diagnostic.
    pub fn error(
        code: Code,
        range: Range,
        byte_range: std::ops::Range<usize>,
        message: impl Into<String>,
    ) -> Diagnostic {
        Diagnostic::new(Severity::Error, code, range, byte_range, message)
    }

    /// Build a [`Severity::Warning`] diagnostic.
    pub fn warning(
        code: Code,
        range: Range,
        byte_range: std::ops::Range<usize>,
        message: impl Into<String>,
    ) -> Diagnostic {
        Diagnostic::new(Severity::Warning, code, range, byte_range, message)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn diagnostic_is_constructible() {
        let d = Diagnostic {
            range: Range {
                start: Position { line: 0, column: 0 },
                end: Position { line: 0, column: 3 },
            },
            byte_range: 0..3,
            severity: Severity::Error,
            code: Code::SyntaxError,
            message: "syntax error".to_string(),
        };
        assert_eq!(d.severity, Severity::Error);
        assert_eq!(d.range.end.column, 3);
    }

    #[test]
    fn constructors_match_struct_literal() {
        let range = Range {
            start: Position { line: 0, column: 0 },
            end: Position { line: 0, column: 3 },
        };
        let expected = Diagnostic {
            range,
            byte_range: 0..3,
            severity: Severity::Error,
            code: Code::SyntaxError,
            message: "boom".to_string(),
        };
        assert_eq!(
            Diagnostic::new(Severity::Error, Code::SyntaxError, range, 0..3, "boom"),
            expected
        );
        assert_eq!(
            Diagnostic::error(Code::SyntaxError, range, 0..3, "boom"),
            expected
        );
        assert_eq!(
            Diagnostic::warning(Code::SyntaxError, range, 0..3, "boom").severity,
            Severity::Warning
        );
    }
}
