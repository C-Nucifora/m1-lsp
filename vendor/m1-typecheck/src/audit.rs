//! Project-wide symbol-name audit (T050) against the EV-M1 naming conventions.
use crate::diagnostics::{make_project, TypeCode, TypeDiagnostic};
use crate::project::Project;
use crate::symbols::SymbolKind;
use m1_core::Severity;

/// Audit the project's own symbol + enum-type names. Empty unless violations.
pub fn audit_project(project: &Project) -> Vec<TypeDiagnostic> {
    let table = project.symbols();
    let mut out = Vec::new();

    for sym in table.iter() {
        let leaf = leaf_of(&sym.path);
        let conv = match sym.kind {
            SymbolKind::Channel => Some(("lowerCamelCase", lower_camel(leaf))),
            SymbolKind::Parameter | SymbolKind::Group => {
                Some(("UpperCamelCase", upper_camel(leaf)))
            }
            SymbolKind::Constant => Some(("CAPITALISATION", capitalised(leaf))),
            SymbolKind::Function | SymbolKind::Method => {
                Some(("lowerCamelCase", lower_camel(leaf)))
            }
            _ => None,
        };
        if let Some((name, ok)) = conv {
            if !ok {
                out.push(make_project(
                    TypeCode::T050,
                    Severity::Warning,
                    format!(
                        "{:?} `{leaf}` does not follow {name} (in `{}`)",
                        sym.kind, sym.path
                    ),
                ));
            }
        }
    }
    for e in table.enums() {
        if !upper_camel(&e.name) {
            out.push(make_project(
                TypeCode::T050,
                Severity::Warning,
                format!("enum type `{}` does not follow UpperCamelCase", e.name),
            ));
        }
    }
    out
}

fn leaf_of(path: &str) -> &str {
    path.rsplit_once('.').map(|(_, l)| l).unwrap_or(path)
}

fn lower_camel(s: &str) -> bool {
    let Some(c) = s.chars().next() else {
        return false;
    };
    c.is_ascii_lowercase() && !s.contains(' ') && !s.contains('_')
}
fn upper_camel(s: &str) -> bool {
    let Some(c) = s.chars().next() else {
        return false;
    };
    c.is_ascii_uppercase() && !s.contains(' ') && !s.contains('_')
}
/// CAPITALISATION: letters are upper-case; spaces and digits allowed (per
/// CONTRIBUTING "Constant names may use spaces").
fn capitalised(s: &str) -> bool {
    s.chars().any(|c| c.is_ascii_alphabetic())
        && s.chars()
            .filter(|c| c.is_ascii_alphabetic())
            .all(|c| c.is_ascii_uppercase())
}
