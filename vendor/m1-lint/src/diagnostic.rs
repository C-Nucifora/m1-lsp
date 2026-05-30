//! Lint-specific diagnostic types.

use m1_core::{Diagnostic, Range, Severity};
use std::fmt;

/// A lint rule code.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum LintCode {
    /// L001 — line-too-long
    L001,
    /// L002 — trailing-whitespace
    L002,
    /// L003 — missing-final-newline
    L003,
    /// L004 — eq-operator-preferred
    L004,
    /// L005 — logical-operator-preferred
    L005,
    /// L006 — float-eq-comparison
    L006,
    /// L007 — operator-spacing
    L007,
    /// L008 — nesting-too-deep
    L008,
    /// L009 — cyclomatic-complexity
    L009,
    /// L010 — tab-for-indentation
    L010,
    /// L011 — comment-style
    L011,
}

impl fmt::Display for LintCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LintCode::L001 => write!(f, "L001"),
            LintCode::L002 => write!(f, "L002"),
            LintCode::L003 => write!(f, "L003"),
            LintCode::L004 => write!(f, "L004"),
            LintCode::L005 => write!(f, "L005"),
            LintCode::L006 => write!(f, "L006"),
            LintCode::L007 => write!(f, "L007"),
            LintCode::L008 => write!(f, "L008"),
            LintCode::L009 => write!(f, "L009"),
            LintCode::L010 => write!(f, "L010"),
            LintCode::L011 => write!(f, "L011"),
        }
    }
}

impl LintCode {
    /// Every lint code, in numeric order.
    pub fn all_codes() -> &'static [LintCode] {
        use LintCode::*;
        &[L001, L002, L003, L004, L005, L006, L007, L008, L009, L010, L011]
    }

    /// Parse a code string such as `"L004"`.
    pub fn from_code_str(s: &str) -> Option<LintCode> {
        LintCode::all_codes()
            .iter()
            .copied()
            .find(|c| c.to_string() == s)
    }

    /// Stable human-readable rule name.
    pub fn name(&self) -> &'static str {
        match self {
            LintCode::L001 => "line-too-long",
            LintCode::L002 => "trailing-whitespace",
            LintCode::L003 => "missing-final-newline",
            LintCode::L004 => "eq-operator-preferred",
            LintCode::L005 => "logical-operator-preferred",
            LintCode::L006 => "float-eq-comparison",
            LintCode::L007 => "operator-spacing",
            LintCode::L008 => "nesting-too-deep",
            LintCode::L009 => "cyclomatic-complexity",
            LintCode::L010 => "tab-for-indentation",
            LintCode::L011 => "comment-style",
        }
    }

    /// Whether `m1-lint --fix` can mechanically fix this rule's diagnostics.
    pub fn fixable(&self) -> bool {
        matches!(
            self,
            LintCode::L002
                | LintCode::L003
                | LintCode::L004
                | LintCode::L005
                | LintCode::L007
                | LintCode::L011
        )
    }
}

/// A diagnostic emitted by a lint rule.
#[derive(Debug, Clone)]
pub struct LintDiagnostic {
    /// The lint rule code.
    pub code: LintCode,
    /// The underlying diagnostic (range, severity, message).
    pub inner: Diagnostic,
}

impl LintDiagnostic {
    /// Construct a new `LintDiagnostic`.
    ///
    /// The `inner.code` field is set to the placeholder
    /// `m1_core::Code::SyntaxError`; `m1_core::Code` has no lint variant. The
    /// meaningful code is [`LintDiagnostic::code`].
    pub fn new(
        code: LintCode,
        range: Range,
        byte_range: std::ops::Range<usize>,
        severity: Severity,
        message: impl Into<String>,
    ) -> Self {
        Self {
            code,
            inner: Diagnostic {
                range,
                byte_range,
                severity,
                code: m1_core::Code::SyntaxError,
                message: message.into(),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_codes() {
        assert_eq!(LintCode::L001.to_string(), "L001");
        assert_eq!(LintCode::L009.to_string(), "L009");
    }

    #[test]
    fn round_trips_code_str() {
        assert_eq!(LintCode::from_code_str("L004"), Some(LintCode::L004));
        assert_eq!(LintCode::from_code_str("L011"), Some(LintCode::L011));
        assert_eq!(LintCode::from_code_str("nope"), None);
    }

    #[test]
    fn all_codes_has_eleven() {
        assert_eq!(LintCode::all_codes().len(), 11);
    }

    #[test]
    fn fixable_flags() {
        assert!(LintCode::L004.fixable());
        assert!(!LintCode::L001.fixable());
        assert!(!LintCode::L006.fixable());
    }
}
