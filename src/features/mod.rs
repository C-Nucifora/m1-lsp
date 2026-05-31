//! Symbol-model-powered LSP features (v2): hover, goto, document symbols,
//! completion, inlay type-hints, rename, references/highlights, folding, and
//! code-action quick-fixes.
pub mod code_action;
pub mod completion;
pub mod document_symbols;
pub mod folding;
pub mod goto;
pub mod hover;
pub mod inlay;
pub mod locate;
pub mod references;
pub mod rename;
pub mod semantic_tokens;
pub mod signature_help;
