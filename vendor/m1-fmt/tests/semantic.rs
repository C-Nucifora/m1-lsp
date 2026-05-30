mod common;
use m1_core::{Kind, Node};

fn extract_semantic_tokens(src: &str) -> Vec<String> {
    let cst = m1_core::parse(src);
    let mut tokens = Vec::new();
    collect_tokens(cst.root(), &mut tokens);
    tokens
}

fn collect_tokens(node: Node, out: &mut Vec<String>) {
    if matches!(node.kind(), Kind::LineComment | Kind::BlockComment) {
        return; // skip trivia
    }
    let children = node.children();
    if children.is_empty() {
        // leaf token
        let text = node.text().trim().to_string();
        if !text.is_empty() {
            out.push(text);
        }
    } else {
        for child in children {
            collect_tokens(child, out);
        }
    }
}

#[test]
fn semantic_preservation_corpus() {
    let scripts = common::corpus_scripts();
    assert!(!scripts.is_empty(), "no corpus scripts found");

    let mut failures = Vec::new();
    for (path, src) in &scripts {
        let cst = m1_core::parse(src);
        if !cst.syntax_diagnostics().is_empty() {
            continue; // only check valid files
        }

        let result = match m1_fmt::format_str(src) {
            Ok(r) => r,
            Err(_) => continue,
        };

        let orig_tokens = extract_semantic_tokens(src);
        let fmt_tokens = extract_semantic_tokens(&result.output);

        if orig_tokens != fmt_tokens {
            failures.push(format!(
                "{}: semantic tokens changed\norig: {:?}\n fmt: {:?}",
                path.display(),
                &orig_tokens[..orig_tokens.len().min(20)],
                &fmt_tokens[..fmt_tokens.len().min(20)],
            ));
        }
    }

    if !failures.is_empty() {
        panic!("{}", failures.join("\n\n"));
    }
}
