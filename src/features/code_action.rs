//! textDocument/codeAction. Quick-fixes for:
//! - `unsupported-c-token` C operators with a direct M1 keyword replacement
//!   (`==`→`eq`, `!=`→`neq`, `&&`→`and`, `||`→`or`, `!`→`not`), and the equivalent
//!   lint diagnostics L004/L005;
//! - L002 trailing-whitespace (delete the span);
//! - `while`/`for`/`do`, which have no mechanical rewrite (M1 has no iteration) but
//!   get a `WhenStatement` skeleton inserted above them as a starting point (#83).
use crate::line_index::{LineIndex, PositionEncoding};
use std::collections::HashMap;
use tower_lsp::lsp_types::{
    CodeAction, CodeActionKind, CodeActionOrCommand, Diagnostic, NumberOrString, Position, Range,
    TextEdit, Url, WorkspaceEdit,
};

/// The M1 keyword that replaces a C operator, for the ones with a clean swap.
fn replacement(op: &str) -> Option<&'static str> {
    match op {
        "==" => Some("eq"),
        "!=" => Some("neq"),
        "&&" => Some("and"),
        "||" => Some("or"),
        "!" => Some("not"),
        _ => None,
    }
}

fn is_unsupported_c_token(d: &Diagnostic) -> bool {
    matches!(&d.code, Some(NumberOrString::String(s)) if s == "unsupported-c-token")
}

/// The lint rule code (`"L004"`, …) of a diagnostic, if it carries one.
fn lint_code(d: &Diagnostic) -> Option<&str> {
    match &d.code {
        Some(NumberOrString::String(s)) if s.starts_with('L') => Some(s),
        _ => None,
    }
}

/// True for diagnostics whose fix is an operator→keyword swap: the syntax-level
/// `unsupported-c-token`, and the lint-level L004 (`eq`-operator) / L005 (spelled
/// logical operator), whose ranges also cover the operator.
fn is_operator_fix(d: &Diagnostic) -> bool {
    is_unsupported_c_token(d) || matches!(lint_code(d), Some("L004" | "L005"))
}

fn quickfix(
    title: String,
    uri: &Url,
    edits: Vec<TextEdit>,
    diag: Option<&Diagnostic>,
) -> CodeActionOrCommand {
    let mut changes = HashMap::new();
    changes.insert(uri.clone(), edits);
    CodeActionOrCommand::CodeAction(CodeAction {
        title,
        kind: Some(CodeActionKind::QUICKFIX),
        diagnostics: diag.map(|d| vec![d.clone()]),
        edit: Some(WorkspaceEdit {
            changes: Some(changes),
            document_changes: None,
            change_annotations: None,
        }),
        is_preferred: Some(diag.is_some()),
        ..Default::default()
    })
}

/// Pad the replacement keyword with spaces only where the operator currently
/// abuts an adjacent token, so `a==b` becomes `a eq b` while `a == b` stays
/// single-spaced. `(` / `)` count as natural boundaries.
fn padded(text: &str, start: usize, end: usize, keyword: &str) -> String {
    let prev = text[..start].chars().next_back();
    let next = text[end..].chars().next();
    let lead = matches!(prev, Some(c) if !c.is_whitespace() && c != '(');
    let trail = matches!(next, Some(c) if !c.is_whitespace() && c != ')');
    let mut s = String::new();
    if lead {
        s.push(' ');
    }
    s.push_str(keyword);
    if trail {
        s.push(' ');
    }
    s
}

pub fn code_actions(
    text: &str,
    li: &LineIndex,
    enc: PositionEncoding,
    uri: &Url,
    diagnostics: &[Diagnostic],
) -> Vec<CodeActionOrCommand> {
    let mut out = Vec::new();

    // Per-diagnostic operator quick-fix: syntax `unsupported-c-token` + L004/L005.
    for d in diagnostics.iter().filter(|d| is_operator_fix(d)) {
        if let Some((op, keyword, edit)) = operator_edit(text, li, enc, d) {
            out.push(quickfix(
                format!("Replace `{op}` with `{keyword}`"),
                uri,
                vec![edit],
                Some(d),
            ));
        }
    }

    // L002 trailing-whitespace: delete the flagged span.
    for d in diagnostics.iter().filter(|d| lint_code(d) == Some("L002")) {
        out.push(quickfix(
            "Remove trailing whitespace".to_string(),
            uri,
            vec![TextEdit {
                range: d.range,
                new_text: String::new(),
            }],
            Some(d),
        ));
    }

    // `while` / `for` / `do`: no mechanical transform (M1 has no iteration), but
    // offer a WhenStatement skeleton inserted above the construct (#83).
    for d in diagnostics.iter() {
        let Some(kw) = loop_keyword(text, li, enc, d) else {
            continue;
        };
        let line = d.range.start.line;
        let indent = line_indent(text, li, enc, line);
        let stub = format!(
            "{indent}-- TODO: `{kw}` is not supported in M1 — rewrite as a WhenStatement\n\
             {indent}When State.Phase {{\n\
             {indent}    Is Phase.Init: -- …\n\
             {indent}}}\n"
        );
        let pos = Position::new(line, 0);
        out.push(quickfix(
            "Insert WhenStatement template".to_string(),
            uri,
            vec![TextEdit {
                range: Range::new(pos, pos),
                new_text: stub,
            }],
            Some(d),
        ));
    }

    // Bulk "fix all <code> in file" for the operator lints, when >1 occurs.
    for code in ["L004", "L005"] {
        let edits: Vec<TextEdit> = diagnostics
            .iter()
            .filter(|d| lint_code(d) == Some(code))
            .filter_map(|d| operator_edit(text, li, enc, d).map(|(_, _, e)| e))
            .collect();
        if edits.len() > 1 {
            out.push(quickfix(
                format!("Fix all {code} in file"),
                uri,
                edits,
                None,
            ));
        }
    }

    out
}

