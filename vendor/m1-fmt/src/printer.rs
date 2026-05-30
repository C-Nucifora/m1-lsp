use crate::trivia::{collect_trivia, format_line_comment, is_eol_comment, TriviaItem};
use m1_core::{Cst, Kind, Node};
use std::collections::VecDeque;

pub struct Printer {
    indent: usize,
    output: String,
    trivia: VecDeque<TriviaItem>,
    width: usize,
    #[allow(dead_code)]
    max_blank_lines: usize,
    /// Source end line of the most recently emitted statement, used to preserve
    /// author blank lines between statements.
    prev_end_line: Option<usize>,
    /// Width (in columns) the pending end-of-line comment will occupy on the
    /// final line of the current statement. Counted against the budget only for
    /// the last element of a wrapped construct (and in the flat-vs-wrap
    /// decision), so a trailing comment can force a wrap without pushing the
    /// greedy fill of earlier lines too far left.
    eol_reserve: usize,
}

impl Printer {
    fn new(cst: &Cst, opts: &crate::FormatOptions) -> Self {
        let trivia = VecDeque::from(collect_trivia(cst));
        Self {
            indent: 0,
            output: String::new(),
            trivia,
            width: opts.line_width,
            max_blank_lines: opts.max_blank_lines,
            prev_end_line: None,
            eol_reserve: 0,
        }
    }

    /// Preserve author blank lines between the previous statement and the one
    /// starting at `start_line`. Over-runs are collapsed later by
    /// `collapse_blank_lines`; brace-adjacent blanks are stripped after that.
    fn emit_blank_gap(&mut self, start_line: usize) {
        if let Some(prev) = self.prev_end_line {
            if start_line > prev + 1 {
                for _ in 0..(start_line - prev - 1) {
                    self.emit_newline();
                }
            }
        }
    }

    fn emit(&mut self, s: &str) {
        self.output.push_str(s);
    }

    fn emit_indent(&mut self) {
        for _ in 0..self.indent {
            self.output.push_str("    ");
        }
    }

    fn emit_newline(&mut self) {
        self.output.push('\n');
    }

    /// Display column of the cursor on the current (last) physical line: the
    /// number of chars emitted since the most recent newline.
    fn current_col(&self) -> usize {
        match self.output.rfind('\n') {
            Some(i) => self.output[i + 1..].chars().count(),
            None => self.output.chars().count(),
        }
    }

    /// True if `flat`, placed starting at column `start_col`, would push the
    /// line past the configured width. Multi-line `flat` is measured by its
    /// longest constituent line (its first line offset by `start_col`).
    fn exceeds_limit(&self, start_col: usize, flat: &str) -> bool {
        let lines: Vec<&str> = flat.split('\n').collect();
        let last = lines.len() - 1;
        for (i, line) in lines.iter().enumerate() {
            let mut len = line.chars().count() + if i == 0 { start_col } else { 0 };
            // The pending EOL comment lands on the statement's final line.
            if i == last {
                len += self.eol_reserve;
            }
            if len > self.width {
                return true;
            }
        }
        false
    }

    /// Emit a continuation indent: the current block indent plus two extra
    /// 4-space units (+8), per the v2 spec §3.3.
    fn emit_continuation_indent(&mut self) {
        for _ in 0..self.indent {
            self.output.push_str("    ");
        }
        self.output.push_str("        ");
    }

    /// Render `f` into a scratch buffer at the current indent WITHOUT mutating
    /// `self.output` or consuming any trivia, and return what `f` appended.
    /// Used to measure a node's flat width before deciding whether to wrap.
    fn trial(&mut self, f: impl FnOnce(&mut Printer)) -> String {
        let mark = self.output.len();
        let saved_trivia = self.trivia.clone();
        let saved_indent = self.indent;
        let saved_prev_end_line = self.prev_end_line;
        f(self);
        let rendered = self.output[mark..].to_string();
        self.output.truncate(mark);
        self.trivia = saved_trivia;
        self.indent = saved_indent;
        self.prev_end_line = saved_prev_end_line;
        rendered
    }

    /// Emit a single own-line trivia item at the current indentation,
    /// preserving any author blank lines that precede it.
    fn emit_own_line_trivia(&mut self, item: &TriviaItem) {
        self.emit_blank_gap(item.source_line);
        self.emit_indent();
        if item.text.starts_with("//") {
            self.emit(&format_line_comment(&item.text));
        } else {
            // Block comment: re-indent continuation lines.
            self.emit_block_comment(&item.text);
        }
        self.emit_newline();
        // A block comment may span multiple source lines.
        let span = item.text.matches('\n').count();
        self.prev_end_line = Some(item.source_line + span);
    }

