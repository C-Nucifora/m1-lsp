//! Rust bindings to the M1 tree-sitter grammar.
//!
//! ```no_run
//! let mut parser = tree_sitter::Parser::new();
//! parser.set_language(&tree_sitter_m1::LANGUAGE.into()).unwrap();
//! ```

use tree_sitter_language::LanguageFn;

extern "C" {
    fn tree_sitter_m1() -> *const ();
}

/// The tree-sitter [`LanguageFn`] for the M1 grammar.
pub const LANGUAGE: LanguageFn = unsafe { LanguageFn::from_raw(tree_sitter_m1) };

/// Syntax-highlighting query source (`queries/highlights.scm`).
pub const HIGHLIGHTS_QUERY: &str = include_str!("../../queries/highlights.scm");
/// Indentation query source (`queries/indents.scm`).
pub const INDENTS_QUERY: &str = include_str!("../../queries/indents.scm");
/// Folding query source (`queries/folds.scm`).
pub const FOLDS_QUERY: &str = include_str!("../../queries/folds.scm");
/// Scope/definition/reference query source (`queries/locals.scm`).
pub const LOCALS_QUERY: &str = include_str!("../../queries/locals.scm");
/// Language-injection query source (`queries/injections.scm`).
pub const INJECTIONS_QUERY: &str = include_str!("../../queries/injections.scm");
/// The grammar's generated node-types description (`src/node-types.json`),
/// consumed by downstream codegen (e.g. m1-core's `Kind` generator).
pub const NODE_TYPES_JSON: &str = include_str!("../../src/node-types.json");

#[cfg(test)]
mod tests {
    #[test]
    fn can_load_grammar() {
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&super::LANGUAGE.into())
            .expect("Error loading M1 grammar");
    }

    #[test]
    fn node_types_json_is_present() {
        assert!(super::NODE_TYPES_JSON.trim_start().starts_with('['));
        assert!(super::NODE_TYPES_JSON.contains("\"identifier\""));
    }

    #[test]
    fn queries_compile_against_grammar() {
        let language: tree_sitter::Language = super::LANGUAGE.into();
        for (name, src) in [
            ("highlights", super::HIGHLIGHTS_QUERY),
            ("indents", super::INDENTS_QUERY),
            ("folds", super::FOLDS_QUERY),
            ("locals", super::LOCALS_QUERY),
        ] {
            tree_sitter::Query::new(&language, src)
                .unwrap_or_else(|e| panic!("query {name} failed to compile: {e}"));
        }
        // injections.scm is an empty placeholder; an empty query is still valid.
        tree_sitter::Query::new(&language, super::INJECTIONS_QUERY)
            .expect("injections query failed to compile");
    }
}
