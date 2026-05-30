use m1_core::{Cst, Kind, Node};

#[derive(Debug, Clone)]
pub struct TriviaItem {
    pub byte_offset: usize,
    pub end_offset: usize,
    pub text: String,
    pub source_line: usize,
}

pub fn collect_trivia(cst: &Cst) -> Vec<TriviaItem> {
    let mut items = Vec::new();
    collect_node(cst.root(), cst.source(), &mut items);
    items.sort_by_key(|t| t.byte_offset);
    items
}

fn collect_node(node: Node, source: &str, out: &mut Vec<TriviaItem>) {
    if matches!(node.kind(), Kind::LineComment | Kind::BlockComment) {
        let range = node.byte_range();
        let text = node.text().to_string();
        let source_line = source[..range.start].chars().filter(|&c| c == '\n').count();
        out.push(TriviaItem {
            byte_offset: range.start,
            end_offset: range.end,
            text,
            source_line,
        });
        return;
    }
    for child in node.children() {
        collect_node(child, source, out);
    }
}

/// Normalize a raw line comment: strip the `//` prefix, normalize to `// ` + body.
pub fn format_line_comment(raw: &str) -> String {
    let body = raw.strip_prefix("//").unwrap_or(raw);
    let trimmed = body.trim_start_matches(' ').trim_end();
    if trimmed.is_empty() {
        "//".to_string()
    } else {
        format!("// {}", trimmed)
    }
}

/// Determine if this trivia item should be attached as an EOL comment to the
/// statement ending at `stmt_end_line`. Returns true if the trivia is on the
/// same line.
pub fn is_eol_comment(item: &TriviaItem, stmt_end_line: usize) -> bool {
    item.source_line == stmt_end_line
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_line_comment_normalizes() {
        assert_eq!(format_line_comment("//foo"), "// foo");
        assert_eq!(format_line_comment("// foo"), "// foo");
        assert_eq!(format_line_comment("//  foo"), "// foo");
        assert_eq!(format_line_comment("//   foo"), "// foo");
        assert_eq!(format_line_comment("//"), "//");
    }

    #[test]
    fn collect_trivia_finds_comments() {
        let src = "// header\nx = 1;\n";
        let cst = m1_core::parse(src);
        let trivia = collect_trivia(&cst);
        assert_eq!(trivia.len(), 1);
        assert_eq!(trivia[0].text, "// header");
        assert_eq!(trivia[0].source_line, 0);
    }

    #[test]
    fn collect_trivia_eol_comment() {
        let src = "x = 1; // note\n";
        let cst = m1_core::parse(src);
        let trivia = collect_trivia(&cst);
        assert_eq!(trivia.len(), 1);
        assert_eq!(trivia[0].source_line, 0); // same line as statement
    }
}