    /// Emit a (possibly multi-line) block comment, re-indenting continuation
    /// lines to the current depth. The first line is emitted assuming the
    /// indent has already been written by the caller.
    fn emit_block_comment(&mut self, text: &str) {
        let mut first = true;
        for line in text.split('\n') {
            if !first {
                self.emit_newline();
                let trimmed = line.trim_start();
                if !trimmed.is_empty() {
                    self.emit_indent();
                    // Conventional block-comment continuation: align ` *` under
                    // the opening `/*`.
                    if trimmed.starts_with('*') {
                        self.emit(" ");
                    }
                    self.emit(trimmed);
                }
            } else {
                self.emit(line.trim_end());
                first = false;
            }
        }
    }

    /// Consume and emit, as own-line comments, all trivia whose byte offset is
    /// before `before_byte`. Trivia that is positioned before the statement
    /// always lands on its own line (true EOL comments are attached after the
    /// statement is printed, via [`Printer::take_eol_comment`]).
    fn inject_trivia_before(&mut self, before_byte: usize) {
        while let Some(item) = self.trivia.front() {
            if item.byte_offset >= before_byte {
                break;
            }
            let item = self.trivia.pop_front().unwrap();
            self.emit_own_line_trivia(&item);
        }
    }

    /// Width of the pending EOL comment for the statement ending on
    /// `stmt_end_line`, as it will be rendered (two spaces + normalized text),
    /// or 0 if none.
    fn pending_eol_width(&self, stmt_end_line: usize) -> usize {
        if let Some(item) = self.trivia.front() {
            if is_eol_comment(item, stmt_end_line) {
                let rendered = if item.text.starts_with("//") {
                    format_line_comment(&item.text)
                } else {
                    item.text.trim_end().to_string()
                };
                return 2 + rendered.chars().count();
            }
        }
        0
    }

    /// If the next pending trivia item is on `stmt_end_line` (i.e. it trails the
    /// statement we just printed), consume and return it as an EOL comment.
    fn take_eol_comment(&mut self, stmt_end_line: usize) -> Option<TriviaItem> {
        if let Some(item) = self.trivia.front() {
            if is_eol_comment(item, stmt_end_line) {
                return self.trivia.pop_front();
            }
        }
        None
    }

    /// Flush all remaining trivia whose offset is before `before_byte` as
    /// own-line comments (used before a closing brace or end of file).
    fn flush_trivia_before(&mut self, before_byte: usize) {
        while let Some(item) = self.trivia.front() {
            if item.byte_offset >= before_byte {
                break;
            }
            let item = self.trivia.pop_front().unwrap();
            self.emit_own_line_trivia(&item);
        }
    }

    fn flush_remaining_trivia(&mut self) {
        while let Some(item) = self.trivia.pop_front() {
            self.emit_own_line_trivia(&item);
        }
    }

    /// If `eol` is present, append it as an end-of-line comment.
    fn emit_eol(&mut self, eol: Option<TriviaItem>) {
        if let Some(item) = eol {
            self.emit("  ");
            if item.text.starts_with("//") {
                self.emit(&format_line_comment(&item.text));
            } else {
                self.emit(item.text.trim_end());
            }
        }
    }

    // ---- Source file ------------------------------------------------------

    fn print_source_file(&mut self, root: Node) {
        for child in root.children() {
            self.print_top_statement(child);
        }
        self.flush_remaining_trivia();
    }

    /// A statement at file scope or inside a block: handles own-line trivia
    /// injection, indentation, the statement, and a trailing EOL comment.
    fn print_top_statement(&mut self, node: Node) {
        // Comments are extras: they may show up as direct children. Skip them;
        // they are handled via the trivia list.
        if matches!(node.kind(), Kind::LineComment | Kind::BlockComment) {
            return;
        }
        let start = node.byte_range().start;
        let start_line = node.range().start.line as usize;
        let end_line = node.range().end.line as usize;
        self.inject_trivia_before(start);
        self.emit_blank_gap(start_line);
        self.emit_indent();
        // Reserve the trailing `;` (statements that carry one) plus any EOL
        // comment, so a line that is over-budget only because of them wraps.
        let semi = usize::from(ends_with_semicolon(node.kind()));
        self.eol_reserve = self.pending_eol_width(end_line) + semi;
        self.print_statement(node);
        self.eol_reserve = 0;
        let eol = self.take_eol_comment(end_line);
        self.emit_eol(eol);
        self.emit_newline();
        self.prev_end_line = Some(end_line);
    }

