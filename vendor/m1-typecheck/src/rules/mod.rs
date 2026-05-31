//! Rule trait, registry, and the single-walk runner.
use crate::diagnostics::{CheckResult, TypeDiagnostic};
use crate::project::Project;
use crate::resolve::Scope;
use m1_core::Node;
use std::path::Path;

pub mod t001_unresolved;
pub mod t002_float_eq;
pub mod t003_int_float_mix;
pub mod t020_enum_member;
pub mod t021_enum_numeric_cmp;
pub mod t030_assign_mismatch;
pub mod t060_stateful_conditional;
pub mod t061_integrated_only;
pub mod t062_deprecated_overload;

pub trait Rule: Send + Sync {
    fn check_node(&self, node: &Node, scope: &Scope, out: &mut Vec<TypeDiagnostic>);
}

pub struct Registry {
    rules: Vec<Box<dyn Rule>>,
}

impl Registry {
    pub fn default_v1() -> Self {
        Registry {
            rules: vec![
                Box::new(t001_unresolved::Rule),
                Box::new(t002_float_eq::Rule),
                Box::new(t003_int_float_mix::Rule),
                Box::new(t060_stateful_conditional::Rule),
                Box::new(t061_integrated_only::Rule),
                Box::new(t062_deprecated_overload::Rule),
            ],
        }
    }
    pub fn default_v2() -> Self {
        Registry {
            rules: vec![
                Box::new(t001_unresolved::Rule),
                Box::new(t002_float_eq::Rule),
                Box::new(t003_int_float_mix::Rule),
                Box::new(t020_enum_member::Rule),
                Box::new(t021_enum_numeric_cmp::Rule),
                Box::new(t030_assign_mismatch::Rule),
                Box::new(t060_stateful_conditional::Rule),
                Box::new(t061_integrated_only::Rule),
                Box::new(t062_deprecated_overload::Rule),
            ],
        }
    }
    pub fn rules(&self) -> &[Box<dyn Rule>] {
        &self.rules
    }
}

/// Collect locals (name -> type) declared anywhere in the script. The type is
/// inferred from the declaration's initialiser via the expression typer, using an
/// empty scope to avoid cross-local ordering hazards.
fn collect_locals(root: Node) -> std::collections::HashMap<String, crate::types::ValueType> {
    use crate::types::ValueType;
    let mut locals = std::collections::HashMap::new();
    fn walk(n: Node, locals: &mut std::collections::HashMap<String, ValueType>) {
        if n.kind() == m1_core::Kind::LocalDeclaration {
            if let Some(name_node) = n
                .named_children()
                .into_iter()
                .find(|c| c.kind() == m1_core::Kind::Identifier)
            {
                let name = name_node.text().to_string();
                let initializer = n.named_children().into_iter().find(|c| {
                    c.kind() != m1_core::Kind::Identifier
                        && c.kind() != m1_core::Kind::TypeAnnotation
                });
                let t = match initializer {
                    Some(init) => crate::typer::type_of(
                        init,
                        &Scope {
                            locals: std::collections::HashMap::new(),
                            group: None,
                            project: None,
                        },
                    ),
                    None => ValueType::Unknown,
                };
                locals.insert(name, t);
            }
        }
        for c in n.children() {
            walk(c, locals);
        }
    }
    walk(root, &mut locals);
    locals
}

/// Check one script. `script_path` (file name) anchors group-relative resolution.
pub fn check_script(project: &Project, script_path: &Path, source: &str) -> CheckResult {
    run(
        Some(project),
        script_path.file_name().and_then(|s| s.to_str()),
        source,
    )
}

pub fn check_script_no_project(source: &str) -> CheckResult {
    run(None, None, source)
}

fn run(project: Option<&Project>, file_name: Option<&str>, source: &str) -> CheckResult {
    let cst = m1_core::parse(source);
    let syntax_errors = cst.syntax_diagnostics();
    if !syntax_errors.is_empty() {
        return CheckResult {
            diagnostics: vec![],
            syntax_errors,
        };
    }
    let group = match (project, file_name) {
        (Some(p), Some(f)) => p.group_for_script(f),
        _ => None,
    };
    let scope = Scope {
        locals: collect_locals(cst.root()),
        group,
        project,
    };
    let registry = Registry::default_v2();
    let mut diagnostics = Vec::new();
    walk(cst.root(), &registry, &scope, &mut diagnostics);
    crate::flow::single_assignment(cst.root(), &scope, &mut diagnostics);
    CheckResult {
        diagnostics,
        syntax_errors,
    }
}

fn walk(node: Node, registry: &Registry, scope: &Scope, out: &mut Vec<TypeDiagnostic>) {
    for rule in registry.rules() {
        rule.check_node(&node, scope, out);
    }
    for c in node.children() {
        walk(c, registry, scope, out);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn clean_script_no_project_has_no_diagnostics_for_resolution() {
        // simple int math -> no T002/T003 and no spurious diagnostics
        let r = check_script_no_project("local sum = 1 + 2;\n");
        assert!(r.syntax_errors.is_empty());
        assert!(
            r.diagnostics.is_empty(),
            "expected no diagnostics, got {:?}",
            r.diagnostics
        );
    }
}
