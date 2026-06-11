//! textDocument/codeAction. Quick-fixes for:
//! - `unsupported-c-token` C operators with a direct M1 keyword replacement
//!   (`==`→`eq`, `!=`→`neq`, `&&`→`and`, `||`→`or`, `!`→`not`), and the equivalent
//!   lint diagnostics L004/L005;
//! - L002 trailing-whitespace (delete the span);
//! - `while`/`for`/`do`, which have no mechanical rewrite (M1 has no iteration) but
//!   get a `WhenStatement` skeleton inserted above them as a starting point (#83).
use crate::features::locate::in_type_annotation;
use crate::line_index::{LineIndex, PositionEncoding};
use m1_core::{Field, Kind, Node};
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
             {indent}}}
"
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

    // Suppress-with-`@m1:allow` (#249): for every diagnostic carrying a lint
    // L-code or typecheck T-code, offer the in-source suppression the whole
    // toolchain honours — `// @m1:allow(CODE)` on its own line above the
    // enclosing statement (or appended to an existing standalone allow line).
    // Offered only where it provably silences: the diagnostic must sit inside
    // a statement that starts its own line (the annotation attaches to the
    // next statement, so a comment-line or shared-line target would miss).
    if diagnostics.iter().any(|d| suppress_code(d).is_some()) {
        let cst = m1_core::parse(text);
        for d in diagnostics.iter() {
            let Some(code) = suppress_code(d) else {
                continue;
            };
            if let Some(edit) = suppress_edit(text, li, enc, &cst, d, code) {
                let mut action = quickfix(
                    format!("Suppress {code} for this statement"),
                    uri,
                    vec![edit],
                    Some(d),
                );
                if let CodeActionOrCommand::CodeAction(ca) = &mut action {
                    // Never outrank a real fix for the same diagnostic.
                    ca.is_preferred = Some(false);
                }
                out.push(action);
            }
        }
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

/// Selection-driven `REFACTOR` code actions (#174), independent of diagnostics:
/// "Extract to local" (a selected expression → a named `local` above the
/// statement) and "Inline local" (a single-assignment `local` → its initializer
/// at every read, declaration deleted). Both are purely in-file syntactic edits.
pub fn refactors(
    text: &str,
    li: &LineIndex,
    enc: PositionEncoding,
    uri: &Url,
    range: Range,
) -> Vec<CodeActionOrCommand> {
    let cst = m1_core::parse(text);
    let root = cst.root();
    let mut out = Vec::new();
    if let Some(a) = extract_local(text, li, enc, uri, range, root) {
        out.push(a);
    }
    if let Some(a) = inline_local(text, li, enc, uri, range, root) {
        out.push(a);
    }
    out
}

/// The M1 expression node kinds — the things that can be extracted to a local or
/// substituted in for one.
fn is_expression_kind(k: Kind) -> bool {
    matches!(
        k,
        Kind::Identifier
            | Kind::Interpolation
            | Kind::MemberExpression
            | Kind::CallExpression
            | Kind::UnaryExpression
            | Kind::BinaryExpression
            | Kind::TernaryExpression
            | Kind::ParenthesizedExpression
            | Kind::Number
            | Kind::Boolean
            | Kind::String
    )
}

/// The statement node that directly contains `n` (the child of a `SourceFile` or
/// `Block` on the path up from `n`).
fn enclosing_statement(n: Node) -> Node {
    let mut cur = n;
    while let Some(p) = cur.parent() {
        if matches!(p.kind(), Kind::SourceFile | Kind::Block) {
            return cur;
        }
        cur = p;
    }
    cur
}

/// The suppressible diagnostic code: a lint `L0xx` or typecheck `T0xx`.
/// Syntax errors (string codes like `unsupported-c-token`) carry no rule code
/// and cannot be `@m1:allow`'d.
fn suppress_code(d: &Diagnostic) -> Option<&str> {
    match &d.code {
        Some(NumberOrString::String(s))
            if s.len() == 4
                && (s.starts_with('L') || s.starts_with('T'))
                && s[1..].chars().all(|c| c.is_ascii_digit()) =>
        {
            Some(s)
        }
        _ => None,
    }
}

/// The edit inserting (or extending) the `@m1:allow` suppression for `d`.
///
/// `None` when the diagnostic is not inside a statement that starts its own
/// line — e.g. a finding on a comment-only line, at end-of-file, or on the
/// second statement of a shared line — where a leading annotation would attach
/// to a different construct and not silence the finding.
fn suppress_edit(
    text: &str,
    li: &LineIndex,
    enc: PositionEncoding,
    cst: &m1_core::Cst,
    d: &Diagnostic,
    code: &str,
) -> Option<TextEdit> {
    let offset = li.offset(d.range.start, text, enc);
    let node = cst.node_at_offset(offset);
    let stmt = enclosing_statement(node);
    if matches!(stmt.kind(), Kind::LineComment | Kind::BlockComment) {
        return None;
    }
    let stmt_range = stmt.byte_range();
    if !(stmt_range.start <= offset && offset <= stmt_range.end) {
        return None;
    }

    // The statement must start its own line (anything before it is indent).
    let stmt_line = stmt.range().start.line;
    let line_start = text[..stmt_range.start].rfind('\n').map_or(0, |i| i + 1);
    let indent = &text[line_start..stmt_range.start];
    if !indent.chars().all(|c| c == ' ' || c == '\t') {
        return None;
    }

    // An existing standalone `// @m1:allow(...)` directly above: append the
    // code to its list instead of stacking a second annotation line.
    if stmt_line > 0 {
        let prev_start = li.offset(Position::new(stmt_line - 1, 0), text, enc);
        let prev_end = line_start.saturating_sub(1); // strip the `\n`
        let prev_line = &text[prev_start..prev_end];
        let trimmed = prev_line.trim_start();
        if let Some(rest) = trimmed.strip_prefix("//")
            && let Some(args) = rest.trim_start().strip_prefix("@m1:allow(")
            && let Some(close) = args.find(')')
        {
            if args[..close].split(',').any(|c| c.trim() == code) {
                return None; // already listed — nothing useful to offer
            }
            let insert_at = prev_start
                + (prev_line.len() - prev_line.trim_start().len())
                + (trimmed.len() - args.len())
                + close;
            let pos = li.position(insert_at, enc);
            return Some(TextEdit {
                range: Range::new(pos, pos),
                new_text: format!(", {code}"),
            });
        }
    }

    let pos = Position::new(stmt_line, 0);
    Some(TextEdit {
        range: Range::new(pos, pos),
        new_text: format!("{indent}// @m1:allow({code})\n"),
    })
}

/// True when `n` lies within the `target` (lvalue) side of an assignment — a
/// position where neither extraction nor an inlined value belongs.
fn within_assignment_target(n: Node) -> bool {
    let mut cur = n;
    while let Some(p) = cur.parent() {
        if p.kind() == Kind::AssignmentStatement {
            return p
                .child_by_field(Field::Target)
                .map(|t| {
                    let (tr, nr) = (t.byte_range(), cur.byte_range());
                    tr.start <= nr.start && nr.end <= tr.end
                })
                .unwrap_or(false);
        }
        cur = p;
    }
    false
}

/// True when `n` is the `name` identifier of a `local` declaration.
fn is_local_decl_name(n: Node) -> bool {
    n.parent()
        .filter(|p| p.kind() == Kind::LocalDeclaration)
        .and_then(|p| p.child_by_field(Field::Name))
        .map(|name| name.byte_range() == n.byte_range())
        .unwrap_or(false)
}

/// A value-position expression occurrence (extractable / substitutable): an
/// expression node that is neither an lvalue target, a type-annotation name, nor
/// a `local` declaration's name.
fn is_value_expression(n: Node) -> bool {
    is_expression_kind(n.kind())
        && !within_assignment_target(n)
        && !in_type_annotation(n)
        && !is_local_decl_name(n)
}

/// A fresh local name (`myValue`, `myValue2`, …) not already declared in `root`.
fn fresh_local_name(root: Node) -> String {
    let locals = crate::features::locate::collect_locals(root);
    let base = "myValue";
    if !locals.contains_key(base) {
        return base.to_string();
    }
    (2..)
        .map(|i| format!("{base}{i}"))
        .find(|c| !locals.contains_key(c))
        .unwrap()
}

/// "Extract to local": wrap the selected expression in a `local` above its
/// statement and replace every textually-identical value-position occurrence with
/// the new name. `None` for an empty selection, a selection that doesn't cover a
/// whole expression, or an expression in lvalue / type-annotation position.
fn extract_local(
    text: &str,
    li: &LineIndex,
    enc: PositionEncoding,
    uri: &Url,
    range: Range,
    root: Node,
) -> Option<CodeActionOrCommand> {
    let mut s = li.offset(range.start, text, enc);
    let mut e = li.offset(range.end, text, enc);
    // Trim whitespace inside the selection so a trailing/leading space (common in
    // a drag-select) still lines up with an expression node's exact span.
    while s < e && text[s..].chars().next().is_some_and(char::is_whitespace) {
        s += text[s..].chars().next().unwrap().len_utf8();
    }
    while s < e
        && text[..e]
            .chars()
            .next_back()
            .is_some_and(char::is_whitespace)
    {
        e -= text[..e].chars().next_back().unwrap().len_utf8();
    }
    if s >= e {
        return None; // empty selection (a bare cursor) is not an extraction
    }

    // The outermost expression node fully covered by the trimmed selection.
    let expr = root
        .descendants()
        .filter(|n| is_expression_kind(n.kind()))
        .filter(|n| {
            let r = n.byte_range();
            r.start >= s && r.end <= e
        })
        .max_by_key(|n| n.byte_range().end - n.byte_range().start)?;
    if !is_value_expression(expr) {
        return None;
    }
    let needle = expr.text().trim().to_string();
    if needle.is_empty() {
        return None;
    }

    // Every identical value-position occurrence, earliest first.
    let mut occ: Vec<Node> = root
        .descendants()
        .filter(|n| is_value_expression(*n) && n.text().trim() == needle)
        .collect();
    occ.sort_by_key(|n| n.byte_range().start);
    occ.dedup_by_key(|n| n.byte_range().start);
    let first = *occ.first()?;

    let name = fresh_local_name(root);
    let stmt = enclosing_statement(first);
    let line = li.position(stmt.byte_range().start, enc).line;
    let indent = line_indent(text, li, enc, line);
    let insert_pos = Position::new(line, 0);

    let mut edits = vec![TextEdit {
        range: Range::new(insert_pos, insert_pos),
        new_text: format!("{indent}local {name} = {needle};\n"),
    }];
    for n in occ {
        edits.push(TextEdit {
            range: crate::convert::range(&n.byte_range(), li, enc),
            new_text: name.clone(),
        });
    }
    Some(refactor_action(
        "Extract to local",
        uri,
        edits,
        CodeActionKind::REFACTOR_EXTRACT,
    ))
}

/// "Inline local": replace each read of the cursor's `local` with its
/// initializer and delete the declaration. `None` when the cursor isn't on a
/// local, the local has no initializer, or it is reassigned anywhere (so its
/// value isn't a single constant expression).
fn inline_local(
    text: &str,
    li: &LineIndex,
    enc: PositionEncoding,
    uri: &Url,
    range: Range,
    root: Node,
) -> Option<CodeActionOrCommand> {
    let byte = li.offset(range.start, text, enc);
    let ident = crate::features::rename::local_ident_at(root, byte)?;
    let name = ident.text();

    // The declaration and its single initializer.
    let decl = root.descendants().find(|n| {
        n.kind() == Kind::LocalDeclaration
            && n.child_by_field(Field::Name).map(|nm| nm.text()) == Some(name)
    })?;
    let decl_name = decl.child_by_field(Field::Name)?;
    let init = decl.child_by_field(Field::Value)?;
    let init_text = init.text().to_string();

    // Collect read occurrences; bail if the local is ever an assignment target
    // (its value isn't a single expression we can substitute everywhere).
    let mut reads = Vec::new();
    for n in root.descendants() {
        if !crate::features::rename::is_local_ref(n, name) {
            continue;
        }
        if n.byte_range() == decl_name.byte_range() {
            continue; // the declaration's own name
        }
        if within_assignment_target(n) {
            return None;
        }
        reads.push(n);
    }

    let mut edits: Vec<TextEdit> = reads
        .iter()
        .map(|n| TextEdit {
            range: crate::convert::range(&n.byte_range(), li, enc),
            new_text: init_text.clone(),
        })
        .collect();
    // Delete the whole declaration line.
    let decl_line = li.position(decl.byte_range().start, enc).line;
    edits.push(TextEdit {
        range: Range::new(Position::new(decl_line, 0), Position::new(decl_line + 1, 0)),
        new_text: String::new(),
    });
    Some(refactor_action(
        "Inline local",
        uri,
        edits,
        CodeActionKind::REFACTOR_INLINE,
    ))
}

/// A `REFACTOR`-kind code action carrying a single-file `WorkspaceEdit`.
fn refactor_action(
    title: &str,
    uri: &Url,
    edits: Vec<TextEdit>,
    kind: CodeActionKind,
) -> CodeActionOrCommand {
    let mut changes = HashMap::new();
    changes.insert(uri.clone(), edits);
    CodeActionOrCommand::CodeAction(CodeAction {
        title: title.to_string(),
        kind: Some(kind),
        edit: Some(WorkspaceEdit {
            changes: Some(changes),
            document_changes: None,
            change_annotations: None,
        }),
        ..Default::default()
    })
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

    // --- #174 extract-local / inline-local refactors -----------------------

    const ENC: PositionEncoding = PositionEncoding::Utf16;

    /// Range spanning the first occurrence of `needle` in `src`.
    fn sel(src: &str, needle: &str, li: &LineIndex) -> Range {
        let s = src.find(needle).unwrap();
        Range::new(li.position(s, ENC), li.position(s + needle.len(), ENC))
    }

    /// A zero-width range (a cursor) at the first occurrence of `needle`.
    fn cursor(src: &str, needle: &str, li: &LineIndex) -> Range {
        let s = src.find(needle).unwrap();
        let p = li.position(s, ENC);
        Range::new(p, p)
    }

    /// The edits of the refactor action titled `title`, if present.
    fn action_edits(actions: &[CodeActionOrCommand], title: &str) -> Option<Vec<TextEdit>> {
        actions.iter().find_map(|a| match a {
            CodeActionOrCommand::CodeAction(ca) if ca.title == title => {
                ca.edit.as_ref()?.changes.as_ref()?.get(&uri()).cloned()
            }
            _ => None,
        })
    }

    /// Apply a set of (non-overlapping) text edits to `src`.
    fn apply(src: &str, edits: &[TextEdit], li: &LineIndex) -> String {
        let mut spans: Vec<(usize, usize, String)> = edits
            .iter()
            .map(|e| {
                (
                    li.offset(e.range.start, src, ENC),
                    li.offset(e.range.end, src, ENC),
                    e.new_text.clone(),
                )
            })
            .collect();
        // Apply right-to-left so earlier offsets stay valid.
        spans.sort_by(|a, b| b.0.cmp(&a.0).then(b.1.cmp(&a.1)));
        let mut out = src.to_string();
        for (s, e, t) in spans {
            out.replace_range(s..e, &t);
        }
        out
    }

    #[test]
    fn extract_local_introduces_named_intermediate() {
        let src = "Demo.Widget Count = (Demo.Raw Value - Demo.Offset) * Demo.Scale Factor;\n\
                   Demo.Mode Output = (Demo.Raw Value - Demo.Offset) * Demo.Scale Factor;\n";
        let li = LineIndex::new(src);
        let range = sel(
            src,
            "(Demo.Raw Value - Demo.Offset) * Demo.Scale Factor",
            &li,
        );
        let actions = refactors(src, &li, ENC, &uri(), range);
        let edits = action_edits(&actions, "Extract to local")
            .expect("an Extract to local action should be offered");
        let out = apply(src, &edits, &li);
        // A `local` is introduced above the first statement, and every identical
        // occurrence is replaced with the new name.
        assert_eq!(
            out,
            "local myValue = (Demo.Raw Value - Demo.Offset) * Demo.Scale Factor;\n\
             Demo.Widget Count = myValue;\n\
             Demo.Mode Output = myValue;\n",
            "got:\n{out}"
        );
    }

    #[test]
    fn extract_local_not_offered_without_a_selection() {
        let src = "Demo.Out = (a - b) * c;\n";
        let li = LineIndex::new(src);
        // A bare cursor (empty range) is not an extraction.
        let range = cursor(src, "(a - b)", &li);
        let actions = refactors(src, &li, ENC, &uri(), range);
        assert!(
            action_edits(&actions, "Extract to local").is_none(),
            "extract must require a non-empty selection"
        );
    }

    #[test]
    fn inline_local_replaces_uses_and_deletes_declaration() {
        let src = "local myValue = Demo.Sensor Value * 2.0;\n\
                   Demo.Widget Count = myValue;\n\
                   Demo.Mode Output = myValue + 1;\n";
        let li = LineIndex::new(src);
        let range = cursor(src, "myValue", &li); // on the declaration
        let actions = refactors(src, &li, ENC, &uri(), range);
        let edits = action_edits(&actions, "Inline local")
            .expect("an Inline local action should be offered");
        let out = apply(src, &edits, &li);
        assert_eq!(
            out,
            "Demo.Widget Count = Demo.Sensor Value * 2.0;\n\
             Demo.Mode Output = Demo.Sensor Value * 2.0 + 1;\n",
            "got:\n{out}"
        );
    }

    #[test]
    fn inline_local_refused_when_reassigned() {
        let src = "local myValue = 0;\n\
                   myValue = myValue + 1;\n\
                   Demo.Out = myValue;\n";
        let li = LineIndex::new(src);
        let range = cursor(src, "local myValue", &li);
        let actions = refactors(src, &li, ENC, &uri(), range);
        assert!(
            action_edits(&actions, "Inline local").is_none(),
            "a reassigned local must not be inlinable"
        );
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
        let src = "while (1) {}
";
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

    // --- #249 suppress-with-@m1:allow ---------------------------------------

    /// Build a diagnostic with `code` spanning the first occurrence of `needle`.
    fn coded_diag(src: &str, needle: &str, code: &str, li: &LineIndex) -> Diagnostic {
        let start = src.find(needle).unwrap();
        Diagnostic {
            range: Range::new(
                li.position(start, ENC),
                li.position(start + needle.len(), ENC),
            ),
            severity: Some(DiagnosticSeverity::WARNING),
            code: Some(NumberOrString::String(code.into())),
            message: "finding".into(),
            ..Default::default()
        }
    }

    /// The applied result of the "Suppress …" action, if offered.
    fn suppressed(src: &str, needle: &str, code: &str) -> Option<String> {
        let li = LineIndex::new(src);
        let d = coded_diag(src, needle, code, &li);
        let actions = code_actions(src, &li, ENC, &uri(), std::slice::from_ref(&d), None);
        let edits = action_edits(&actions, &format!("Suppress {code} for this statement"))?;
        Some(apply(src, &edits, &li))
    }

    #[test]
    fn suppress_inserts_annotation_above_statement() {
        let out = suppressed("x = 1;\ny    = 2;\n", "y", "L007").unwrap();
        assert_eq!(out, "x = 1;\n// @m1:allow(L007)\ny    = 2;\n");
    }

    #[test]
    fn suppress_matches_block_indentation() {
        let src = "if (a)\n{\n\ty = 2;\n}
";
        let out = suppressed(src, "y", "T030").unwrap();
        assert_eq!(
            out,
            "if (a)\n{\n\t// @m1:allow(T030)\n\ty = 2;\n}
"
        );
    }

    #[test]
    fn suppress_appends_to_existing_allow_line() {
        let src = "// @m1:allow(L010)\ny = 2;\n";
        let out = suppressed(src, "y", "L007").unwrap();
        assert_eq!(out, "// @m1:allow(L010, L007)\ny = 2;\n");
    }

    #[test]
    fn suppress_not_offered_when_code_already_listed() {
        assert!(suppressed("// @m1:allow(L007)\ny = 2;\n", "y", "L007").is_none());
    }

    #[test]
    fn suppress_not_offered_on_comment_lines() {
        // e.g. an L001 line-too-long on a comment-only line: a leading
        // annotation would attach to the next statement, not the comment.
        assert!(suppressed("// some very long comment\nx = 1;\n", "long", "L001").is_none());
    }

    #[test]
    fn suppress_not_offered_for_second_statement_on_a_line() {
        assert!(suppressed("x = 1; y = 2;\n", "y = 2", "L021").is_none());
    }

    #[test]
    fn suppress_not_offered_for_syntax_codes() {
        let src = "x == 1;\n";
        let li = LineIndex::new(src);
        let d = diag_for(src, "==", &li);
        let actions = code_actions(src, &li, ENC, &uri(), std::slice::from_ref(&d), None);
        assert!(
            action_edits(&actions, "Suppress unsupported-c-token for this statement").is_none()
        );
    }

    #[test]
    fn suppress_provably_silences_the_lint_finding() {
        // End-to-end through the real m1-lint engine: take a live finding,
        // apply the suppression edit, re-lint, and the finding is gone.
        use crate::analysis::LintProvider;
        let backend = crate::lint_backend::M1Lint::new();
        let src = "x = a == b;\n";
        let li = LineIndex::new(src);
        let diags = backend.lint(src, &li, ENC);
        let d = diags
            .iter()
            .find(|d| suppress_code(d) == Some("L004"))
            .expect("live L004 finding")
            .clone();
        let actions = code_actions(src, &li, ENC, &uri(), std::slice::from_ref(&d), None);
        let edits = action_edits(&actions, "Suppress L004 for this statement").unwrap();
        let fixed = apply(src, &edits, &li);
        let li2 = LineIndex::new(&fixed);
        let after = backend.lint(&fixed, &li2, ENC);
        assert!(
            !after.iter().any(|d| suppress_code(d) == Some("L004")),
            "L004 still present after suppression:\n{fixed}"
        );
    }
}