    fn print_statement(&mut self, node: Node) {
        match node.kind() {
            Kind::LocalDeclaration => self.print_local_decl(node),
            Kind::AssignmentStatement => self.print_assignment(node),
            Kind::ExpressionStatement => self.print_expression_stmt(node),
            Kind::IfStatement => self.print_if(node),
            Kind::WhenStatement => self.print_when(node),
            Kind::ExpandStatement => self.print_expand(node),
            Kind::Block => self.print_bare_block(node),
            // A stray bare semicolon: preserved verbatim to keep the token
            // sequence identical (semantic-preservation invariant).
            Kind::EmptyStatement => self.emit(";"),
            _ => self.emit_verbatim(node),
        }
    }

    fn emit_verbatim(&mut self, node: Node) {
        self.emit(node.text().trim_end());
    }

    // ---- Simple statements ------------------------------------------------

    fn print_local_decl(&mut self, node: Node) {
        // Children in order: optional `static`, `local`, optional
        // type_annotation, name, optional (`=` value), `;`.
        let mut emitted_any = false;
        for child in node.children() {
            match child.kind() {
                Kind::Static => {
                    self.emit("static ");
                }
                Kind::Local => {
                    self.emit("local");
                    emitted_any = true;
                }
                Kind::TypeAnnotation => {
                    self.emit(" ");
                    self.print_type_annotation(child);
                }
                Kind::Identifier => {
                    self.emit(" ");
                    self.emit(child.text());
                }
                Kind::Assign => {
                    self.emit(" = ");
                }
                Kind::Semicolon => {
                    self.emit(";");
                }
                Kind::LineComment | Kind::BlockComment => {}
                // The value expression (any expression kind).
                _ => {
                    self.emit_expr(child);
                }
            }
        }
        let _ = emitted_any;
    }

    fn print_type_annotation(&mut self, node: Node) {
        // `<` identifier `>`
        self.emit("<");
        for child in node.children() {
            if child.kind() == Kind::Identifier {
                self.emit(child.text());
            }
        }
        self.emit(">");
    }

    fn print_assignment(&mut self, node: Node) {
        // target operator value ;
        for child in node.children() {
            match child.kind() {
                Kind::Assign | Kind::PlusEq | Kind::MinusEq | Kind::StarEq | Kind::SlashEq => {
                    self.emit(" ");
                    self.emit(child.text());
                    self.emit(" ");
                }
                Kind::Semicolon => self.emit(";"),
                Kind::LineComment | Kind::BlockComment => {}
                _ => self.emit_expr(child),
            }
        }
    }

    fn print_expression_stmt(&mut self, node: Node) {
        for child in node.children() {
            match child.kind() {
                Kind::Semicolon => self.emit(";"),
                Kind::LineComment | Kind::BlockComment => {}
                _ => self.emit_expr(child),
            }
        }
    }

    // ---- Expressions ------------------------------------------------------

    fn emit_expr(&mut self, node: Node) {
        match node.kind() {
            Kind::Identifier | Kind::Number | Kind::String | Kind::Boolean => {
                self.emit(node.text());
            }
            Kind::MemberExpression => self.emit_member(node),
            Kind::CallExpression => self.emit_call(node),
            Kind::UnaryExpression => self.emit_unary(node),
            Kind::BinaryExpression => self.emit_binary(node),
            Kind::TernaryExpression => self.emit_ternary(node),
            Kind::ParenthesizedExpression => self.emit_paren(node),
            // `true`/`false` may surface as bare keyword tokens.
            Kind::True | Kind::False => self.emit(node.text()),
            _ => self.emit_verbatim(node),
        }
    }

    fn emit_member(&mut self, node: Node) {
        // object `.` property — no spaces around the dot.
        for child in node.children() {
            match child.kind() {
                Kind::Dot => self.emit("."),
                Kind::LineComment | Kind::BlockComment => {}
                _ => self.emit_expr(child),
            }
        }
    }

