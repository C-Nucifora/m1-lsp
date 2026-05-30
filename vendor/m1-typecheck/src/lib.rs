//! m1-typecheck: symbol model + name resolution + basic type checking for .m1scr.
pub mod audit;
pub mod diagnostics;
pub mod flow;
pub mod project;
pub mod resolve;
pub mod rules;
pub mod symbols;
pub mod typer;
pub mod types;

pub use diagnostics::{CheckResult, TypeCode, TypeDiagnostic};
pub use project::Project;
pub use types::ValueType;
