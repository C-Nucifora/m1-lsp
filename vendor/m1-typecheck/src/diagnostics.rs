//! Type-check diagnostic codes and result types.
use m1_core::{Code, Diagnostic, Node, Position, Range, Severity};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TypeCode {
    T001, // unresolved-reference
    T002, // float-equality
    T003, // int-float-mixing
    T020, // enum-non-member
    T021, // enum-numeric-comparison
    T030, // assignment-type-mismatch
    T040, // channel-multiple-assignment
    T050, // symbol-name-convention (project audit)
    T060, // stateful-conditional
    T061, // integrated-only-call
    T062, // deprecated-overload
}

impl TypeCode {
    pub fn as_str(self) -> &'static str {
        match self {
            TypeCode::T001 => "T001",
            TypeCode::T002 => "T002",
            TypeCode::T003 => "T003",
            TypeCode::T020 => "T020",
            TypeCode::T021 => "T021",
            TypeCode::T030 => "T030",
            TypeCode::T040 => "T040",
            TypeCode::T050 => "T050",
            TypeCode::T060 => "T060",
            TypeCode::T061 => "T061",
            TypeCode::T062 => "T062",
        }
    }
}

#[derive(Debug, Clone)]
pub struct TypeDiagnostic {
    pub code: TypeCode,
    pub inner: Diagnostic,
}

/// Build a `TypeDiagnostic` spanning `node`. The inner `Code` field is unused by
/// type rules (they carry identity in `TypeDiagnostic::code`); it is set to
/// `Code::SyntaxError` as a placeholder.
pub fn make(code: TypeCode, node: &Node, severity: Severity, message: String) -> TypeDiagnostic {
    TypeDiagnostic {
        code,
        inner: Diagnostic {
            range: node.range(),
            byte_range: node.byte_range(),
            severity,
            code: Code::SyntaxError,
            message,
        },
    }
}

/// Build a project-level `TypeDiagnostic` (no source node; zero range).
pub fn make_project(code: TypeCode, severity: Severity, message: String) -> TypeDiagnostic {
    let zero = Position { line: 0, column: 0 };
    TypeDiagnostic {
        code,
        inner: Diagnostic {
            range: Range {
                start: zero,
                end: zero,
            },
            byte_range: 0..0,
            severity,
            code: Code::SyntaxError,
            message,
        },
    }
}

#[derive(Debug, Default)]
pub struct CheckResult {
    pub diagnostics: Vec<TypeDiagnostic>,
    pub syntax_errors: Vec<Diagnostic>,
}