/// The operator→keyword replacement edit for an operator-fix diagnostic, or
/// `None` if its range doesn't cover a replaceable operator (e.g. `while`/`for`).
fn operator_edit(
    text: &str,
    li: &LineIndex,
    enc: PositionEncoding,
    d: &Diagnostic,
) -> Option<(String, &'static str, TextEdit)> {
    let start = li.offset(d.range.start, text, enc);
    let end = li.offset(d.range.end, text, enc);
    // get() (not indexing): a range produced under a different position encoding
    // may not land on a char boundary — skip rather than panic.
    let op = text.get(start..end).filter(|s| !s.is_empty())?;
    let keyword = replacement(op)?;
    let edit = TextEdit {
        range: d.range,
        new_text: padded(text, start, end, keyword),
    };
    Some((op.to_string(), keyword, edit))
}

/// `Some("while"|"for"|"do")` if the diagnostic is an unsupported-C-token whose
/// span is one of the loop keywords (which have no operator replacement).
fn loop_keyword(
    text: &str,
    li: &LineIndex,
    enc: PositionEncoding,
    d: &Diagnostic,
) -> Option<&'static str> {
    if !is_unsupported_c_token(d) {
        return None;
    }
    let start = li.offset(d.range.start, text, enc);
    let end = li.offset(d.range.end, text, enc);
    match text.get(start..end)? {
        "while" => Some("while"),
        "for" => Some("for"),
        "do" => Some("do"),
        _ => None,
    }
}

