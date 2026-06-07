//! textDocument/codeAction. Quick-fixes for:
//! - `unsupported-c-token` C operators with a direct M1 keyword replacement
//!   (`==`→`eq`, `!=`→`neq`, `&&`→`and`, `||`→`or`, `!`→`not`), and the equivalent
//!   lint diagnostics L004/L005;
//! - L002 trailing-whitespace (delete the span);
//! - `while`/`for`/`do`, which have no mechanical rewrite (M1 has no iteration) but
//!   get a `WhenStatement` skeleton inserted above them as a starting point (#83).
use crate::line_index::{LineIndex, PositionEncoding};
use m1_typecheck::project::Project;
use std::collections::HashMap;
use tower_lsp::lsp_types::{
    CodeAction, CodeActionKind, CodeActionOrCommand, Diagnostic, NumberOrString, Position, Range,
    TextEdit, Url, WorkspaceEdit,
};

/// True for the `T020` enum-non-member type diagnostic.
fn is_t020(d: &Diagnostic) -> bool {
    matches!(&d.code, Some(NumberOrString::String(s)) if s == "T020")
}

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

/// True for a `missing-token` syntax diagnostic (m1-core inserted a zero-width
/// MISSING node during recovery, e.g. for an absent statement terminator).
fn is_missing_token(d: &Diagnostic) -> bool {
    matches!(&d.code, Some(NumberOrString::String(s)) if s == "missing-token")
}

/// The corrected form of an L016-flagged local name: spaces become underscores
/// and the first letter is lowercased (M1 locals are conventionally lowerCamel).
/// `None` when the name already conforms (nothing to fix).
fn corrected_local_name(name: &str) -> Option<String> {
    let snake = name.replace(' ', "_");
    let mut chars = snake.chars();
    let first = chars.next()?;
    let corrected = format!("{}{}", first.to_ascii_lowercase(), chars.as_str());
    (corrected != name).then_some(corrected)
}

