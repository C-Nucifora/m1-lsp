//! The wrapped concrete syntax tree. The only module that depends on
//! `tree_sitter`; everything outside sees m1-core's own [`Cst`]/[`Node`].

use crate::diagnostic::{Code, Diagnostic, Position, Range, Severity};
use crate::field::Field;
use crate::kind::Kind;

/// A parsed M1 source file: the tree-sitter tree plus the owned source text.
#[derive(Debug)]
pub struct Cst {
    tree: tree_sitter::Tree,
    source: String,
}

/// Parse M1 source into a [`Cst`]. Infallible: grammar load is a build
/// invariant and tree-sitter always returns a tree.
pub fn parse(src: &str) -> Cst {
    let mut parser = tree_sitter::Parser::new();
    parser
        .set_language(&tree_sitter_m1::LANGUAGE.into())
        .expect("load M1 grammar");
    let tree = parser
        .parse(src, None)
        .expect("tree-sitter always returns a tree");
    Cst {
        tree,
        source: src.to_string(),
    }
}

impl Cst {
    /// The original source text.
    pub fn source(&self) -> &str {
        &self.source
    }

    /// All syntax-error diagnostics (ERROR and MISSING nodes) in this tree.
    pub fn syntax_diagnostics(&self) -> Vec<crate::diagnostic::Diagnostic> {
        crate::syntax::collect(self)
    }

    /// The root node (`source_file`).
    pub fn root(&self) -> Node<'_> {
        Node {
            inner: self.tree.root_node(),
            source: &self.source,
        }
    }

    /// The smallest node whose byte span contains `offset` (any node). Offsets
    /// past end-of-file clamp to the document length; always returns a node.
    pub fn node_at_offset(&self, offset: usize) -> Node<'_> {
        let off = offset.min(self.source.len());
        let inner = self
            .tree
            .root_node()
            .descendant_for_byte_range(off, off)
            .unwrap_or_else(|| self.tree.root_node());
        Node {
            inner,
            source: &self.source,
        }
    }

    /// The smallest *named* node whose byte span contains `offset`. Offsets past
    /// end-of-file clamp; always returns a node.
    pub fn named_node_at_offset(&self, offset: usize) -> Node<'_> {
        let off = offset.min(self.source.len());
        let inner = self
            .tree
            .root_node()
            .named_descendant_for_byte_range(off, off)
            .unwrap_or_else(|| self.tree.root_node());
        Node {
            inner,
            source: &self.source,
        }
    }
}

/// A node in the CST, wrapping a `tree_sitter::Node` plus a borrow of the
/// source so callers can get text and ranges without a separate handle.
#[derive(Debug, Clone, Copy)]
pub struct Node<'a> {
    inner: tree_sitter::Node<'a>,
    source: &'a str,
}

impl<'a> Node<'a> {
    /// The typed node kind.
    pub fn kind(&self) -> Kind {
        Kind::from_kind_str(self.inner.kind())
    }

    /// The raw tree-sitter kind string (escape hatch / `Other` recovery).
    pub fn kind_str(&self) -> &'a str {
        self.inner.kind()
    }

    /// The source text this node spans.
    pub fn text(&self) -> &'a str {
        &self.source[self.inner.byte_range()]
    }

    /// Byte offsets of this node within the source.
    pub fn byte_range(&self) -> std::ops::Range<usize> {
        self.inner.byte_range()
    }

    /// Line/column range (0-based; column is a byte offset within the line).
    pub fn range(&self) -> Range {
        let s = self.inner.start_position();
        let e = self.inner.end_position();
        Range {
            start: Position {
                line: s.row as u32,
                column: s.column as u32,
            },
            end: Position {
                line: e.row as u32,
                column: e.column as u32,
            },
        }
    }

    /// Build a diagnostic spanning exactly this node.
    pub fn diagnostic(
        &self,
        severity: Severity,
        code: Code,
        message: impl Into<String>,
    ) -> Diagnostic {
        Diagnostic::new(severity, code, self.range(), self.byte_range(), message)
    }

    /// True if this is an ERROR node.
    pub fn is_error(&self) -> bool {
        self.inner.is_error()
    }

    /// True if this is a zero-width MISSING node inserted during recovery.
    pub fn is_missing(&self) -> bool {
        self.inner.is_missing()
    }

    /// The parent node, if any.
    pub fn parent(&self) -> Option<Node<'a>> {
        self.inner.parent().map(|inner| Node {
            inner,
            source: self.source,
        })
    }

    /// The next sibling in the parent's child list, if any.
    pub fn next_sibling(&self) -> Option<Node<'a>> {
        self.inner.next_sibling().map(|inner| Node {
            inner,
            source: self.source,
        })
    }

    /// The previous sibling in the parent's child list, if any.
    pub fn prev_sibling(&self) -> Option<Node<'a>> {
        self.inner.prev_sibling().map(|inner| Node {
            inner,
            source: self.source,
        })
    }

    /// All direct children (named and anonymous).
    pub fn children(&self) -> Vec<Node<'a>> {
        let mut cursor = self.inner.walk();
        self.inner
            .children(&mut cursor)
            .map(|inner| Node {
                inner,
                source: self.source,
            })
            .collect()
    }

    /// Direct named children only (skips punctuation/keywords).
    pub fn named_children(&self) -> Vec<Node<'a>> {
        let mut cursor = self.inner.walk();
        self.inner
            .named_children(&mut cursor)
            .map(|inner| Node {
                inner,
                source: self.source,
            })
            .collect()
    }

    /// Lazy iterator over all direct children (named and anonymous).
    pub fn child_nodes(&self) -> Children<'a> {
        Children {
            parent: self.inner,
            source: self.source,
            index: 0,
            count: self.inner.child_count(),
            named_only: false,
        }
    }

    /// Lazy iterator over direct named children only.
    pub fn named_child_nodes(&self) -> Children<'a> {
        Children {
            parent: self.inner,
            source: self.source,
            index: 0,
            count: self.inner.named_child_count(),
            named_only: true,
        }
    }

    /// Pre-order iterator over this node and all of its descendants.
    pub fn descendants(&self) -> Descendants<'a> {
        Descendants {
            stack: vec![self.inner],
            source: self.source,
        }
    }

    /// The child filling the given grammar field, if present.
    pub fn child_by_field(&self, field: Field) -> Option<Node<'a>> {
        self.inner
            .child_by_field_name(field.as_str())
            .map(|inner| Node {
                inner,
                source: self.source,
            })
    }
}

