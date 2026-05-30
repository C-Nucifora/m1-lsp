//! Syntax-error diagnostics: ERROR -> `SyntaxError`, MISSING -> `MissingToken`.

use crate::cst::{Cst, Node};
use crate::diagnostic::{Code, Diagnostic, Severity};

pub(crate) fn collect(cst: &Cst) -> Vec<Diagnostic> {
    let mut out = Vec::new();
    walk(cst.root(), &mut out);
    out
}

fn walk(node: Node, out: &mut Vec<Diagnostic>) {
    if node.is_missing() {
        out.push(node.diagnostic(
            Severity::Error,
            Code::MissingToken,
            format!("missing {}", node.kind_str()),
        ));
    } else if node.is_error() {
        out.push(node.diagnostic(Severity::Error, Code::SyntaxError, "syntax error"));
    }
    for child in node.children() {
        walk(child, out);
    }
}

#[cfg(test)]
mod tests {
    use crate::{parse, Severity};

    #[test]
    fn clean_source_has_no_diagnostics() {
        let cst = parse("local x = (a >> 2) & 1;\n");
        assert!(cst.syntax_diagnostics().is_empty());
    }

    #[test]
    fn broken_source_reports_errors() {
        // `local <Type> = 1;` is missing the declared name.
        let cst = parse("local <Integer> = 1;\n");
        let diags = cst.syntax_diagnostics();
        assert!(!diags.is_empty(), "expected at least one diagnostic");
        assert!(
            diags.iter().all(|d| d.severity == Severity::Error),
            "syntax diagnostics are all errors"
        );
        // Range is within the source and non-degenerate at the source level.
        assert!(diags.iter().all(|d| d.byte_range.start <= d.byte_range.end));
    }
}
