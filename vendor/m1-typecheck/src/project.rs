//! The loaded project: symbol table + enum types + opaque roots + file->group.
use crate::symbols::{m1cfg, m1prj, SymbolTable};
use std::collections::{HashMap, HashSet};
use std::path::Path;

/// M1 standard-library roots always treated as opaque (never flagged).
const STATIC_OPAQUE_ROOTS: &[&str] = &[
    "This",
    "Calculate",
    "Limit",
    "Convert",
    "Math",
    "Time",
    "Filter",
    "CanComms",
    "Table",
    "Channel",
    "Constant",
    "Parameter",
    "Diagnostic",
];

pub struct Project {
    table: SymbolTable,
    opaque_roots: HashSet<String>,
    file_to_group: HashMap<String, String>,
}

#[derive(Debug)]
pub enum LoadError {
    Io(std::io::Error),
    Parse(String),
}
impl std::fmt::Display for LoadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LoadError::Io(e) => write!(f, "{e}"),
            LoadError::Parse(s) => write!(f, "{s}"),
        }
    }
}
impl std::error::Error for LoadError {}

impl Project {
    pub fn load(m1prj_path: &Path) -> Result<Project, LoadError> {
        let xml = std::fs::read_to_string(m1prj_path).map_err(LoadError::Io)?;
        let parsed = m1prj::parse(&xml).map_err(|e| LoadError::Parse(e.to_string()))?;
        let mut opaque_roots: HashSet<String> =
            STATIC_OPAQUE_ROOTS.iter().map(|s| s.to_string()).collect();
        opaque_roots.extend(parsed.module_roots);
        Ok(Project {
            table: parsed.table,
            opaque_roots,
            file_to_group: parsed.file_to_group,
        })
    }

    pub fn with_config(mut self, m1cfg_path: &Path) -> Result<Project, LoadError> {
        let xml = std::fs::read_to_string(m1cfg_path).map_err(LoadError::Io)?;
        m1cfg::augment(&mut self.table, &xml).map_err(|e| LoadError::Parse(e.to_string()))?;
        Ok(self)
    }

    pub fn symbols(&self) -> &SymbolTable {
        &self.table
    }

    pub fn is_opaque_root(&self, root: &str) -> bool {
        self.opaque_roots.contains(root)
    }

    /// Enclosing group for a script file name (the bare `Filename`, e.g.
    /// "Foo Update.m1scr"); None if the project has no such function/method.
    pub fn group_for_script(&self, file_name: &str) -> Option<String> {
        self.file_to_group.get(file_name).cloned()
    }

    /// Audit the project's own symbol names (T050).
    pub fn audit(&self) -> Vec<crate::diagnostics::TypeDiagnostic> {
        crate::audit::audit_project(self)
    }
}