    fn emit_call(&mut self, node: Node) {
        // function argument_list — no space before `(`.
        for child in node.children() {
            match child.kind() {
                Kind::ArgumentList => self.emit_arg_list(child),
                Kind::LineComment | Kind::BlockComment => {}
                _ => self.emit_expr(child),
            }
        }
    }

    fn emit_arg_list(&mut self, node: Node) {
        let start_col = self.current_col();
        let flat = self.trial(|p| p.emit_arg_list_flat(node));
        if self.exceeds_limit(start_col, &flat) {
            self.emit_arg_list_wrapped(node);
        } else {
            self.emit(&flat);
        }
    }

    /// `(` expr (`,` expr)* `)` on a single line — no padding, comma + space.
    fn emit_arg_list_flat(&mut self, node: Node) {
        self.emit("(");
        let mut first = true;
        for child in node.children() {
            match child.kind() {
                Kind::LParen | Kind::RParen => {}
                Kind::Comma => self.emit(", "),
                Kind::LineComment | Kind::BlockComment => {}
                _ => {
                    let _ = first;
                    first = false;
                    self.emit_expr(child);
                }
            }
        }
        self.emit(")");
    }

    /// Wrapped argument list: greedy fill, continuation lines at +8, no trailing
    /// comma before `)`.
    fn emit_arg_list_wrapped(&mut self, node: Node) {
        self.emit("(");
        let args: Vec<Node> = node
            .children()
            .into_iter()
            .filter(|c| {
                !matches!(
                    c.kind(),
                    Kind::LParen
                        | Kind::RParen
                        | Kind::Comma
                        | Kind::LineComment
                        | Kind::BlockComment
                )
            })
            .collect();
        let n = args.len();
        for (i, arg) in args.iter().enumerate() {
            let piece = self.trial(|p| p.emit_expr(*arg));
            let last = i + 1 == n;
            // The last argument's line also carries `)`, the trailing `;`, and
            // any EOL comment; a non-last argument is followed by a `,`. Reserve
            // for whichever applies so the break decision is honest.
            let tail = if last {
                1 + self.eol_reserve // ")" + ("; comment" already in eol_reserve)
            } else {
                1 // ","
            };
            if i == 0 {
                self.emit(&piece);
            } else {
                // We are after a prior arg; a "," was already emitted for it.
                // Decide: continue on this line (" " + piece) or break.
                let on_same = self.current_col() + 1 + piece.chars().count() + tail;
                if on_same > self.width {
                    self.emit_newline();
                    self.emit_continuation_indent();
                    self.emit(&piece);
                } else {
                    self.emit(" ");
                    self.emit(&piece);
                }
            }
            if !last {
                self.emit(",");
            }
        }
        self.emit(")");
    }

    fn emit_unary(&mut self, node: Node) {
        // operator operand. `not` is a word operator (needs a trailing space);
        // `-` and `!` bind tight to the operand.
        for child in node.children() {
            match child.kind() {
                Kind::Minus | Kind::Bang => self.emit(child.text()),
                Kind::Not => self.emit("not "),
                Kind::LineComment | Kind::BlockComment => {}
                _ => self.emit_expr(child),
            }
        }
    }

    fn emit_binary(&mut self, node: Node) {
        let start_col = self.current_col();
        let flat = self.trial(|p| p.emit_binary_flat(node));
        if self.exceeds_limit(start_col, &flat) {
            self.emit_binary_wrapped(node);
        } else {
            self.emit(&flat);
        }
    }

    fn emit_binary_flat(&mut self, node: Node) {
        let parts: Vec<Node> = node
            .children()
            .into_iter()
            .filter(|c| !matches!(c.kind(), Kind::LineComment | Kind::BlockComment))
            .collect();
        for (i, child) in parts.iter().enumerate() {
            if i == 1 {
                self.emit(" ");
                self.emit(child.text());
                self.emit(" ");
            } else {
                self.emit_expr(*child);
            }
        }
    }

