//! Discovery, loading, caching, and reload of the m1-typecheck Project.
use m1_typecheck::Project;
use std::path::{Path, PathBuf};
use std::sync::RwLock;

/// A loaded project plus the paths it came from (for reload + goto).
pub struct LoadedProject {
    pub project: Project,
    pub root: PathBuf,
    pub m1prj_path: PathBuf,
    pub m1cfg_path: Option<PathBuf>,
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

    /// Read access to the loaded project for the feature handlers.
    pub fn with_project<R>(&self, f: impl FnOnce(Option<&LoadedProject>) -> R) -> R {
        let guard = self.inner.read().unwrap();
        f(guard.as_ref())
    }

    /// Find `Project.m1prj` at `start` or any ancestor; return its path.
    pub fn find_m1prj(start: &Path) -> Option<PathBuf> {
        let mut dir = Some(start);
        while let Some(d) = dir {
            let cand = d.join("Project.m1prj");
            if cand.is_file() {
                return Some(cand);
            }
            dir = d.parent();
        }
        None
    }

    /// First `*.m1cfg` sibling of the `.m1prj`, if any.
    fn find_m1cfg(root: &Path) -> Option<PathBuf> {
        std::fs::read_dir(root).ok()?.flatten().find_map(|e| {
            let p = e.path();
            (p.extension().and_then(|x| x.to_str()) == Some("m1cfg")).then_some(p)
        })
    }

    /// Discover + load from `start_dir`, replacing any cached project. Returns
    /// `Ok(true)` if a project was loaded, `Ok(false)` if none was found, and
    /// `Err(msg)` if a found project failed to load (store is left empty).
    pub fn discover_and_load(&self, start_dir: &Path) -> Result<bool, String> {
        let Some(m1prj_path) = Self::find_m1prj(start_dir) else {
            *self.inner.write().unwrap() = None;
            return Ok(false);
        };
        self.load_from(&m1prj_path)
    }

    /// Load a specific `Project.m1prj` (used by discovery and the `project_file` option).
    pub fn load_from(&self, m1prj_path: &Path) -> Result<bool, String> {
        let root = m1prj_path.parent().unwrap_or(Path::new(".")).to_path_buf();
        let m1cfg_path = Self::find_m1cfg(&root);
        let mut project = Project::load(m1prj_path).map_err(|e| e.to_string())?;
        if let Some(cfg) = &m1cfg_path {
            project = project.with_config(cfg).map_err(|e| e.to_string())?;
        }
        *self.inner.write().unwrap() = Some(LoadedProject {
            project,
            root,
            m1prj_path: m1prj_path.to_path_buf(),
            m1cfg_path,
        });
        Ok(true)
    }

    /// True if `path` is the currently-loaded `.m1prj` or `.m1cfg` (reload trigger).
    pub fn is_watched(&self, path: &Path) -> bool {
        self.with_project(|p| {
            p.map(|lp| lp.m1prj_path == path || lp.m1cfg_path.as_deref() == Some(path))
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
    fn find_m1prj_walks_ancestors() {
        let tmp = tempfile::tempdir().unwrap();
        write_project(tmp.path());
        let nested = tmp.path().join("a/b");
        std::fs::create_dir_all(&nested).unwrap();
        assert!(ProjectStore::find_m1prj(&nested).is_some());
    }
}
