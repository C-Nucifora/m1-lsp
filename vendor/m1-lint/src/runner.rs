//! Runner — orchestrates parsing and rule execution.

use std::path::Path;

use m1_core::Node;

use crate::diagnostic::LintDiagnostic;
use crate::registry::Registry;

/// The result of linting a single file.
#[derive(Debug, Default)]
pub struct RunResult {
    /// Diagnostics from lint rules.
    pub diagnostics: Vec<LintDiagnostic>,
    /// Syntax errors from the parser.
    pub syntax_errors: Vec<m1_core::Diagnostic>,
}

/// Runs all registered rules over a source file.
pub struct Runner {
    registry: Registry,
}

impl Runner {
    /// Construct a runner from a registry.
    pub fn new(registry: Registry) -> Self {
        Self { registry }
    }

    /// Lint a source string (no file I/O).
    pub fn run_source(&self, source: &str) -> RunResult {
        let cst = m1_core::parse(source);
        let mut result = RunResult {
            // `syntax_diagnostics` already returns an owned Vec.
            syntax_errors: cst.syntax_diagnostics(),
            ..Default::default()
        };

        // Split into lines for file-level rules.
        let lines: Vec<&str> = source.split('\n').collect();

        for rule in self.registry.rules() {
            rule.check_file(source, &lines, &mut result.diagnostics);
        }

        // Walk the CST depth-first (pre-order).
        let root = cst.root();
        self.walk(&root, source, &mut result.diagnostics);

        // Sort diagnostics by start position.
        result
            .diagnostics
            .sort_by_key(|d| (d.inner.range.start.line, d.inner.range.start.column));

        result
    }

    /// Lint a file on disk.
    pub fn run_file(&self, path: &Path) -> std::io::Result<RunResult> {
        let source = std::fs::read_to_string(path)?;
        Ok(self.run_source(&source))
    }

    /// Apply safe autofixes to a source string. See [`crate::fix::Fixer`].
    pub fn fix_source(&self, source: &str) -> Result<Option<String>, crate::fix::FixError> {
        crate::fix::Fixer::new(&self.registry).fix_source(source)
    }

    /// Apply safe autofixes to a file, writing it back when changed.
    /// Returns `Ok(true)` if the file was modified.
    pub fn fix_file(&self, path: &Path) -> std::io::Result<bool> {
        let source = std::fs::read_to_string(path)?;
        match self.fix_source(&source) {
            Ok(Some(fixed)) => {
                std::fs::write(path, fixed)?;
                Ok(true)
            }
            Ok(None) => Ok(false),
            Err(e) => Err(std::io::Error::other(e.to_string())),
        }
    }

    fn walk(&self, node: &Node, source: &str, diags: &mut Vec<LintDiagnostic>) {
        for rule in self.registry.rules() {
            rule.check_node(node, source, diags);
        }
        for child in node.children() {
            self.walk(&child, source, diags);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::Registry;

    #[test]
    fn empty_registry_no_diagnostics() {
        let runner = Runner::new(Registry::empty());
        let result = runner.run_source("x = 1;\n");
        assert!(result.diagnostics.is_empty());
    }

    #[test]
    fn runner_does_not_panic_on_empty_source() {
        let runner = Runner::new(Registry::empty());
        let result = runner.run_source("");
        assert!(result.diagnostics.is_empty());
    }

    #[test]
    fn fix_source_rewrites_eq_eq() {
        let runner = Runner::new(crate::registry::Registry::default_v2());
        let out = runner.fix_source("x = a == b;\n").unwrap();
        assert_eq!(out.as_deref(), Some("x = a eq b;\n"));
    }

    #[test]
    fn fix_source_none_when_clean() {
        let runner = Runner::new(crate::registry::Registry::default_v2());
        assert_eq!(runner.fix_source("x = a eq b;\n").unwrap(), None);
    }
}