    /// Flatten a left-associative binary chain into the first operand followed
    /// by (operator, operand) pairs, so we can break before each operator.
    fn flatten_binary<'a>(
        &self,
        node: Node<'a>,
        ops: &mut Vec<(Node<'a>, Node<'a>)>,
        first: &mut Option<Node<'a>>,
    ) {
        let parts: Vec<Node> = node
            .children()
            .into_iter()
            .filter(|c| !matches!(c.kind(), Kind::LineComment | Kind::BlockComment))
            .collect();
        // parts == [left, operator, right]
        let left = parts[0];
        let op = parts[1];
        let right = parts[2];
        if left.kind() == Kind::BinaryExpression {
            self.flatten_binary(left, ops, first);
        } else if first.is_none() {
            *first = Some(left);
        }
        ops.push((op, right));
    }

    fn emit_binary_wrapped(&mut self, node: Node) {
        let mut ops: Vec<(Node, Node)> = Vec::new();
        let mut first: Option<Node> = None;
        self.flatten_binary(node, &mut ops, &mut first);
        // Emit the first operand.
        if let Some(f) = first {
            self.emit_expr(f);
        }
        // Then each "op operand", breaking before the operator when the pair
        // would overflow the current line.
        let n = ops.len();
        for (idx, (op, operand)) in ops.into_iter().enumerate() {
            let op_text = op.text().to_string();
            let piece = self.trial(|p| p.emit_expr(operand));
            // The last operand's line also carries the trailing `;` and any EOL
            // comment; reserve for them on the final pair only.
            let tail = if idx + 1 == n {
                1 + self.eol_reserve
            } else {
                0
            };
            // " op operand"
            let same_line =
                self.current_col() + 1 + op_text.chars().count() + 1 + piece.chars().count() + tail;
            if same_line > self.width {
                self.emit_newline();
                self.emit_continuation_indent();
                self.emit(&op_text);
                self.emit(" ");
                self.emit(&piece);
            } else {
                self.emit(" ");
                self.emit(&op_text);
                self.emit(" ");
                self.emit(&piece);
            }
        }
    }

    fn emit_ternary(&mut self, node: Node) {
        // condition ? consequence : alternative
        for child in node.children() {
            match child.kind() {
                Kind::Question => self.emit(" ? "),
                Kind::Colon => self.emit(" : "),
                Kind::LineComment | Kind::BlockComment => {}
                _ => self.emit_expr(child),
            }
        }
    }

    fn emit_paren(&mut self, node: Node) {
        // `(` expr `)` — no padding.
        self.emit("(");
        for child in node.children() {
            match child.kind() {
                Kind::LParen | Kind::RParen => {}
                Kind::LineComment | Kind::BlockComment => {}
                _ => self.emit_expr(child),
            }
        }
        self.emit(")");
    }

    // ---- Block statements -------------------------------------------------

    /// Print a `{ ... }` block. Assumes the caller has already emitted the
    /// opening context (e.g. `if (cond) `). Emits `{`, the indented body, and
    /// the closing `}` at the current indent (without a trailing newline).
    fn print_block(&mut self, node: Node) {
        self.emit("{");
        self.emit_newline();
        self.indent += 1;
        // Measure blank gaps inside the block from the opening-brace line.
        self.prev_end_line = Some(node.range().start.line as usize);
        let rbrace = self.find_rbrace(node);
        for child in node.children() {
            match child.kind() {
                Kind::LBrace | Kind::RBrace => {}
                _ => self.print_block_statement(child),
            }
        }
        // Flush any trailing comments before the closing brace.
        if let Some(end) = rbrace {
            self.flush_trivia_before(end);
        }
        self.indent -= 1;
        self.emit_indent();
        self.emit("}");
    }

    fn find_rbrace(&self, node: Node) -> Option<usize> {
        node.children()
            .into_iter()
            .find(|c| c.kind() == Kind::RBrace)
            .map(|c| c.byte_range().start)
    }

    fn print_block_statement(&mut self, node: Node) {
        if matches!(node.kind(), Kind::LineComment | Kind::BlockComment) {
            return;
        }
        let start = node.byte_range().start;
        let start_line = node.range().start.line as usize;
        let end_line = node.range().end.line as usize;
        self.inject_trivia_before(start);
        self.emit_blank_gap(start_line);
        self.emit_indent();
        // Reserve the trailing `;` (statements that carry one) plus any EOL
        // comment, so a line that is over-budget only because of them wraps.
        let semi = usize::from(ends_with_semicolon(node.kind()));
        self.eol_reserve = self.pending_eol_width(end_line) + semi;
        self.print_statement(node);
        self.eol_reserve = 0;
        let eol = self.take_eol_comment(end_line);
        self.emit_eol(eol);
        self.emit_newline();
        self.prev_end_line = Some(end_line);
    }

    fn print_bare_block(&mut self, node: Node) {
        self.print_block(node);
    }

    fn print_if(&mut self, node: Node) {
        // `if` `(` condition `)` block else_clause?
        self.emit("if (");
        let mut seen_lparen = false;
        for child in node.children() {
            match child.kind() {
                Kind::If => {}
                Kind::LParen => {
                    seen_lparen = true;
                }
                Kind::RParen => {
                    self.emit(") ");
                }
                Kind::Block => self.print_block(child),
                Kind::ElseClause => self.print_else_clause(child),
                Kind::LineComment | Kind::BlockComment => {}
                _ => {
                    if seen_lparen {
                        // Reserve room for the trailing `) {` that follows the
                        // condition, so the wrap decision accounts for it.
                        let saved = self.width;
                        self.width = self.width.saturating_sub(3);
                        self.emit_expr(child);
                        self.width = saved;
                    }
                }
            }
        }
    }

    fn print_else_clause(&mut self, node: Node) {
        // `else` (if_statement | block)
        self.emit(" else ");
        for child in node.children() {
            match child.kind() {
                Kind::Else => {}
                Kind::Block => self.print_block(child),
                Kind::IfStatement => self.print_if(child),
                Kind::LineComment | Kind::BlockComment => {}
                _ => {}
            }
        }
    }

    fn print_when(&mut self, node: Node) {
        // `when` `(` subject `)` `{` is_clause* `}`
        self.emit("when (");
        let mut seen_lparen = false;
        let mut in_body = false;
        let rbrace = self.find_rbrace(node);
        for child in node.children() {
            match child.kind() {
                Kind::When => {}
                Kind::LParen => seen_lparen = true,
                Kind::RParen => {
                    self.emit(") ");
                }
                Kind::LBrace => {
                    self.emit("{");
                    self.emit_newline();
                    self.indent += 1;
                    in_body = true;
                }
                Kind::RBrace => {}
                Kind::IsClause => {
                    self.print_is_clause(child);
                }
                Kind::LineComment | Kind::BlockComment => {}
                _ => {
                    if seen_lparen && !in_body {
                        self.emit_expr(child);
                    }
                }
            }
        }
        if let Some(end) = rbrace {
            self.flush_trivia_before(end);
        }
        self.indent -= 1;
        self.emit_indent();
        self.emit("}");
    }

    fn print_is_clause(&mut self, node: Node) {
        // `is` `(` state `)` block
        let start = node.byte_range().start;
        let end_line = node.range().end.line as usize;
        self.inject_trivia_before(start);
        self.emit_indent();
        self.emit("is (");
        let mut seen_lparen = false;
        for child in node.children() {
            match child.kind() {
                Kind::Is => {}
                Kind::LParen => seen_lparen = true,
                Kind::RParen => self.emit(") "),
                Kind::Block => self.print_block(child),
                Kind::LineComment | Kind::BlockComment => {}
                _ => {
                    if seen_lparen {
                        self.emit_expr(child);
                    }
                }
            }
        }
        let eol = self.take_eol_comment(end_line);
        self.emit_eol(eol);
        self.emit_newline();
    }

    fn print_expand(&mut self, node: Node) {
        // `expand` `(` variable `=` start `to` end `)` block
        self.emit("expand (");
        let mut seen_lparen = false;
        let mut expr_index = 0; // 0 = variable, 1 = start, 2 = end
        for child in node.children() {
            match child.kind() {
                Kind::Expand => {}
                Kind::LParen => seen_lparen = true,
                Kind::Assign => self.emit(" = "),
                Kind::To => self.emit(" to "),
                Kind::RParen => self.emit(") "),
                Kind::Block => self.print_block(child),
                Kind::Identifier if seen_lparen && expr_index == 0 => {
                    self.emit(child.text());
                    expr_index += 1;
                }
                Kind::LineComment | Kind::BlockComment => {}
                _ => {
                    if seen_lparen {
                        self.emit_expr(child);
                        expr_index += 1;
                    }
                }
            }
        }
    }
}

