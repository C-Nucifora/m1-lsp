//! Mapping from m1-core (and later m1-lint) diagnostic types to lsp-types.
use crate::line_index::{LineIndex, PositionEncoding};
use m1_core::{Code, Diagnostic as CoreDiag, Severity};
use tower_lsp::lsp_types::{Diagnostic as LspDiag, DiagnosticSeverity, NumberOrString, Range};

pub fn severity(s: Severity) -> DiagnosticSeverity {
    match s {
        Severity::Error => DiagnosticSeverity::ERROR,
        Severity::Warning => DiagnosticSeverity::WARNING,
        Severity::Info => DiagnosticSeverity::INFORMATION,
        Severity::Hint => DiagnosticSeverity::HINT,
    }
}

pub fn code_str(c: Code) -> &'static str {
    match c {
        Code::SyntaxError => "syntax-error",
        Code::MissingToken => "missing-token",
    }
}

pub fn range(
    byte_range: &std::ops::Range<usize>,
    li: &LineIndex,
    enc: PositionEncoding,
) -> Range {
    Range::new(
        li.position(byte_range.start, enc),
        li.position(byte_range.end, enc),
    )
}

pub fn core_diagnostic(d: &CoreDiag, li: &LineIndex, enc: PositionEncoding) -> LspDiag {
    LspDiag {
        range: range(&d.byte_range, li, enc),
        severity: Some(severity(d.severity)),
        code: Some(NumberOrString::String(code_str(d.code).to_string())),
        source: Some("m1-core".to_string()),
        message: d.message.clone(),
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_severity_and_code() {
        assert_eq!(severity(Severity::Error), DiagnosticSeverity::ERROR);
        assert_eq!(severity(Severity::Hint), DiagnosticSeverity::HINT);
        assert_eq!(code_str(Code::SyntaxError), "syntax-error");
    }

    #[test]
    fn maps_core_diagnostic_to_lsp() {
        let src = "local <Integer> = 1;\n"; // missing name -> syntax error(s)
        let cst = m1_core::parse(src);
        let li = LineIndex::new(src);
        let diags = cst.syntax_diagnostics();
        assert!(!diags.is_empty());
        let lsp = core_diagnostic(&diags[0], &li, PositionEncoding::Utf16);
        assert_eq!(lsp.severity, Some(DiagnosticSeverity::ERROR));
        assert_eq!(lsp.source.as_deref(), Some("m1-core"));
        assert!(lsp.range.start.line == 0);
    }
}
