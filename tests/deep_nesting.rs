//! Regression for #133: the CST-walking features must not overflow the call
//! stack on a pathologically deep document. tree-sitter parses such a document
//! iteratively and returns a tree whose depth equals the nesting depth; the old
//! recursive feature walkers then blew the thread stack and aborted the whole
//! server with SIGABRT (uncatchable). The walkers are now iterative, so the
//! entry points below simply RETURN — and because a stack overflow *aborts* the
//! process, a test that calls them and reaches its assertions IS the proof.
//!
//! These run on the default test-harness thread stack (no oversized stack to
//! mask the bug): pre-fix, a depth-18000 doc already aborted on a 2 MiB thread,
//! so the 50_000 depth used here is comfortably past the failure point.
use m1_lsp::features::{folding, locate, references, semantic_tokens};
use m1_lsp::line_index::{LineIndex, PositionEncoding};
use tower_lsp::lsp_types::Url;

/// `x = ` + `(` * depth + `1` + `)` * depth + `;` — a single expression nested
/// `depth` levels deep, the exact shape from the audit repro.
fn deeply_nested(depth: usize) -> String {
    let mut s = String::with_capacity(depth * 2 + 8);
    s.push_str("x = ");
    for _ in 0..depth {
        s.push('(');
    }
    s.push('1');
    for _ in 0..depth {
        s.push(')');
    }
    s.push_str(";\n");
    s
}

// Depth for the O(n) walkers (semantic tokens, folding, collect_locals): well
// past where the recursive form aborted, and cheap to run.
const DEPTH: usize = 50_000;
// The reference walks additionally climb parents per node (`in_type_annotation`,
// `top_path_node`) — unchanged O(n²) behaviour — so a slightly smaller depth
// keeps the test quick while staying above the ~18_000 pre-fix abort point.
const REF_DEPTH: usize = 20_000;

#[test]
fn semantic_tokens_does_not_overflow_on_deep_input() {
    let src = deeply_nested(DEPTH);
    let cst = m1_core::parse(&src);
    let li = LineIndex::new(&src);
    // Returning at all is the proof: a stack overflow would have aborted.
    let toks =
        semantic_tokens::semantic_tokens(cst.root(), None, None, &li, PositionEncoding::Utf16);
    // The lone literal is still classified.
    assert!(!toks.is_empty(), "expected at least the `1` token");
}

#[test]
fn folding_does_not_overflow_on_deep_input() {
    let src = deeply_nested(DEPTH);
    let cst = m1_core::parse(&src);
    let li = LineIndex::new(&src);
    let _ = folding::folding_ranges(cst.root(), &li, PositionEncoding::Utf16);
}

#[test]
fn references_do_not_overflow_on_deep_input() {
    let src = deeply_nested(REF_DEPTH);
    let cst = m1_core::parse(&src);
    let li = LineIndex::new(&src);
    let uri = Url::parse("file:///deep.m1scr").unwrap();
    // Cursor on the `x` target — exercises the path/local occurrence walks.
    let _ = references::references(cst.root(), 0, uri, &li, PositionEncoding::Utf16);
}

#[test]
fn collect_locals_does_not_overflow_on_deep_input() {
    let src = deeply_nested(DEPTH);
    let cst = m1_core::parse(&src);
    let _ = locate::collect_locals(cst.root());
}

#[test]
fn small_doc_output_is_unchanged() {
    // The iterative walkers must produce the same result as before on a normal
    // document: a couple of locals, a few tokens, no folds for one-liners.
    let src = "local fGain = 1.0;\nfGain = fGain + 1;\n";
    let cst = m1_core::parse(src);
    let li = LineIndex::new(src);

    let locals = locate::collect_locals(cst.root());
    assert!(
        locals.contains_key("fGain"),
        "fGain local must be collected"
    );

    let toks =
        semantic_tokens::semantic_tokens(cst.root(), None, None, &li, PositionEncoding::Utf16);
    assert!(!toks.is_empty(), "small doc still yields tokens");

    let uri = Url::parse("file:///t.m1scr").unwrap();
    let refs = references::references(
        cst.root(),
        src.find("fGain").unwrap(),
        uri,
        &li,
        PositionEncoding::Utf16,
    )
    .expect("fGain has references");
    assert_eq!(refs.len(), 3, "declaration + two uses, as before");
}