/// Statement kinds whose printed form ends in a `;` on the same line as their
/// (potentially wrapped) expression.
fn ends_with_semicolon(kind: Kind) -> bool {
    matches!(
        kind,
        Kind::LocalDeclaration | Kind::AssignmentStatement | Kind::ExpressionStatement
    )
}

pub fn print(cst: &Cst) -> String {
    print_with(cst, &crate::FormatOptions::default())
}

pub fn print_with(cst: &Cst, opts: &crate::FormatOptions) -> String {
    let mut p = Printer::new(cst, opts);
    p.print_source_file(cst.root());
    normalize_trailing(&mut p.output, opts.max_blank_lines);
    strip_brace_adjacent_blanks(&mut p.output);
    p.output
}

/// Remove blank lines that immediately follow an opening `{` line or
/// immediately precede a closing `}` line, plus a leading run of blank lines at
/// the very top of the file. These never carry intent.
fn strip_brace_adjacent_blanks(output: &mut String) {
    let lines: Vec<&str> = output.split_inclusive('\n').collect();
    let is_blank = |s: &str| s.strip_suffix('\n').unwrap_or(s).trim().is_empty();
    fn trimmed_end(s: &str) -> &str {
        s.strip_suffix('\n').unwrap_or(s).trim_end()
    }
    let mut keep = vec![true; lines.len()];

    for i in 0..lines.len() {
        if !is_blank(lines[i]) {
            continue;
        }
        // Leading blanks at file top.
        let prev_nonblank = (0..i).rev().find(|&j| !is_blank(lines[j]));
        let next_nonblank = (i + 1..lines.len()).find(|&j| !is_blank(lines[j]));
        match prev_nonblank {
            None => keep[i] = false, // leading run
            Some(p) if trimmed_end(lines[p]).ends_with('{') => keep[i] = false,
            _ => {}
        }
        if let Some(n) = next_nonblank {
            if trimmed_end(lines[n]) == "}" || trimmed_end(lines[n]).starts_with('}') {
                keep[i] = false;
            }
        }
    }

    let mut result = String::with_capacity(output.len());
    for (i, line) in lines.iter().enumerate() {
        if keep[i] {
            result.push_str(line);
        }
    }
    *output = result;
}