/// Iterator over a node's direct children, yielded lazily. When `named_only`
/// is set, only named children are visited. Allocates nothing per element.
pub struct Children<'a> {
    parent: tree_sitter::Node<'a>,
    source: &'a str,
    index: usize,
    count: usize,
    named_only: bool,
}

impl<'a> Iterator for Children<'a> {
    type Item = Node<'a>;

    fn next(&mut self) -> Option<Node<'a>> {
        while self.index < self.count {
            let i = self.index;
            self.index += 1;
            let child = if self.named_only {
                self.parent.named_child(i)
            } else {
                self.parent.child(i)
            };
            if let Some(inner) = child {
                return Some(Node {
                    inner,
                    source: self.source,
                });
            }
        }
        None
    }
}

/// Pre-order iterator over a node and all of its descendants (node first, then
/// each child's subtree, left to right). Uses a single worklist for the whole
/// traversal rather than allocating a child vector per node.
pub struct Descendants<'a> {
    stack: Vec<tree_sitter::Node<'a>>,
    source: &'a str,
}

impl<'a> Iterator for Descendants<'a> {
    type Item = Node<'a>;

    fn next(&mut self) -> Option<Node<'a>> {
        let inner = self.stack.pop()?;
        // Push children in reverse so the leftmost is popped next (pre-order).
        let count = inner.child_count();
        for i in (0..count).rev() {
            if let Some(child) = inner.child(i) {
                self.stack.push(child);
            }
        }
        Some(Node {
            inner,
            source: self.source,
        })
    }
}

#[cfg(test)]
mod tests {
    use crate::{parse, Kind};

    #[test]
    fn parses_and_walks() {
        let cst = parse("local x = 1;\n");
        let root = cst.root();
        assert_eq!(root.kind(), Kind::SourceFile);

        let decl = root.children().into_iter().next().unwrap();
        assert_eq!(decl.kind(), Kind::LocalDeclaration);
        assert_eq!(decl.kind_str(), "local_declaration");
    }

    #[test]
    fn node_text_and_range_round_trip() {
        let src = "Ratio = 2;\n";
        let cst = parse(src);
        let assign = cst.root().children().into_iter().next().unwrap();
        let target = assign.named_children().into_iter().next().unwrap();
        assert_eq!(target.kind(), Kind::Identifier);
        assert_eq!(target.text(), "Ratio");
        assert_eq!(target.range().start.line, 0);
        assert_eq!(target.range().start.column, 0);
        assert_eq!(target.range().end.column, 5);
        assert_eq!(&src[target.byte_range()], "Ratio");
    }

    #[test]
    fn multi_word_identifier_is_one_node() {
        // Exercises the external scanner through the m1-core boundary.
        // (Identifiers here are synthetic placeholders, not from any real project.)
        let cst = parse("Vund Klee.Trilby Glonk = 1;\n");
        let assign = cst.root().children().into_iter().next().unwrap();
        let member = assign.named_children().into_iter().next().unwrap();
        assert_eq!(member.kind(), Kind::MemberExpression);
        let obj = member.named_children().into_iter().next().unwrap();
        assert_eq!(obj.text(), "Vund Klee");
    }

