//! L005 — logical-operator-preferred
//!
//! Flags `&&`, `||`, `!`; the project prefers `and`, `or`, `not`.

use crate::diagnostic::{LintCode, LintDiagnostic};
use crate::rules::Rule;
use m1_core::{Kind, Node, Severity};

/// L005 — flags symbolic logical operators `&&`, `||`, `!`.
pub struct LogicalOperatorPreferred;

impl Rule for LogicalOperatorPreferred {
    fn code(&self) -> LintCode {
        LintCode::L005
    }
    fn name(&self) -> &'static str {
        "logical-operator-preferred"
    }

    fn check_node(&self, node: &Node, _source: &str, diags: &mut Vec<LintDiagnostic>) {
        // Binary: && and ||
        if node.kind() == Kind::BinaryExpression {
            for child in node.children() {
                let msg = match child.kind() {
                    Kind::AmpAmp => "use `and` instead of `&&`",
                    Kind::PipePipe => "use `or` instead of `||`",
                    _ => continue,
                };
                diags.push(LintDiagnostic::new(
                    LintCode::L005,
                    child.range(),
                    child.byte_range(),
                    Severity::Warning,
                    msg,
                ));
            }
        }

        // Unary: !
        if node.kind() == Kind::UnaryExpression {
            for child in node.children() {
                if child.kind() == Kind::Bang {
                    diags.push(LintDiagnostic::new(
                        LintCode::L005,
                        child.range(),
                        child.byte_range(),
                        Severity::Warning,
                        "use `not` instead of `!`",
                    ));
                }
            }
        }
    }

    fn fix_node(&self, node: &m1_core::Node, source: &str, edits: &mut Vec<crate::fix::Edit>) {
        if node.kind() == Kind::BinaryExpression {
            for child in node.children() {
                let repl = match child.kind() {
                    Kind::AmpAmp => "and",
                    Kind::PipePipe => "or",
                    _ => continue,
                };
                edits.push(crate::fix::Edit {
                    byte_range: child.byte_range(),
                    replacement: repl.into(),
                });
            }
        }
        if node.kind() == Kind::UnaryExpression {
            for child in node.children() {
                if child.kind() == Kind::Bang {
                    // `!x` -> `not x`: ensure a separating space so tokens split.
                    let br = child.byte_range();
                    let next = source.as_bytes().get(br.end).copied();
                    let needs_space = !matches!(next, Some(b' ') | Some(b'(') | None);
                    let replacement = if needs_space { "not " } else { "not" };
                    // Skip the ambiguous `!!a` case (double negation).
                    if next == Some(b'!') {
                        continue;
                    }
                    edits.push(crate::fix::Edit { byte_range: br, replacement: replacement.into() });
                }
            }
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
        r.register(Box::new(LogicalOperatorPreferred));
        Runner::new(r)
    }

    #[test]
    fn flags_amp_amp() {
        let source = "x = a && b;\n";
        let result = runner().run_source(source);
        assert_eq!(result.diagnostics.len(), 1);
        assert!(result.diagnostics[0].inner.message.contains("and"));
    }

    #[test]
    fn flags_pipe_pipe() {
        let source = "x = a || b;\n";
        let result = runner().run_source(source);
        assert_eq!(result.diagnostics.len(), 1);
        assert!(result.diagnostics[0].inner.message.contains("or"));
    }

    #[test]
    fn flags_bang() {
        let source = "x = !a;\n";
        let result = runner().run_source(source);
        assert_eq!(result.diagnostics.len(), 1);
        assert!(result.diagnostics[0].inner.message.contains("not"));
    }

    #[test]
    fn no_false_positive_and_or_not() {
        let source = "x = a and b;\ny = c or d;\nz = not e;\n";
        let result = runner().run_source(source);
        assert!(result.diagnostics.iter().all(|d| d.code != LintCode::L005));
    }

    #[test]
    fn fixes_amp_amp() {
        let mut r = Registry::empty();
        r.register(Box::new(LogicalOperatorPreferred));
        let fixer = crate::fix::Fixer::new(&r);
        let out = fixer.fix_source("x = a && b;\n").unwrap();
        assert_eq!(out.as_deref(), Some("x = a and b;\n"));
    }
}
