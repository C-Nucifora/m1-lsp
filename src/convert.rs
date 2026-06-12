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
        // Widened in m1-core for downstream consumers (m1-core#7); kept
        // exhaustive so a m1-core dep bump doesn't break the build.
        Code::TypeError => "type-error",
        Code::LintError => "lint-error",
        Code::SemanticError => "semantic-error",
        Code::Annotation => "annotation",
    }
}

pub fn range(byte_range: &std::ops::Range<usize>, li: &LineIndex, enc: PositionEncoding) -> Range {
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

use m1_typecheck::diagnostics::{RelatedPlace, TypeDiagnostic};
use tower_lsp::lsp_types::{DiagnosticRelatedInformation, DiagnosticTag, Location, Position, Url};

pub fn type_diagnostic(
    d: &TypeDiagnostic,
    li: &LineIndex,
    enc: PositionEncoding,
    project_path: Option<&std::path::Path>,
) -> LspDiag {
    let code = d.code.as_str();
    // T062 flags use of a deprecated overload; tag it so editors strike it through.
    let tags = (code == "T062").then(|| vec![DiagnosticTag::DEPRECATED]);
    // Two-location diagnostics (m1-typecheck#200): T030/T085/T086 carry their
    // declaration site as a 0-based project-file line; the LSP knows the
    // project path, so it becomes clickable DiagnosticRelatedInformation.
    let related_information = project_path
        .filter(|_| !d.related.is_empty())
        .and_then(|prj| Url::from_file_path(prj).ok())
        .map(|url| {
            d.related
                .iter()
                .map(|r| {
                    let RelatedPlace::Project { line } = r.place;
                    DiagnosticRelatedInformation {
                        location: Location {
                            uri: url.clone(),
                            range: Range::new(Position::new(line, 0), Position::new(line, 0)),
                        },
                        message: r.message.clone(),
                    }
                })
                .collect()
        });
    LspDiag {
        range: range(&d.inner.byte_range, li, enc),
        severity: Some(severity(d.inner.severity)),
        code: Some(NumberOrString::String(code.to_string())),
        source: Some("m1-typecheck".to_string()),
        message: d.inner.message.clone(),
        tags,
        related_information,
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_type_diagnostic_to_lsp() {
        use m1_core::Severity;
        use m1_typecheck::diagnostics::{TypeCode, make};
        let src = "x = 1.0 == y;\n";
        let cst = m1_core::parse(src);
        let li = LineIndex::new(src);
        let node = cst.root();
        let d = make(
            TypeCode::T002,
            &node,
            Severity::Warning,
            "float equality".into(),
        );
        let lsp = type_diagnostic(&d, &li, PositionEncoding::Utf16, None);
        assert_eq!(lsp.source.as_deref(), Some("m1-typecheck"));
        assert_eq!(lsp.severity, Some(DiagnosticSeverity::WARNING));
        assert!(matches!(lsp.code, Some(NumberOrString::String(ref s)) if s == "T002"));
    }

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
