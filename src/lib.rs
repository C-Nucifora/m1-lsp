//! m1-lsp library surface. The binary (`main.rs`) is a thin bootstrap; all
//! logic lives here so it can be unit- and integration-tested.
pub mod analysis;
pub mod backend;
pub mod convert;
pub mod document;
pub mod format;
pub mod line_index;
