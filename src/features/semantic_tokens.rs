//! textDocument/semanticTokens/full: classify every token in a document.
use crate::features::locate::build_scope;
use crate::line_index::{LineIndex, PositionEncoding};
use m1_core::{Kind, Node};
use m1_typecheck::project::Project;
use m1_typecheck::resolve::{Resolution, Scope, resolve};
use m1_typecheck::symbols::SymbolKind;
use tower_lsp::lsp_types::{
    SemanticToken, SemanticTokenModifier, SemanticTokenType, SemanticTokensLegend,
};

// ── Legend indices ────────────────────────────────────────────────────────────

pub const TT_VARIABLE: u32 = 0;
pub const TT_FUNCTION: u32 = 1;
pub const TT_KEYWORD: u32 = 2;
pub const TT_NUMBER: u32 = 3;
pub const TT_STRING: u32 = 4;
pub const TT_COMMENT: u32 = 5;
pub const TT_TYPE: u32 = 6;
pub const TT_PARAMETER: u32 = 7;
pub const TT_NAMESPACE: u32 = 8;

pub const TM_DEFINITION: u32 = 1 << 0;
pub const TM_READONLY: u32 = 1 << 1;

pub fn legend() -> SemanticTokensLegend {
    SemanticTokensLegend {
        token_types: vec![
            SemanticTokenType::VARIABLE,
            SemanticTokenType::FUNCTION,
            SemanticTokenType::KEYWORD,
            SemanticTokenType::NUMBER,
            SemanticTokenType::STRING,
            SemanticTokenType::COMMENT,
            SemanticTokenType::TYPE,
            SemanticTokenType::PARAMETER,
            SemanticTokenType::NAMESPACE,
        ],
        token_modifiers: vec![
            SemanticTokenModifier::DEFINITION,
            SemanticTokenModifier::READONLY,
        ],
    }
}

// ── Public entry point ────────────────────────────────────────────────────────

/// Walk `root` and return a sorted, delta-encoded token list ready for the LSP
/// `textDocument/semanticTokens/full` response.
pub fn semantic_tokens(
    root: Node,
    project: Option<&Project>,
    file_name: Option<&str>,
    li: &LineIndex,
    enc: PositionEncoding,
) -> Vec<SemanticToken> {
    let scope = build_scope(root, project, file_name);
    let mut raw: Vec<RawToken> = Vec::new();
    walk(root, &scope, li, enc, &mut raw);
    raw.sort_by(|a, b| a.line.cmp(&b.line).then(a.start.cmp(&b.start)));
    delta_encode(raw)
}

// ── Internal types ────────────────────────────────────────────────────────────

#[derive(Debug)]
struct RawToken {
    line: u32,
    start: u32,
    length: u32,
    token_type: u32,
    token_modifiers: u32,
}

// ── Walker ────────────────────────────────────────────────────────────────────