    #[test]
    fn child_by_field_finds_roles() {
        use crate::Field;
        let cst = parse("x = a + b;\n");
        let stmt = cst.root().children().into_iter().next().unwrap();
        assert_eq!(stmt.kind(), Kind::AssignmentStatement);
        let target = stmt.child_by_field(Field::Target).unwrap();
        assert_eq!(target.text(), "x");

        let value = stmt.child_by_field(Field::Value).unwrap();
        assert_eq!(value.kind(), Kind::BinaryExpression);
        assert_eq!(value.child_by_field(Field::Left).unwrap().text(), "a");
        assert_eq!(value.child_by_field(Field::Operator).unwrap().text(), "+");
        assert_eq!(value.child_by_field(Field::Right).unwrap().text(), "b");

        // Absent field -> None.
        assert!(stmt.child_by_field(Field::Condition).is_none());
    }

    #[test]
    fn sibling_navigation() {
        let cst = parse("x = a + b;\n");
        let stmt = cst.root().children().into_iter().next().unwrap();
        let value = {
            use crate::Field;
            stmt.child_by_field(Field::Value).unwrap()
        };
        // children of the binary expression: a, +, b
        let left = value.children().into_iter().next().unwrap();
        assert_eq!(left.text(), "a");
        let op = left.next_sibling().unwrap();
        assert_eq!(op.text(), "+");
        let right = op.next_sibling().unwrap();
        assert_eq!(right.text(), "b");
        assert!(right.next_sibling().is_none());
        assert_eq!(op.prev_sibling().unwrap().text(), "a");
        assert!(left.prev_sibling().is_none());
    }

    #[test]
    fn child_iterators_match_vec_accessors() {
        let cst = parse("if x { y = 1; }\n");
        let if_stmt = cst.root().children().into_iter().next().unwrap();

        let iter_all: Vec<_> = if_stmt.child_nodes().map(|n| n.kind()).collect();
        let vec_all: Vec<_> = if_stmt.children().iter().map(|n| n.kind()).collect();
        assert_eq!(iter_all, vec_all);

        let iter_named: Vec<_> = if_stmt.named_child_nodes().map(|n| n.kind()).collect();
        let vec_named: Vec<_> = if_stmt.named_children().iter().map(|n| n.kind()).collect();
        assert_eq!(iter_named, vec_named);

        // Iterators are non-empty for a node with children and byte-faithful.
        assert!(iter_all.len() >= iter_named.len());
        let first = if_stmt.child_nodes().next().unwrap();
        assert_eq!(first.byte_range(), if_stmt.children()[0].byte_range());
    }

    #[test]
    fn descendants_preorder_matches_recursive_walk() {
        let cst = parse("x = a + b;\n");
        let root = cst.root();

        // Reference: recursive children() walk (node first, then each subtree).
        fn rec<'a>(n: crate::Node<'a>, out: &mut Vec<(Kind, std::ops::Range<usize>)>) {
            out.push((n.kind(), n.byte_range()));
            for c in n.children() {
                rec(c, out);
            }
        }
        let mut reference = Vec::new();
        rec(root, &mut reference);

        let via_iter: Vec<_> = root
            .descendants()
            .map(|n| (n.kind(), n.byte_range()))
            .collect();
        assert_eq!(via_iter, reference);

        // Root is yielded first.
        assert_eq!(root.descendants().next().unwrap().kind(), Kind::SourceFile);
    }

    #[test]
    fn node_at_offset_finds_token() {
        let src = "Ratio = 2;\n";
        let cst = parse(src);
        // Offset 2 is inside "Ratio".
        let n = cst.node_at_offset(2);
        assert_eq!(n.kind(), Kind::Identifier);
        assert_eq!(n.text(), "Ratio");

        // Named lookup at the "2" literal.
        let two_at = src.find('2').unwrap();
        let named = cst.named_node_at_offset(two_at);
        assert_eq!(named.kind(), Kind::Number);
        assert_eq!(named.text(), "2");

        // Past EOF clamps and never panics; returns some node.
        let past = cst.node_at_offset(src.len() + 100);
        let _ = past.kind();
    }

    #[test]
    fn node_diagnostic_spans_node() {
        use crate::{Code, Severity};
        let src = "Ratio = 2;\n";
        let cst = parse(src);
        let target = cst
            .root()
            .children()
            .into_iter()
            .next()
            .unwrap()
            .named_children()
            .into_iter()
            .next()
            .unwrap();
        let d = target.diagnostic(Severity::Warning, Code::SyntaxError, "hi");
        assert_eq!(d.severity, Severity::Warning);
        assert_eq!(d.code, Code::SyntaxError);
        assert_eq!(d.message, "hi");
        assert_eq!(d.byte_range, target.byte_range());
        assert_eq!(d.range, target.range());
    }
}
