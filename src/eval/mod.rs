// SPDX-License-Identifier: GPL-3.0-or-later
//! Evaluation support: a thin view over a single cached [`m1_eval::Trace`].
//!
//! This module integrates [`m1-eval`](https://github.com/C-Nucifora/m1-eval) so
//! hover and inlay hints can surface a channel's *evaluated value* alongside the
//! existing type/symbol information. The LSP runs no second engine and writes no
//! evaluation logic of its own — it caches a `Trace` and renders it.
//!
//! It re-exports the m1-eval types the later milestones build on, and owns the
//! LSP-local [`EvalConfig`] sourced from `m1.eval.*` editor settings. With eval
//! disabled or unavailable (the default), hover and inlay behave exactly as
//! today.

pub mod config;
pub mod engine;
pub mod render;

pub use config::{EvalConfig, TickPolicy};
pub use engine::{EvalOutcome, Provenance, evaluate};
pub use render::{eval_expr_fragment, eval_hover_fragment, value_markdown};

/// Re-exports of the m1-eval public surface the LSP integration builds on.
///
/// Centralising the imports here keeps the dependency boundary explicit: every
/// type the integration touches is one of m1-eval's own (`Engine`, `Scenario`,
/// `Trace`, `Value`, `EvalError`), with no `m1-core`/`m1-typecheck` type leaks.
pub use m1_eval::{Engine, EvalError, RunMode, Scenario, Trace, Value};

#[cfg(test)]
mod tests {
    //! Compile-level reachability checks for the m1-eval surface.
    //!
    //! These assert that the dependency resolves and the types the later
    //! milestones need are nameable; they exercise no evaluation behaviour.

    use super::{Engine, Scenario, Trace, Value};

    /// `Engine`, `Scenario`, `Trace` and `Value` are reachable through the
    /// crate-local re-export, proving the dependency builds and links.
    #[test]
    fn eval_types_are_reachable() {
        // Naming each type in a turbofish-free position is enough to require it
        // to exist and resolve at compile time. We also touch a value-level use
        // of `Value` so the type is exercised, not merely named.
        fn _assert_named<T>() {}
        _assert_named::<Engine>();
        _assert_named::<Scenario>();
        _assert_named::<Trace>();
        _assert_named::<Value>();

        let v = Value::Int(50);
        assert!(matches!(v, Value::Int(50)));
    }

    /// The re-exports are also reachable via the original `m1_eval` crate path,
    /// confirming we depend on the crate directly (not a vendored copy).
    #[test]
    fn eval_crate_path_is_reachable() {
        fn _assert_named<T>() {}
        _assert_named::<m1_eval::Engine>();
        _assert_named::<m1_eval::Scenario>();
        _assert_named::<m1_eval::Trace>();
        _assert_named::<m1_eval::Value>();
    }
}