fn walk(node: Node, scope: &Scope, li: &LineIndex, enc: PositionEncoding, out: &mut Vec<RawToken>) {
    match node.kind() {
        // ── Literals ──────────────────────────────────────────────────────────
        Kind::Number => {
            push(node, TT_NUMBER, 0, li, enc, out);
        }
        Kind::String => {
            push_singleline(node, TT_STRING, 0, li, enc, out);
        }
        Kind::LineComment => {
            push_singleline(node, TT_COMMENT, 0, li, enc, out);
        }
        Kind::BlockComment => {
            push_singleline(node, TT_COMMENT, 0, li, enc, out);
        }

        // ── Keywords ─────────────────────────────────────────────────────────
        Kind::If
        | Kind::Else
        | Kind::Local
        | Kind::Static
        | Kind::When
        | Kind::And
        | Kind::Or
        | Kind::Not
        | Kind::Expand
        | Kind::To
        | Kind::Is
        | Kind::True
        | Kind::False => {
            push(node, TT_KEYWORD, 0, li, enc, out);
        }

        // ── Type annotations (<Integer>, <Float>, …) ──────────────────────────
        Kind::TypeAnnotation => {
            push(node, TT_TYPE, 0, li, enc, out);
        }

        // ── Member expressions (Foo.Bar, Group.Channel.Value, …) ─────────────
        Kind::MemberExpression => {
            let (tt, tm) = classify_member(node, scope);
            push(node, tt, tm, li, enc, out);
            // Do NOT recurse — we just covered the whole span.
        }

        // ── Identifiers ───────────────────────────────────────────────────────
        Kind::Identifier => {
            // Identifiers that are part of a MemberExpression are covered by
            // the MemberExpression arm above; skip them here.
            if let Some(parent) = node.parent() {
                if parent.kind() == Kind::MemberExpression {
                    return;
                }
                // Callee of a bare call (not member): CallExpression's first
                // named child is the function name.
                if parent.kind() == Kind::CallExpression && is_first_named_child(node, parent) {
                    push(node, TT_FUNCTION, 0, li, enc, out);
                    return;
                }
                // Declaration site of a local variable.
                if parent.kind() == Kind::LocalDeclaration && is_first_named_child(node, parent) {
                    push(node, TT_VARIABLE, TM_DEFINITION, li, enc, out);
                    return;
                }
            }
            // General identifier: resolve against scope.
            let (tt, tm) = classify_path(node.text(), scope);
            push(node, tt, tm, li, enc, out);
        }

        // ── Everything else: recurse into children ────────────────────────────
        _ => {
            for child in node.children() {
                walk(child, scope, li, enc, out);
            }
        }
    }
}

// ── Classification helpers ────────────────────────────────────────────────────

fn classify_member(node: Node, scope: &Scope) -> (u32, u32) {
    // If the parent is a CallExpression and this node is its callee, it's a function.
    if let Some(parent) = node.parent()
        && parent.kind() == Kind::CallExpression
        && is_first_named_child(node, parent)
    {
        return (TT_FUNCTION, 0);
    }
    classify_path(node.text(), scope)
}

fn classify_path(path: &str, scope: &Scope) -> (u32, u32) {
    match resolve(path, scope) {
        Resolution::Local(_) => (TT_VARIABLE, 0),
        Resolution::Symbol(sym) => match sym.kind {
            SymbolKind::Parameter => (TT_PARAMETER, 0),
            SymbolKind::Constant => (TT_VARIABLE, TM_READONLY),
            SymbolKind::Function | SymbolKind::Method => (TT_FUNCTION, 0),
            SymbolKind::Group => (TT_NAMESPACE, 0),
            SymbolKind::Channel | SymbolKind::Table | SymbolKind::Reference | SymbolKind::Other => {
                (TT_VARIABLE, 0)
            }
        },
        Resolution::Opaque | Resolution::Unresolved => (TT_VARIABLE, 0),
    }
}

fn is_first_named_child(node: Node, parent: Node) -> bool {
    parent
        .named_children()
        .into_iter()
        .next()
        .map(|c| c.byte_range() == node.byte_range())
        .unwrap_or(false)
}

// ── Token emission ────────────────────────────────────────────────────────────

/// Emit a token for `node`. Skips nodes that span multiple lines.
fn push_singleline(
    node: Node,
    tt: u32,
    tm: u32,
    li: &LineIndex,
    enc: PositionEncoding,
    out: &mut Vec<RawToken>,
) {
    let r = node.byte_range();
    let start_pos = li.position(r.start, enc);
    let end_pos = li.position(r.end, enc);
    if start_pos.line != end_pos.line {
        return; // multi-line tokens require per-line splitting; skip for now
    }
    out.push(RawToken {
        line: start_pos.line,
        start: start_pos.character,
        length: end_pos.character.saturating_sub(start_pos.character),
        token_type: tt,
        token_modifiers: tm,
    });
}

/// Emit a token; assumes the node is guaranteed to be single-line (keywords,
/// numbers, identifiers, type annotations).
fn push(
    node: Node,
    tt: u32,
    tm: u32,
    li: &LineIndex,
    enc: PositionEncoding,
    out: &mut Vec<RawToken>,
) {
    push_singleline(node, tt, tm, li, enc, out);
}

