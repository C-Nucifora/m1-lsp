//! m1-lsp library surface. The binary (`main.rs`) is a thin bootstrap; all
//! logic lives here so it can be unit- and integration-tested.
pub mod analysis;
pub mod backend;
pub mod config;
pub mod convert;
pub mod disk_read;
pub mod document;
pub mod features;
pub mod fmt_backend;
pub mod format;
pub mod line_index;
pub mod lint_backend;
pub mod project_store;
pub(crate) mod semtok_delta;
pub mod type_backend;