/// Ensure exactly one final newline and collapse blank runs to `max_blank`.
fn normalize_trailing(output: &mut String, max_blank: usize) {
    collapse_blank_lines(output, max_blank);
    while output.ends_with("\n\n") {
        output.pop();
    }
    if output.is_empty() {
        return;
    }
    if !output.ends_with('\n') {
        output.push('\n');
    }
}

fn collapse_blank_lines(output: &mut String, max_blank: usize) {
    let mut result = String::with_capacity(output.len());
    let mut blank_run = 0usize;
    for line in output.split_inclusive('\n') {
        let content = line.strip_suffix('\n').unwrap_or(line);
        if content.trim().is_empty() {
            blank_run += 1;
            if blank_run <= max_blank {
                result.push_str(line);
            }
        } else {
            blank_run = 0;
            result.push_str(line);
        }
    }
    *output = result;
}

#[cfg(test)]
mod wrap_tests {
    use super::*;

    fn printer() -> Printer {
        let cst = m1_core::parse("x = 1;\n");
        Printer::new(&cst, &crate::FormatOptions::default())
    }

    #[test]
    fn current_col_counts_since_last_newline() {
        let mut p = printer();
        p.emit("abc");
        assert_eq!(p.current_col(), 3);
        p.emit_newline();
        p.emit("de");
        assert_eq!(p.current_col(), 2);
    }

    #[test]
    fn exceeds_limit_boundary() {
        let p = printer();
        // 88 chars exactly: not over. 89: over.
        let s88 = "a".repeat(88);
        let s89 = "a".repeat(89);
        assert!(!p.exceeds_limit(0, &s88));
        assert!(p.exceeds_limit(0, &s89));
        // Landing column counts toward the budget.
        assert!(p.exceeds_limit(1, &s88));
    }

    #[test]
    fn trial_does_not_mutate_output_or_trivia() {
        let cst = m1_core::parse("// c\nx = 1;\n");
        let mut p = Printer::new(&cst, &crate::FormatOptions::default());
        p.emit("before");
        let trivia_before = p.trivia.len();
        let rendered = p.trial(|p| {
            p.emit("inside");
            p.flush_remaining_trivia();
        });
        // `rendered` captures everything `f` appended (including the flushed
        // comment), but `trial` must restore `output` and `trivia` afterwards.
        assert!(rendered.starts_with("inside"));
        assert_eq!(p.output, "before");
        assert_eq!(p.trivia.len(), trivia_before);
    }

    #[test]
    fn continuation_indent_is_block_plus_eight() {
        let mut p = printer();
        p.indent = 1; // 4 spaces of block indent
        p.emit_continuation_indent();
        assert_eq!(p.output, " ".repeat(4 + 8));
    }
}
