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
    pub m1cfg_path: Option<PathBuf>,
    /// `.m1dbc` files merged into the project (watched for reload).
    pub dbc_paths: Vec<PathBuf>,
    /// Every `*.m1scr` under the root, found once at load (see `walk_scripts`).
    pub script_files: Vec<PathBuf>,
}

impl LoadedProject {
    /// The definition [`Location`] of a project symbol: its backing script/DBC
    /// file (at the start) for file-backed symbols, else the `.m1prj` at the
    /// symbol's declared line. `None` if no definition site is known or the path
    /// can't form a file URL. Mirrors the goto-definition resolution.
    pub fn symbol_location(&self, sym: &Symbol) -> Option<Location> {
        let (target, line) = match &sym.filename {
            Some(f) => (self.root.join(f), 0),
            None => (self.m1prj_path.clone(), sym.def_line?),
        };
        let uri = Url::from_file_path(&target).ok()?;
        Some(Location {
            uri,
            range: Range::new(Position::new(line, 0), Position::new(line, 0)),
        })
    }
}

/// Every `*.m1scr` file under `root`, recursively (sorted). Taken from the
/// filesystem rather than the symbol table's `Filename` attributes, because a
/// real `.m1prj` typically omits `Filename=` (scripts are matched to components
/// by the path-encoding convention) — so the symbol-based list would be empty.
/// Computed once at load and cached in [`LoadedProject::script_files`] for the
/// workspace-wide features (cross-file references, rename).
fn walk_scripts(root: &Path) -> Vec<PathBuf> {
    fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for e in entries.flatten() {
            let p = e.path();
            if p.is_dir() {
                walk(&p, out);
            } else if p.extension().and_then(|x| x.to_str()) == Some("m1scr") {
                out.push(p);
            }
        }
    }
    let mut out = Vec::new();
    walk(root, &mut out);
    out.sort();
    out
}

/// Apply the disk-sourced `.m1cfg`/`.m1dbc` augmentation to a freshly-parsed
/// project. Shared by `load_from` (parse from the file) and
/// `reload_from_m1prj_text` (parse from edited in-memory text); the cfg/dbc are
/// unaffected by a `.m1prj` text edit, so both re-apply them the same way.
fn augment(
    mut project: Project,
    root: &Path,
    m1cfg_path: &Option<PathBuf>,
    dbc_paths: &[PathBuf],
) -> Result<Project, String> {
    if let Some(cfg) = m1cfg_path {
        project = project.with_config(cfg).map_err(|e| e.to_string())?;
    }
    for dbc in dbc_paths {
        let rel = dbc
            .strip_prefix(root)
            .unwrap_or(dbc)
            .to_string_lossy()
            .into_owned();
        project = project.with_dbc(dbc, &rel).map_err(|e| e.to_string())?;
    }
    Ok(project)
}

#[derive(Default)]
pub struct ProjectStore {
    inner: RwLock<Option<LoadedProject>>,
}

impl ProjectStore {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn project_loaded(&self) -> bool {
        self.inner.read().unwrap().is_some()
    }

    /// Re-walk the project root for `*.m1scr` files, refreshing the cached set
    /// without re-parsing the project. Cheap; called when a script file is
    /// created or deleted (an edit to an existing script doesn't change the set).
    pub fn refresh_scripts(&self) {
        if let Some(lp) = self.inner.write().unwrap().as_mut() {
            lp.script_files = walk_scripts(&lp.root);
        }
    }

    /// Read access to the loaded project for the feature handlers.
    pub fn with_project<R>(&self, f: impl FnOnce(Option<&LoadedProject>) -> R) -> R {
        let guard = self.inner.read().unwrap();
        f(guard.as_ref())
    }