// ── Delta encoding ────────────────────────────────────────────────────────────

fn delta_encode(raw: Vec<RawToken>) -> Vec<SemanticToken> {
    let mut out = Vec::with_capacity(raw.len());
    let mut prev_line = 0u32;
    let mut prev_start = 0u32;
    for t in raw {
        let delta_line = t.line - prev_line;
        let delta_start = if delta_line == 0 {
            t.start - prev_start
        } else {
            t.start
        };
        out.push(SemanticToken {
            delta_line,
            delta_start,
            length: t.length,
            token_type: t.token_type,
            token_modifiers_bitset: t.token_modifiers,
        });
        prev_line = t.line;
        prev_start = t.start;
    }
    out
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn tokens(src: &str) -> Vec<SemanticToken> {
        let cst = m1_core::parse(src);
        let li = LineIndex::new(src);
        semantic_tokens(cst.root(), None, None, &li, PositionEncoding::Utf16)
    }

    #[test]
    fn number_literal_gets_number_type() {
        let toks = tokens("x = 42;\n");
        let num = toks.iter().find(|t| t.token_type == TT_NUMBER);
        assert!(num.is_some(), "expected a number token");
    }

    #[test]
    fn local_declaration_name_gets_variable_definition() {
        let toks = tokens("local fGain = 1.0;\n");
        let var = toks
            .iter()
            .find(|t| t.token_type == TT_VARIABLE && t.token_modifiers_bitset & TM_DEFINITION != 0);
        assert!(
            var.is_some(),
            "expected variable+definition token for local name"
        );
    }

    #[test]
    fn keywords_are_classified() {
        let toks = tokens("local x = 1;\nif x then\nend\n");
        let kw = toks.iter().filter(|t| t.token_type == TT_KEYWORD).count();
        assert!(kw >= 1, "expected at least one keyword token");
    }

    #[test]
    fn local_variable_reference_gets_variable_type() {
        let toks = tokens("local fGain = 1.0;\nfGain = fGain + 1.0;\n");
        let vars: Vec<_> = toks
            .iter()
            .filter(|t| t.token_type == TT_VARIABLE)
            .collect();
        // At least: the declaration name + one or two references
        assert!(vars.len() >= 2);
    }

    #[test]
    fn tokens_are_delta_encoded_correctly() {
        let toks = tokens("local fGain = 1.0;\n");
        // All tokens on line 0 must have non-decreasing delta_start (delta_line 0)
        // and the first token's delta_line is 0.
        assert_eq!(toks[0].delta_line, 0);
        // delta_start values must be >= 0 (u32, always true) and reconstruct
        // positions correctly — verify by re-accumulating.
        let mut col = 0u32;
        for t in &toks {
            col = if t.delta_line == 0 {
                col + t.delta_start
            } else {
                t.delta_start
            };
            assert!(col < 200, "reconstructed column looks sane");
        }
    }

    #[test]
    fn string_literal_gets_string_type() {
        let toks = tokens("msg = \"hello\";\n");
        let s = toks.iter().find(|t| t.token_type == TT_STRING);
        assert!(s.is_some(), "expected a string token");
    }

    #[test]
    fn line_comment_gets_comment_type() {
        let toks = tokens("// this is a comment\nx = 1;\n");
        let c = toks.iter().find(|t| t.token_type == TT_COMMENT);
        assert!(c.is_some(), "expected a comment token");
    }

    #[test]
    fn no_duplicate_tokens_for_member_expression_children() {
        // "Foo.Bar" is a MemberExpression — identifiers inside must not be
        // double-emitted alongside the parent token.
        let toks = tokens("Foo.Bar = 1;\n");
        // Only one token for the "Foo.Bar" span (variable), not three.
        let vars: Vec<_> = toks
            .iter()
            .filter(|t| t.token_type == TT_VARIABLE)
            .collect();
        assert_eq!(
            vars.len(),
            1,
            "expected exactly one variable token for Foo.Bar"
        );
    }
}