/// The token the parser expected, parsed from a `missing X` diagnostic message
/// (`"missing ;"` → `";"`). `None` if the message isn't in that form or the token
/// is implausible (empty, or long enough to suggest it isn't a punctuation token).
fn expected_missing_token(message: &str) -> Option<&str> {
    let tok = message.strip_prefix("missing ")?.trim();
    (!tok.is_empty() && tok.len() <= 3 && !tok.contains(char::is_whitespace)).then_some(tok)
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
    project: Option<&Project>,
) -> Vec<CodeActionOrCommand> {
    let mut out = Vec::new();

    // T020 enum-non-member: suggest the nearest valid enum member (#159). Needs
    // the project model to look up the enum's members. The diagnostic range spans
    // the whole `<Enum>.<Member>` path.
    if let Some(project) = project {
        let table = project.symbols();
        for d in diagnostics.iter().filter(|d| is_t020(d)) {
            let start = li.offset(d.range.start, text, enc);
            let end = li.offset(d.range.end, text, enc);
            let Some((head, member)) = text.get(start..end).and_then(|s| s.rsplit_once('.')) else {
                continue;
            };
            let (head, member) = (head.trim(), member.trim());
            let Some(id) = table.enum_by_name(head) else {
                continue;
            };
            if let Some(best) = nearest_enum_member(member, &table.enum_type(id).members)
                && best != member
            {
                out.push(quickfix(
                    format!("Replace `{member}` with `{best}`"),
                    uri,
                    vec![TextEdit {
                        range: d.range,
                        new_text: format!("{head}.{best}"),
                    }],
                    Some(d),
                ));
            }
        }
    }

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

    // L016 local-variable-naming: offer a one-click rename to the corrected name
    // across the whole file, reusing the rename machinery (#162).
    let l016: Vec<&Diagnostic> = diagnostics
        .iter()
        .filter(|d| lint_code(d) == Some("L016"))
        .collect();
    if !l016.is_empty() {
        let cst = m1_core::parse(text);
        for d in l016 {
            let start = li.offset(d.range.start, text, enc);
            let end = li.offset(d.range.end, text, enc);
            let Some(name) = text.get(start..end) else {
                continue;
            };
            let Some(corrected) = corrected_local_name(name) else {
                continue;
            };
            if let Some(edits) =
                crate::features::rename::local_rename_edits(cst.root(), start, &corrected, li, enc)
                && !edits.is_empty()
            {
                out.push(quickfix(
                    format!("Rename `{name}` to `{corrected}`"),
                    uri,
                    edits,
                    Some(d),
                ));
            }
        }
    }

    // Missing-token syntax error: the parser pinpoints a token it had to
    // synthesise (`missing ;`, `missing )`, …) at a zero-width position. Offer a
    // quick-fix that inserts the expected token there — the in-editor counterpart
    // of `m1-lint --fix`'s missing-semicolon repair.
    for d in diagnostics.iter().filter(|d| is_missing_token(d)) {
        let Some(tok) = expected_missing_token(&d.message) else {
            continue;
        };
        out.push(quickfix(
            format!("Insert `{tok}`"),
            uri,
            vec![TextEdit {
                range: Range::new(d.range.start, d.range.start),
                new_text: tok.to_string(),
            }],
            Some(d),
        ));
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

/// A source-level "Format Document" / "Format Selection" action (kind `SOURCE`)
/// that applies `edits` to the document. Surfaced independently of diagnostics so
/// the code-action menu offers formatting even on clean code (#161).
pub fn format_action(title: &str, uri: &Url, edits: Vec<TextEdit>) -> CodeActionOrCommand {
    let mut changes = HashMap::new();
    changes.insert(uri.clone(), edits);
    CodeActionOrCommand::CodeAction(CodeAction {
        title: title.to_string(),
        kind: Some(CodeActionKind::SOURCE),
        edit: Some(WorkspaceEdit {
            changes: Some(changes),
            document_changes: None,
            change_annotations: None,
        }),
        ..Default::default()
    })
}

/// A whole-document "fix all auto-fixable lint issues" action that replaces the
/// buffer with `fixed` (the output of the shared `m1-lint` fixer). Kind
/// `SOURCE_FIX_ALL` so editors can run it on save and offer it from the lightbulb
/// — this is what gives L003/L007/L011/L018 (and any future fixable rule) an
/// in-editor fix without hand-porting each rule (#158).
pub fn fix_all_lint_action(
    uri: &Url,
    text: &str,
    li: &LineIndex,
    enc: PositionEncoding,
    fixed: String,
) -> CodeActionOrCommand {
    let end = li.position(text.len(), enc);
    let mut changes = HashMap::new();
    changes.insert(
        uri.clone(),
        vec![TextEdit {
            range: Range::new(Position::new(0, 0), end),
            new_text: fixed,
        }],
    );
    CodeActionOrCommand::CodeAction(CodeAction {
        title: "Fix all auto-fixable lint issues (m1-lint)".to_string(),
        kind: Some(CodeActionKind::SOURCE_FIX_ALL),
        edit: Some(WorkspaceEdit {
            changes: Some(changes),
            document_changes: None,
            change_annotations: None,
        }),
        ..Default::default()
    })
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

/// Levenshtein edit distance between two strings (small inputs — enum member
/// names — so the simple O(m·n) row form is fine).
fn edit_distance(a: &str, b: &str) -> usize {
    let b: Vec<char> = b.chars().collect();
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    let mut cur = vec![0usize; b.len() + 1];
    for (i, ca) in a.chars().enumerate() {
        cur[0] = i + 1;
        for (j, &cb) in b.iter().enumerate() {
            let cost = if ca == cb { 0 } else { 1 };
            cur[j + 1] = (prev[j] + cost).min(prev[j + 1] + 1).min(cur[j] + 1);
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    prev[b.len()]
}

/// The enum member that `typo` most likely meant: a case-insensitive match wins
/// outright (e.g. `OFf` → `Off`); otherwise the smallest edit distance, but only
/// when it is close enough (≤ a third of the name's length, min 1) so unrelated
/// garbage suggests nothing. `None` when no member is close.
fn nearest_enum_member(typo: &str, members: &[(String, i64)]) -> Option<String> {
    if let Some((name, _)) = members.iter().find(|(m, _)| m.eq_ignore_ascii_case(typo)) {
        return Some(name.clone());
    }
    let mut best: Option<(&str, usize)> = None;
    for (m, _) in members {
        let d = edit_distance(&typo.to_ascii_lowercase(), &m.to_ascii_lowercase());
        if best.is_none_or(|(_, bd)| d < bd) {
            best = Some((m, d));
        }
    }
    best.and_then(|(name, d)| {
        let limit = (name.chars().count() / 3).max(1);
        (d <= limit).then(|| name.to_string())
    })
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
        let actions = code_actions(src, &li, enc, &uri(), &[d], None);
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
    fn nearest_enum_member_prefers_case_then_edit_distance() {
        let members = vec![
            ("Off".to_string(), 0i64),
            ("On".to_string(), 1),
            ("Driving".to_string(), 2),
        ];
        // case-only difference wins outright
        assert_eq!(nearest_enum_member("OFf", &members).as_deref(), Some("Off"));
        // one-edit typo
        assert_eq!(nearest_enum_member("Of", &members).as_deref(), Some("Off"));
        assert_eq!(
            nearest_enum_member("Drivng", &members).as_deref(),
            Some("Driving")
        );
        // far-off garbage suggests nothing
        assert_eq!(nearest_enum_member("Xyzzy", &members), None);
        // an exact member is not a typo (caller shouldn't have flagged it, but be safe)
        assert_eq!(nearest_enum_member("On", &members).as_deref(), Some("On"));
    }

    #[test]
    fn spaces_tight_operator() {
        assert_eq!(fix("x = a==b;\n", "==").unwrap(), "x = a eq b;\n");
    }

    #[test]
    fn l016_offers_rename_to_corrected_local_name() {
        // #162: an L016 naming warning on `local Count` should offer a one-click
        // rename to the corrected name, applied to every reference in the file.
        let src = "local Count = 0;\nCount = Count + 1;\n";
        let li = LineIndex::new(src);
        let enc = PositionEncoding::Utf16;
        // L016 diagnostic on the declaration's `Count`.
        let d = lint_diag(src, "Count", "L016", &li);
        let action = code_actions(src, &li, enc, &uri(), &[d], None)
            .into_iter()
            .find_map(|a| match a {
                CodeActionOrCommand::CodeAction(a) if a.title.contains("count") => Some(a),
                _ => None,
            })
            .expect("an L016 rename quick-fix should be offered");
        assert_eq!(action.is_preferred, Some(true));
        let edits = action
            .edit
            .unwrap()
            .changes
            .unwrap()
            .into_values()
            .next()
            .unwrap();
        // Declaration + two references = 3 edits, all renaming to `count`.
        assert_eq!(edits.len(), 3, "all occurrences renamed: {edits:?}");
        assert!(edits.iter().all(|e| e.new_text == "count"));
    }

    #[test]
    fn missing_semicolon_offers_insert_quickfix() {
        // The parser reports a zero-width `missing-token` diagnostic ("missing ;")
        // at the spot the `;` should go. Offer a lightbulb quick-fix that inserts
        // it, mirroring `m1-lint --fix`.
        let src = "x = 1\n";
        let li = LineIndex::new(src);
        let enc = PositionEncoding::Utf16;
        // Diagnostic at the end of `x = 1` (byte 5, zero-width).
        let pos = li.position(5, enc);
        let d = Diagnostic {
            range: Range::new(pos, pos),
            severity: Some(DiagnosticSeverity::ERROR),
            code: Some(NumberOrString::String("missing-token".into())),
            source: Some("m1-core".into()),
            message: "missing ;".into(),
            ..Default::default()
        };
        let actions = code_actions(src, &li, enc, &uri(), &[d], None);
        let action = actions
            .into_iter()
            .find_map(|a| match a {
                CodeActionOrCommand::CodeAction(a) if a.title.contains(';') => Some(a),
                _ => None,
            })
            .expect("a quick-fix inserting `;` should be offered");
        let edit = action
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
        assert_eq!(edit.new_text, ";");
        // Applying it produces valid source.
        let start = li.offset(edit.range.start, src, enc);
        let mut s = src.to_string();
        s.insert_str(start, &edit.new_text);
        assert_eq!(s, "x = 1;\n");
        assert!(m1_core::parse(&s).syntax_diagnostics().is_empty());
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
        code_actions(src, &li, PositionEncoding::Utf16, &uri(), diags, None)
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
        let actions = code_actions(src, &li, enc, &uri(), &[d], None);
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
        let actions = code_actions(src, &li, enc, &uri(), &[d], None);
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
        let actions = code_actions(src, &li, PositionEncoding::Utf16, &uri(), &[d], None);
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
        let _ = code_actions(src, &li, PositionEncoding::Utf8, &uri(), &[d], None);
    }

    #[test]
    fn fix_all_action_replaces_the_whole_document() {
        let text = "//x\n";
        let li = LineIndex::new(text);
        let a = fix_all_lint_action(
            &uri(),
            text,
            &li,
            PositionEncoding::Utf16,
            "// x\n".to_string(),
        );
        let CodeActionOrCommand::CodeAction(a) = a else {
            panic!("expected a code action");
        };
        assert_eq!(a.kind, Some(CodeActionKind::SOURCE_FIX_ALL));
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
        assert_eq!(edit.new_text, "// x\n");
        assert_eq!(edit.range.start, Position::new(0, 0));
    }

    #[test]
    fn t020_offers_nearest_enum_member_quickfix() {
        use crate::project_store::ProjectStore;
        use std::io::Write;

        let tmp = tempfile::tempdir().unwrap();
        let xml = r#"<?xml version="1.0"?>
<Project>
  <DataTypes>
    <Type Name="Drive State" Storage="enum" Default="Off">
      <Enum Name="Off" ContainerOrder="0"/>
      <Enum Name="On" ContainerOrder="1"/>
    </Type>
  </DataTypes>
  <Component Classname="BuiltIn.GroupCompound" Name="Root"/>
</Project>"#;
        std::fs::File::create(tmp.path().join("Project.m1prj"))
            .unwrap()
            .write_all(xml.as_bytes())
            .unwrap();
        let store = ProjectStore::new();
        store.discover_and_load(tmp.path()).unwrap();

        // A T020 on `Drive State.Of` (a typo of `Off`) spanning the whole path.
        let src = "x = Drive State.Of;\n";
        let li = LineIndex::new(src);
        let enc = PositionEncoding::Utf16;
        let at = src.find("Drive State.Of").unwrap();
        let d = Diagnostic {
            range: Range::new(
                li.position(at, enc),
                li.position(at + "Drive State.Of".len(), enc),
            ),
            code: Some(NumberOrString::String("T020".into())),
            ..Default::default()
        };
        store.with_project(|p| {
            let actions = code_actions(
                src,
                &li,
                enc,
                &uri(),
                std::slice::from_ref(&d),
                p.map(|lp| &lp.project),
            );
            let titles: Vec<_> = actions
                .iter()
                .filter_map(|a| match a {
                    CodeActionOrCommand::CodeAction(a) => Some(a.title.clone()),
                    _ => None,
                })
                .collect();
            assert!(
                titles.iter().any(|t| t == "Replace `Of` with `Off`"),
                "expected a did-you-mean fix; got {titles:?}"
            );
        });
    }
}