/// The leading whitespace (indentation) of `line`.
fn line_indent(text: &str, li: &LineIndex, enc: PositionEncoding, line: u32) -> String {
    let line_start = li.offset(Position::new(line, 0), text, enc);
    text[line_start..]
        .chars()
        .take_while(|c| *c == ' ' || *c == '\t')
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tower_lsp::lsp_types::{DiagnosticSeverity, Position, Range};

    fn uri() -> Url {
        Url::parse("file:///t.m1scr").unwrap()
    }

    /// Build the `unsupported-c-token` diagnostic for the substring `op` in `src`.
    fn diag_for(src: &str, op: &str, li: &LineIndex) -> Diagnostic {
        let start = src.find(op).unwrap();
        let enc = PositionEncoding::Utf16;
        Diagnostic {
            range: Range::new(li.position(start, enc), li.position(start + op.len(), enc)),
            severity: Some(DiagnosticSeverity::ERROR),
            code: Some(NumberOrString::String("unsupported-c-token".into())),
            message: "nope".into(),
            ..Default::default()
        }
    }

    fn fix(src: &str, op: &str) -> Option<String> {
        let li = LineIndex::new(src);
        let enc = PositionEncoding::Utf16;
        let d = diag_for(src, op, &li);
        let actions = code_actions(src, &li, enc, &uri(), &[d]);
        let CodeActionOrCommand::CodeAction(a) = actions.into_iter().next()? else {
            return None;
        };
        // Apply the single edit to produce the fixed source.
        let edit = a.edit?.changes?.into_values().next()?.into_iter().next()?;
        let start = li.offset(edit.range.start, src, enc);
        let end = li.offset(edit.range.end, src, enc);
        let mut s = src.to_string();
        s.replace_range(start..end, &edit.new_text);
        Some(s)
    }

    #[test]
    fn spaces_tight_operator() {
        assert_eq!(fix("x = a==b;\n", "==").unwrap(), "x = a eq b;\n");
    }

    #[test]
    fn keeps_existing_spacing() {
        assert_eq!(fix("x = a && b;\n", "&&").unwrap(), "x = a and b;\n");
    }

    fn lint_diag(src: &str, substr: &str, code: &str, li: &LineIndex) -> Diagnostic {
        let start = src.find(substr).unwrap();
        let enc = PositionEncoding::Utf16;
        Diagnostic {
            range: Range::new(
                li.position(start, enc),
                li.position(start + substr.len(), enc),
            ),
            severity: Some(DiagnosticSeverity::WARNING),
            code: Some(NumberOrString::String(code.into())),
            message: "lint".into(),
            ..Default::default()
        }
    }

    fn titles(src: &str, diags: &[Diagnostic]) -> Vec<String> {
        let li = LineIndex::new(src);
        code_actions(src, &li, PositionEncoding::Utf16, &uri(), diags)
            .into_iter()
            .filter_map(|a| match a {
                CodeActionOrCommand::CodeAction(a) => Some(a.title),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn l004_lint_diagnostic_offers_operator_fix() {
        let src = "x = a == b;\n";
        let li = LineIndex::new(src);
        let d = lint_diag(src, "==", "L004", &li);
        assert!(
            titles(src, &[d])
                .iter()
                .any(|t| t == "Replace `==` with `eq`"),
            "L004 should offer the eq fix"
        );
    }

    #[test]
    fn l002_offers_and_applies_trailing_whitespace_removal() {
        let src = "x = 1;  \n"; // two trailing spaces
        let li = LineIndex::new(src);
        let d = lint_diag(src, "  ", "L002", &li);
        let enc = PositionEncoding::Utf16;
        let actions = code_actions(src, &li, enc, &uri(), &[d]);
        let CodeActionOrCommand::CodeAction(a) = actions.into_iter().next().unwrap() else {
            panic!("expected an action");
        };
        assert_eq!(a.title, "Remove trailing whitespace");
        let edit = a
            .edit
            .unwrap()
            .changes
            .unwrap()
            .into_values()
            .next()
            .unwrap()
            .into_iter()
            .next()
            .unwrap();
        let start = li.offset(edit.range.start, src, enc);
        let end = li.offset(edit.range.end, src, enc);
        let mut s = src.to_string();
        s.replace_range(start..end, &edit.new_text);
        assert_eq!(s, "x = 1;\n");
    }

    #[test]
    fn bulk_fix_all_offered_for_multiple_l004() {
        let src = "x = a == b;\ny = c == d;\n";
        let li = LineIndex::new(src);
        let enc = PositionEncoding::Utf16;
        // Two L004 diagnostics, one per `==`.
        let first = src.find("==").unwrap();
        let second = src[first + 2..].find("==").unwrap() + first + 2;
        let mk = |at: usize| Diagnostic {
            range: Range::new(li.position(at, enc), li.position(at + 2, enc)),
            code: Some(NumberOrString::String("L004".into())),
            ..Default::default()
        };
        let ts = titles(src, &[mk(first), mk(second)]);
        assert!(
            ts.iter().any(|t| t == "Fix all L004 in file"),
            "expected a bulk fix, got {ts:?}"
        );
    }

    #[test]
    fn loop_keyword_offers_whenstatement_template() {
        let src = "    for i = 0 to 9 {\n";
        let li = LineIndex::new(src);
        let enc = PositionEncoding::Utf16;
        let d = diag_for(src, "for", &li); // unsupported-c-token on `for`
        let actions = code_actions(src, &li, enc, &uri(), &[d]);
        let CodeActionOrCommand::CodeAction(a) = actions.into_iter().next().unwrap() else {
            panic!("expected a code action");
        };
        assert_eq!(a.title, "Insert WhenStatement template");
        let edit = a
            .edit
            .unwrap()
            .changes
            .unwrap()
            .into_values()
            .next()
            .unwrap()
            .into_iter()
            .next()
            .unwrap();
        assert!(edit.new_text.contains("When State.Phase"));
        assert!(edit.new_text.contains("not supported in M1"));
        // Indentation of the construct is preserved on the inserted stub.
        assert!(
            edit.new_text.starts_with("    --"),
            "got: {:?}",
            edit.new_text
        );
    }

    #[test]
    fn bang_becomes_not() {
        assert_eq!(fix("x = !flag;\n", "!").unwrap(), "x = not flag;\n");
    }

    #[test]
    fn while_offers_only_the_whenstatement_template() {
        // `while` has no operator replacement, but now offers the WhenStatement
        // skeleton (#83) — and nothing else.
        let src = "while (1) {}\n";
        let li = LineIndex::new(src);
        let d = diag_for(src, "while", &li);
        let actions = code_actions(src, &li, PositionEncoding::Utf16, &uri(), &[d]);
        assert_eq!(actions.len(), 1);
        let CodeActionOrCommand::CodeAction(a) = &actions[0] else {
            panic!("expected a code action");
        };
        assert_eq!(a.title, "Insert WhenStatement template");
    }

    #[test]
    fn works_after_multibyte_char() {
        // The operator follows a multibyte char (`é` = 2 bytes); slicing must
        // land on the right bytes and produce the fix.
        assert_eq!(fix("x = café==b;\n", "==").unwrap(), "x = café eq b;\n");
    }

    #[test]
    fn off_boundary_range_is_skipped_not_panicked() {
        // `𝄞` is 4 bytes / 2 UTF-16 units. A range whose character offsets were
        // computed under UTF-16 (start=2) but get resolved here under UTF-8
        // lands mid-codepoint (byte 2); it must be skipped, not panic.
        let src = "𝄞==b;\n";
        let li = LineIndex::new(src);
        let d = Diagnostic {
            range: Range::new(Position::new(0, 2), Position::new(0, 4)),
            code: Some(NumberOrString::String("unsupported-c-token".into())),
            ..Default::default()
        };
        // Must not panic (produces no action for the mid-codepoint slice).
        let _ = code_actions(src, &li, PositionEncoding::Utf8, &uri(), &[d]);
    }
}