    /// Discover + load from `start_dir`, replacing any cached project. Returns
    /// `Ok(true)` if a project was loaded, `Ok(false)` if none was found, and
    /// `Err(msg)` if a found project failed to load (store is left empty).
    pub fn discover_and_load(&self, start_dir: &Path) -> Result<bool, String> {
        let Some(m1prj_path) = m1_workspace::find_project_file(start_dir) else {
            *self.inner.write().unwrap() = None;
            return Ok(false);
        };
        self.load_from(&m1prj_path)
    }

    /// Load a specific `Project.m1prj` (used by discovery and the `project_file` option).
    pub fn load_from(&self, m1prj_path: &Path) -> Result<bool, String> {
        let root = m1prj_path.parent().unwrap_or(Path::new(".")).to_path_buf();
        let m1cfg_path = m1_workspace::find_config_file(&root);
        let dbc_paths = m1_workspace::find_dbc_files(&root);
        // Build the full project first; if ANY step (.m1prj/.m1cfg/.m1dbc) fails,
        // clear the store so we don't keep serving a stale/partial project.
        let build = || -> Result<Project, String> {
            let project = Project::load(m1prj_path).map_err(|e| e.to_string())?;
            augment(project, &root, &m1cfg_path, &dbc_paths)
        };
        match build() {
            Ok(project) => {
                let script_files = walk_scripts(&root);
                *self.inner.write().unwrap() = Some(LoadedProject {
                    project,
                    root,
                    m1prj_path: m1prj_path.to_path_buf(),
                    m1cfg_path,
                    dbc_paths,
                    script_files,
                });
                Ok(true)
            }
            Err(e) => {
                *self.inner.write().unwrap() = None;
                Err(e)
            }
        }
    }

    /// Rebuild the cached project from edited `.m1prj` **text** (not disk), then
    /// re-apply the disk-sourced `.m1cfg`/`.m1dbc` and re-walk scripts. Used after
    /// a rename rewrites `Project.m1prj`: the client applies the edit to a buffer
    /// it may not save (and never notifies us via file-watching), so we refresh
    /// the symbol model immediately from the text the rename produced — otherwise
    /// the renamed symbol reads as undefined until the server restarts.
    ///
    /// `Ok(false)` if no project is loaded. On a parse/augment failure the
    /// previous model is **kept** (not cleared) — a transiently invalid edit
    /// shouldn't drop the whole project; `Err` is returned for logging.
    pub fn reload_from_m1prj_text(&self, m1prj_text: &str) -> Result<bool, String> {
        let (root, m1prj_path, m1cfg_path, dbc_paths) = {
            let guard = self.inner.read().unwrap();
            let Some(lp) = guard.as_ref() else {
                return Ok(false);
            };
            (
                lp.root.clone(),
                lp.m1prj_path.clone(),
                lp.m1cfg_path.clone(),
                lp.dbc_paths.clone(),
            )
        };
        let project = Project::from_xml(m1prj_text)
            .map_err(|e| e.to_string())
            .and_then(|p| augment(p, &root, &m1cfg_path, &dbc_paths))?;
        let script_files = walk_scripts(&root);
        *self.inner.write().unwrap() = Some(LoadedProject {
            project,
            root,
            m1prj_path,
            m1cfg_path,
            dbc_paths,
            script_files,
        });
        Ok(true)
    }

