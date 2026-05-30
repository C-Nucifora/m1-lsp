//! `m1-core` — shared foundation for the MoTeC M1 (.m1scr) tooling.
//!
//! v1 provides the syntactic layer: [`parse`] returns a [`Cst`] that wraps the
//! tree-sitter tree behind m1-core's own [`Node`]/[`Kind`] types, plus a shared
//! [`Diagnostic`] type and syntax-error reporting. tree-sitter is an
//! implementation detail and is never exposed to consumers.

mod kind;
mod field;
mod diagnostic;
mod cst;
mod syntax;

pub use kind::Kind;
pub use field::Field;
pub use diagnostic::{Code, Diagnostic, Position, Range, Severity};
pub use cst::{parse, Children, Cst, Descendants, Node};
