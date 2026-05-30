//! Rule trait, registry, and the single-walk runner.
use crate::diagnostics::{CheckResult, TypeDiagnostic};
use crate::project::Project;
use crate::resolve::Scope;
use m1_core::Node;
use std::path::Path;

pub mod t001_unresolved;
pub mod t002_float_eq;
pub mod t003_int_float_mix;
pub mod t010_local_prefix;
pub mod t011_prefix_mismatch;
pub mod t020_enum_member;
pub mod t021_enum_numeric_cmp;
pub mod t030_assign_mismatch;

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
                Box::new(t010_local_prefix::Rule),
                Box::new(t011_prefix_mismatch::Rule),
            ],
        }
    }
    pub fn default_v2() -> Self {
        Registry {
            rules: vec![
                Box::new(t001_unresolved::Rule),
                Box::new(t002_float_eq::Rule),
                Box::new(t003_int_float_mix::Rule),
                Box::new(t010_local_prefix::Rule),
                Box::new(t011_prefix_mismatch::Rule),
                Box::new(t020_enum_member::Rule),
                Box::new(t021_enum_numeric_cmp::Rule),
                Box::new(t030_assign_mismatch::Rule),
            ],
        }
    }
    pub fn rules(&self) -> &[Box<dyn Rule>] {
        &self.rules
    }
}

/// Collect locals (name -> type) declared anywhere in the script.
fn collect_locals(root: Node) -> std::collections::HashMap<String, crate::types::ValueType> {
    use crate::types::{type_from_hungarian, ValueType};
    let mut locals = std::collections::HashMap::new();
    fn walk(n: Node, locals: &mut std::collections::HashMap<String, ValueType>) {
        if n.kind() == m1_core::Kind::LocalDeclaration {
            if let Some(name_node) = n
                .named_children()
                .into_iter()
                .find(|c| c.kind() == m1_core::Kind::Identifier)
            {
                let name = name_node.text().to_string();
                let t = type_from_hungarian(&name).unwrap_or(ValueType::Unknown);
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
        // local with proper prefix, simple int math -> no T010/T011/T002/T003
        let r = check_script_no_project("local iSum = 1 + 2;\n");
        assert!(r.syntax_errors.is_empty());
        assert!(r
            .diagnostics
            .iter()
            .all(|d| d.code != crate::diagnostics::TypeCode::T010));
    }
}