    /// True if `path` is the currently-loaded `.m1prj` or `.m1cfg` (reload trigger).
    pub fn is_watched(&self, path: &Path) -> bool {
        self.with_project(|p| {
            p.map(|lp| {
                lp.m1prj_path == path
                    || lp.m1cfg_path.as_deref() == Some(path)
                    || lp.dbc_paths.iter().any(|d| d == path)
            })
            .unwrap_or(false)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    // Minimal .m1prj: a Root group containing one channel component.
    const M1PRJ: &str = r#"<?xml version="1.0"?>
<Project>
  <Component Classname="BuiltIn.GroupCompound" Name="Root"/>
  <Component Classname="BuiltIn.ChannelMeasure" Name="Root.Speed Glonk"/>
</Project>"#;

    fn write_project(dir: &Path) -> PathBuf {
        let p = dir.join("Project.m1prj");
        let mut f = std::fs::File::create(&p).unwrap();
        f.write_all(M1PRJ.as_bytes()).unwrap();
        p
    }

    const DBC: &[u8] = br#"<?xml version="1.0"?>
<DBC>
 <ComponentStream>
  <List>
   <Component Classname="BuiltIn.CAN.DBC" Name="Balls3EV25"/>
   <Component Classname="BuiltIn.CAN.Message" Name="Balls3EV25.DashVals"/>
   <Component Classname="BuiltIn.CAN.Signal" Name="Balls3EV25.DashVals.Inverter Error">
    <Props Type="u32"/>
   </Component>
  </List>
 </ComponentStream>
</DBC>"#;

    #[test]
    fn partial_load_failure_clears_store() {
        let tmp = tempfile::tempdir().unwrap();
        write_project(tmp.path());
        let store = ProjectStore::new();
        // First load succeeds and populates the store.
        assert!(store.discover_and_load(tmp.path()).unwrap());
        assert!(store.with_project(|p| p.is_some()));
        // Add a malformed .m1dbc; the reload must fail AND clear the store so we
        // don't keep serving the now-stale project. (Regression for #37.)
        let dbcdir = tmp.path().join("dbc");
        std::fs::create_dir_all(&dbcdir).unwrap();
        std::fs::File::create(dbcdir.join("bad.m1dbc"))
            .unwrap()
            .write_all(b"<<< not valid xml")
            .unwrap();
        assert!(store.discover_and_load(tmp.path()).is_err());
        assert!(
            store.with_project(|p| p.is_none()),
            "store must be cleared after a partial load failure"
        );
    }

    #[test]
    fn discovers_and_loads_dbc_objects() {
        let tmp = tempfile::tempdir().unwrap();
        write_project(tmp.path());
        let dbcdir = tmp.path().join("dbc");
        std::fs::create_dir_all(&dbcdir).unwrap();
        let dbc = dbcdir.join("Balls3EV25.m1dbc");
        std::fs::File::create(&dbc).unwrap().write_all(DBC).unwrap();

        let store = ProjectStore::new();
        assert_eq!(store.discover_and_load(tmp.path()), Ok(true));
        store.with_project(|p| {
            let t = p.unwrap().project.symbols();
            assert!(t.get("Balls3EV25").is_some(), "DBC root must be a symbol");
            let sig = t
                .get("Balls3EV25.DashVals.Inverter Error")
                .expect("signal symbol");
            assert_eq!(sig.value_type, m1_typecheck::ValueType::Unsigned);
        });
        // The DBC file is watched so edits trigger a reload.
        assert!(store.is_watched(&dbc), "dbc must be watched");
    }

    #[test]
    fn discovers_and_loads_known_symbol() {
        let tmp = tempfile::tempdir().unwrap();
        write_project(tmp.path());
        let store = ProjectStore::new();
        assert_eq!(store.discover_and_load(tmp.path()), Ok(true));
        assert!(store.project_loaded());
        store.with_project(|p| {
            let lp = p.unwrap();
            assert!(lp.project.symbols().get("Root.Speed Glonk").is_some());
        });
    }

    #[test]
    fn no_project_is_project_less_mode() {
        let tmp = tempfile::tempdir().unwrap();
        let store = ProjectStore::new();
        assert_eq!(store.discover_and_load(tmp.path()), Ok(false));
        assert!(!store.project_loaded());
    }

    #[test]
    fn caches_scripts_at_load_and_refreshes_on_demand() {
        let tmp = tempfile::tempdir().unwrap();
        write_project(tmp.path());
        let scripts = tmp.path().join("Scripts");
        std::fs::create_dir_all(&scripts).unwrap();
        std::fs::write(scripts.join("A.m1scr"), "x = 1;\n").unwrap();

        let store = ProjectStore::new();
        store.discover_and_load(tmp.path()).unwrap();
        // Cached at load: the one script present.
        store.with_project(|p| assert_eq!(p.unwrap().script_files.len(), 1));

        // A new script created after load is invisible until a refresh.
        std::fs::write(scripts.join("B.m1scr"), "y = 2;\n").unwrap();
        store.with_project(|p| assert_eq!(p.unwrap().script_files.len(), 1));
        store.refresh_scripts();
        store.with_project(|p| {
            let files = &p.unwrap().script_files;
            assert_eq!(files.len(), 2, "refresh picks up the new script: {files:?}");
        });
    }

    #[test]
    fn reload_from_m1prj_text_refreshes_symbols_without_disk() {
        // After a rename, the LSP must refresh its model from the edited (not yet
        // saved) project text, so the renamed symbol is immediately live.
        let tmp = tempfile::tempdir().unwrap();
        write_project(tmp.path()); // declares Root.Speed Glonk
        let store = ProjectStore::new();
        assert!(store.discover_and_load(tmp.path()).unwrap());
        store.with_project(|p| {
            let t = p.unwrap().project.symbols();
            assert!(t.get("Root.Speed Glonk").is_some());
            assert!(t.get("Root.Velocity Glonk").is_none());
        });

        // Simulate the post-rename project text (Speed -> Velocity), as the client
        // *would* write it — but without touching disk.
        let renamed = M1PRJ.replace("Speed Glonk", "Velocity Glonk");
        assert!(store.reload_from_m1prj_text(&renamed).unwrap());

        store.with_project(|p| {
            let t = p.unwrap().project.symbols();
            assert!(
                t.get("Root.Velocity Glonk").is_some(),
                "renamed symbol must be live after reload"
            );
            assert!(
                t.get("Root.Speed Glonk").is_none(),
                "old symbol must be gone"
            );
        });
    }

    // A parameter-bearing .m1prj plus a matching .m1cfg, used to verify that the
    // .m1cfg is discovered and applied (it augments the parameter's value type
    // and unit). Authored from scratch — not derived from any vehicle corpus.
    const M1PRJ_PARAM: &str = r#"<?xml version="1.0"?>
<Project>
  <Component Classname="BuiltIn.GroupCompound" Name="Root.Foo"/>
  <Component Classname="BuiltIn.Parameter" Name="Root.Foo.Gain.Value"><Props/></Component>
</Project>"#;
    const M1CFG_PARAM: &str = r#"<?xml version="1.0"?>
<Configuration><Group Name="">
  <Parameter Name="Root.Foo.Gain.Value"><Cell Type="u16" Unit="ratio"><![CDATA[1]]></Cell></Parameter>
</Group></Configuration>"#;

    #[test]
    fn find_m1cfg_walks_ancestors_and_is_loaded() {
        // Repo root holds parameters.m1cfg; the Project.m1prj lives several
        // directories deeper. The cfg must still be discovered by walking up.
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("parameters.m1cfg"), M1CFG_PARAM).unwrap();
        let nested = tmp.path().join("UQR-EV/01.00/Project");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(nested.join("Project.m1prj"), M1PRJ_PARAM).unwrap();

        let store = ProjectStore::new();
        assert_eq!(store.discover_and_load(&nested), Ok(true));
        store.with_project(|p| {
            let lp = p.unwrap();
            // The recorded cfg path points at the ancestor file.
            assert_eq!(
                lp.m1cfg_path.as_deref(),
                Some(tmp.path().join("parameters.m1cfg").as_path()),
                "m1cfg_path should point at the ancestor cfg"
            );
            // And it was actually applied: the parameter gained type + unit.
            let sym = lp
                .project
                .symbols()
                .get("Root.Foo.Gain.Value")
                .expect("parameter symbol");
            assert_eq!(sym.value_type, m1_typecheck::ValueType::Unsigned);
            assert_eq!(sym.unit.as_deref(), Some("ratio"));
        });
    }
}
