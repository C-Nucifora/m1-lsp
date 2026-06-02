//! Discovery, loading, caching, and reload of the m1-typecheck Project.
use m1_typecheck::Project;
use m1_typecheck::symbols::Symbol;
use std::path::{Path, PathBuf};
use std::sync::RwLock;
use tower_lsp::lsp_types::{Location, Position, Range, Url};

/// A loaded project plus the paths it came from (for reload + goto).
pub struct LoadedProject {
    pub project: Project,
    pub root: PathBuf,
    pub m1prj_path: PathBuf,
    pub m1cfg_path: Option <PathBuf>,
    /// `.m1dbc` files merged into the project (watched for reload).
    pub dbc_paths: Vec<PathBuf>,
    /// Every `*.m1scr` under the root, found once at load (see `walk_scripts`).
    pub script_files: Vec<PathBuf>,
}
