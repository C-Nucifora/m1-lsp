//! textDocument/codeAction: quick-fixes for the mechanically-fixable
//! `unsupported-c-token` diagnostics — the C operators that have a direct M1
//! keyword replacement (`==`→`eq`, `!=`→`neq`, `&&`→`and`, `||`→`or`, `!`→`not`).
//!
//! `while` / `for` / `do` are *not* offered: there is no local rewrite (M1 has no
//! iteration), so their diagnostics stay informational-only.
use crate::line_index::{LineIndex, PositionEncoding};
use std::collections::HashMap;
use tower_lsp::lsp_types::{
    CodeAction, CodeActionKind, CodeActionOrCommand, Diagnostic, NumberOrString, TextEdit, Url,
    WorkspaceEdit,
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
    for d in diagnostics.iter().filter(|d| is_unsupported_c_token(d)) {
        let start = li.offset(d.range.start, text, enc);
        let end = li.offset(d.range.end, text, enc);
        if start >= end || end > text.len() {
            continue;
        }
        let op = &text[start..end];
        let Some(keyword) = replacement(op) else {
            continue; // while / for / do: no mechanical fix
        };
        let edit = TextEdit {
            range: d.range,
            new_text: padded(text, start, end, keyword),
        };
        let mut changes = HashMap::new();
        changes.insert(uri.clone(), vec![edit]);
        out.push(CodeActionOrCommand::CodeAction(CodeAction {
            title: format!("Replace `{op}` with `{keyword}`"),
            kind: Some(CodeActionKind::QUICKFIX),
            diagnostics: Some(vec![d.clone()]),
            edit: Some(WorkspaceEdit {
                changes: Some(changes),
                document_changes: None,
                change_annotations: None,
            }),
            is_preferred: Some(true),
            ..Default::default()
        }));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use tower_lsp::lsp_types::{DiagnosticSeverity, Range};

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

    #[test]
    fn bang_becomes_not() {
        assert_eq!(fix("x = !flag;\n", "!").unwrap(), "x = not flag;\n");
    }

    #[test]
    fn no_fix_for_while() {
        let src = "while (1) {}\n";
        let li = LineIndex::new(src);
        let d = diag_for(src, "while", &li);
        assert!(code_actions(src, &li, PositionEncoding::Utf16, &uri(), &[d]).is_empty());
    }
}
