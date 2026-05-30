//! L008 — nesting-too-deep
//!
//! Control-flow constructs nested more than 4 levels deep are flagged.
//!
//! The M1 grammar's control constructs are `if` and `when`; there is no
//! `for`/`while`. Depth is computed by counting control-node ancestors.

use crate::diagnostic::{LintCode, LintDiagnostic};
use crate::rules::Rule;
use m1_core::{Kind, Node, Severity};

fn is_control_node(kind: Kind) -> bool {
    matches!(kind, Kind::IfStatement | Kind::WhenStatement)
}

fn nesting_depth(node: &Node) -> usize {
    let mut depth = 0usize;
    let mut current = node.parent();
    while let Some(parent) = current {
        if is_control_node(parent.kind()) {
            depth += 1;
        }
        current = parent.parent();
    }
    depth
}

/// L008 — flags control structures nested deeper than `max_depth` levels.
pub struct NestingTooDeep {
    pub max_depth: usize,
}

impl Default for NestingTooDeep {
    fn default() -> Self {
        Self { max_depth: 4 }
    }
}

impl Rule for NestingTooDeep {
    fn code(&self) -> LintCode {
        LintCode::L008
    }
    fn name(&self) -> &'static str {
        "nesting-too-deep"
    }

    fn check_node(&self, node: &Node, _source: &str, diags: &mut Vec<LintDiagnostic>) {
        if !is_control_node(node.kind()) {
            return;
        }
        let depth = nesting_depth(node) + 1; // +1 for this node itself
        if depth > self.max_depth {
            diags.push(LintDiagnostic::new(
                LintCode::L008,
                node.range(),
                node.byte_range(),
                Severity::Warning,
                format!("nesting depth {} exceeds maximum of {}", depth, self.max_depth),
            ));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::Registry;
    use crate::runner::Runner;

    fn runner() -> Runner {
        let mut r = Registry::empty();
        r.register(Box::new(NestingTooDeep::default()));
        Runner::new(r)
    }

    /// Build `depth` nested `if (a) { ... }` blocks.
    fn nested_ifs(depth: usize) -> String {
        let mut s = String::new();
        for _ in 0..depth {
            s.push_str("if (a) {\n");
        }
        s.push_str("x = 1;\n");
        for _ in 0..depth {
            s.push_str("}\n");
        }
        s
    }

    #[test]
    fn no_diagnostic_at_depth_4() {
        let source = nested_ifs(4);
        let result = runner().run_source(&source);
        assert!(result.diagnostics.iter().all(|d| d.code != LintCode::L008));
    }

    #[test]
    fn diagnostic_at_depth_5() {
        let source = nested_ifs(5);
        let result = runner().run_source(&source);
        assert!(result.diagnostics.iter().any(|d| d.code == LintCode::L008));
    }
}
