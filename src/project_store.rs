//! Discovery, loading, caching, and reload of the m1-typecheck Project.
use crate::features::call_hierarchy::CallGraph;
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
            Some(f) => (contained_join(&self.root, f)?, 0),
            None => (self.m1prj_path.clone(), sym.def_line?),
        };
        let uri = Url::from_file_path(&target).ok()?;
        Some(Location {
            uri,
            range: Range::new(Position::new(line, 0), Position::new(line, 0)),
        })
    }
}

/// `(uri, text)` for every project script that a cross-file rename or
/// reference-search loop walks: the **cursor file first** (so it's always
/// present and processed first), then every other `*.m1scr` in `script_files`,
/// deduped by URI, preferring an open editor buffer over the on-disk text.
///
/// `cursor_text` lets the caller skip an I/O round-trip when it already holds the
/// cursor file's text (the references/highlight path): pass `Some(text)` to use
/// it verbatim, or `None` to read the cursor file like any other (open buffer,
/// then disk — the rename path, which only has the URI).
///
/// Takes the script-path slice by reference (rather than a `&LoadedProject`) so
/// the caller can clone the small `Vec<PathBuf>` and drop the project `RwLock`
/// guard *before* this read+parse-every-script loop runs (#135).
pub(crate) fn gather_project_scripts(
    script_files: &[PathBuf],
    cursor_uri: &Url,
    cursor_text: Option<&str>,
    open_text: &dyn Fn(&Url) -> Option<String>,
) -> Vec<(Url, String)> {
    let mut out: Vec<(Url, String)> = Vec::new();
    let cursor = cursor_text.map(str::to_string).or_else(|| {
        open_text(cursor_uri).or_else(|| {
            cursor_uri
                .to_file_path()
                .ok()
                .and_then(|p| crate::disk_read::read_disk(&p))
        })
    });
    if let Some(t) = cursor {
        out.push((cursor_uri.clone(), t));
    }
    for p in script_files {
        let Ok(uri) = Url::from_file_path(p) else {
            continue;
        };
        if out.iter().any(|(u, _)| *u == uri) {
            continue;
        }
        if let Some(t) = open_text(&uri).or_else(|| crate::disk_read::read_disk(p)) {
            out.push((uri, t));
        }
    }
    out
}

/// Maximum directory depth [`walk_scripts`] descends (defense-in-depth against a
/// pathologically deep tree).
const MAX_WALK_DEPTH: usize = 64;

/// Join an (untrusted) `.m1prj` `Filename=` value to the project root, rejecting
/// anything that would escape the project tree.
///
/// A `.m1prj` comes from an arbitrary cloned repo, so its `Filename=` is
/// attacker-controllable. `Path::join` discards the base entirely for an
/// *absolute* value (`Filename="/etc/passwd"`), and preserves `..` segments, so
/// a naive join can yield a Location pointing anywhere on disk that the editor
/// would open on goto-definition / workspace-symbol click (#134). Accept only a
/// relative path whose components are all normal (no root, prefix, or `..`), so
/// the result is lexically contained in `root`.
pub(crate) fn contained_join(root: &Path, filename: &str) -> Option<PathBuf> {
    use std::path::Component;
    let rel = Path::new(filename);
    for comp in rel.components() {
        match comp {
            Component::Normal(_) | Component::CurDir => {}
            // RootDir/Prefix (absolute) or ParentDir (`..`) could escape root.
            Component::RootDir | Component::Prefix(_) | Component::ParentDir => return None,
        }
    }
    Some(root.join(rel))
}

/// Every `*.m1scr` file under `root`, recursively (sorted). Taken from the
/// filesystem rather than the symbol table's `Filename` attributes, because a
/// real `.m1prj` typically omits `Filename=` (scripts are matched to components
/// by the path-encoding convention) — so the symbol-based list would be empty.
/// Computed once at load and cached in [`LoadedProject::script_files`] for the
/// workspace-wide features (cross-file references, rename).
fn walk_scripts(root: &Path) -> Vec<PathBuf> {
    fn walk(dir: &Path, depth: usize, out: &mut Vec<PathBuf>) {
        if depth > MAX_WALK_DEPTH {
            return;
        }
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for e in entries.flatten() {
            // Use the entry's own (non-following) file type: a crafted in-tree
            // symlink to an out-of-tree directory must not be descended, or the
            // server would enumerate arbitrary host `*.m1scr` files into the
            // workspace symbol set (mirrors m1-workspace#7).
            let Ok(ft) = e.file_type() else {
                continue;
            };
            if ft.is_symlink() {
                continue;
            }
            let p = e.path();
            if ft.is_dir() {
                walk(&p, depth + 1, out);
            } else if p.extension().and_then(|x| x.to_str()) == Some("m1scr") {
                out.push(p);
            }
        }
    }
    let mut out = Vec::new();
    walk(root, 0, &mut out);
    out.sort();
    out
}

/// Read each cached script path into a `(basename, source)` pair for the
/// project-wide scheduling/usage checks ([`m1_typecheck::schedule`]). The key is
/// the file's basename (not a relative path) because
/// `Project::function_symbol_for_script` matches on the basename — this mirrors
/// the CLI's `gather_project_scripts`. Tolerant decode (UTF-8 → Windows-1252) so
/// a `°`-bearing MoTeC script is included; an unreadable file is skipped.
fn scripts_from_disk(script_files: &[PathBuf]) -> Vec<(String, String)> {
    script_files
        .iter()
        .filter_map(|p| {
            let name = p.file_name()?.to_str()?.to_string();
            let src = crate::disk_read::read_disk(p)?;
            Some((name, src))
        })
        .collect()
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
        // A malformed/unreadable DBC must not blank the whole project model:
        // skip just that file and keep every other symbol. Encoding is handled
        // upstream (m1-typecheck decodes Windows-1252), so this only trips on
        // genuinely broken CAN XML.
        if let Err(e) = project.augment_dbc(dbc, &rel) {
            eprintln!("m1-lsp: skipping .m1dbc {}: {e}", dbc.display());
        }
    }
    Ok(project)
}

#[derive(Default)]
pub struct ProjectStore {
    inner: RwLock<Option<LoadedProject>>,
    /// Cached call-hierarchy data-flow graph for the loaded project, built lazily
    /// on the first call-hierarchy request and reused across the
    /// `prepare`/`incoming`/`outgoing` requests of one "Show Call Hierarchy"
    /// interaction (each is a separate LSP request, so without this the graph
    /// would be rebuilt — every script re-parsed — once per request). Invalidated
    /// by [`Self::invalidate_call_graph`] on any document edit/open/close and on
    /// every project (re)load, so a rebuild always reflects the live buffers.
    call_graph: RwLock<Option<CallGraph>>,
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
        // The script set changed (file created/deleted) — drop the stale graph.
        self.invalidate_call_graph();
    }

    /// Read access to the loaded project for the feature handlers.
    pub fn with_project<R>(&self, f: impl FnOnce(Option<&LoadedProject>) -> R) -> R {
        let guard = self.inner.read().unwrap();
        f(guard.as_ref())
    }

    /// Drop the cached call-hierarchy graph so the next call-hierarchy request
    /// rebuilds it from the current project + buffers. Cheap (just clears the
    /// cell); called on any document edit/open/close and on every project reload.
    pub fn invalidate_call_graph(&self) {
        *self.call_graph.write().unwrap() = None;
    }

    /// Run `f` against the loaded project and its call-hierarchy graph, building
    /// (and caching) the graph with `build` on a cache miss. The graph is reused
    /// across the three requests of one call-hierarchy interaction; it is dropped
    /// by [`Self::invalidate_call_graph`] whenever a buffer or the project
    /// changes, so a freshly-built graph always reflects the live state. `f`
    /// receives `None` only when no project is loaded.
    pub fn with_call_graph<R>(
        &self,
        build: impl FnOnce(&LoadedProject) -> CallGraph,
        f: impl FnOnce(Option<(&LoadedProject, &CallGraph)>) -> R,
    ) -> R {
        let project = self.inner.read().unwrap();
        let Some(lp) = project.as_ref() else {
            return f(None);
        };
        let mut graph = self.call_graph.write().unwrap();
        if graph.is_none() {
            *graph = Some(build(lp));
        }
        f(Some((lp, graph.as_ref().unwrap())))
    }

    /// Project-scope diagnostics for the loaded project: the `.m1cfg`-coverage
    /// audit (T041), the symbol-name / component audits (T050/T010/T071), and
    /// the M1-Build-parity checks (T092 tags, T088 circular schedule, T093/T094
    /// unassigned-channel / unread-parameter). These are not tied to any one
    /// script — the CLI emits them once per project, and the LSP anchors them to
    /// the `.m1prj` (#139). The opt-in T089 is excluded here; use
    /// [`Self::project_diagnostics_with`] to include it. Empty when no project
    /// is loaded.
    pub fn project_diagnostics(&self) -> Vec<m1_typecheck::diagnostics::TypeDiagnostic> {
        self.project_diagnostics_with(false)
    }

    /// [`Self::project_diagnostics`] plus the project-wide checks that M1 Build
    /// itself reports, so the editor mirrors a *Validate Project* run (the CLI
    /// runs these by default too — this is the LSP catching up, #145):
    ///
    /// - **T092** untagged-component (tag warnings 1142/1549) — `audit_tags`,
    /// - **T088** circular-dependency (warning 1640) — `schedule::check`,
    /// - **T093/T094** unassigned-channel / unread-parameter (errors 1627/1631)
    ///   — `schedule::check_usage`.
    ///
    /// All of these are **default-on** because M1 Build emits exactly them; a
    /// team that doesn't want one drops it with `[diagnostics] ignore` (the
    /// downstream `allows_subject` filter), the same lever the CLI uses. The one
    /// opt-in is **T089** rate-inversion, which M1 Build does *not* flag — it
    /// runs only when `rate_inversion` is set (i.e. `select` names T089).
    ///
    /// The scheduling/usage checks need the **complete** project script set (a
    /// missing writer would make its channels look never-assigned), so they read
    /// every `.m1scr` under the root from disk — exactly like the CLI's
    /// `gather_project_scripts`, keyed by basename to match
    /// `Project::function_symbol_for_script`. Project-scope diagnostics are
    /// recomputed on project (re)load, not per-keystroke, so the per-call script
    /// re-read is not on a hot path.
    pub fn project_diagnostics_with(
        &self,
        rate_inversion: bool,
    ) -> Vec<m1_typecheck::diagnostics::TypeDiagnostic> {
        self.with_project(|p| match p {
            Some(lp) => {
                let mut v = lp.project.missing_cfg_parameters();
                v.extend(lp.project.audit());
                v.extend(lp.project.audit_tags());
                v.extend(lp.project.audit_display_units());
                let scripts = scripts_from_disk(&lp.script_files);
                v.extend(m1_typecheck::schedule::check(
                    &lp.project,
                    &scripts,
                    true,
                    rate_inversion,
                ));
                v.extend(m1_typecheck::schedule::check_usage(
                    &lp.project,
                    &scripts,
                    true,
                    true,
                ));
                v
            }
            None => Vec::new(),
        })
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
        // Build the full project first; if .m1prj or .m1cfg fails, clear the
        // store so we don't keep serving a stale/partial project. A bad .m1dbc
        // is non-fatal (skipped in `augment`) — one malformed CAN file must not
        // blank the whole model.
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
                self.invalidate_call_graph();
                Ok(true)
            }
            Err(e) => {
                *self.inner.write().unwrap() = None;
                self.invalidate_call_graph();
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
        self.invalidate_call_graph();
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
    fn malformed_dbc_is_skipped_not_fatal() {
        let tmp = tempfile::tempdir().unwrap();
        write_project(tmp.path());
        let store = ProjectStore::new();
        // First load succeeds and populates the store.
        assert!(store.discover_and_load(tmp.path()).unwrap());
        assert!(store.with_project(|p| p.is_some()));
        // Add a malformed .m1dbc. A single broken CAN file must NOT blank the
        // whole model: the reload still succeeds, the store stays populated, and
        // every non-DBC symbol still resolves. (The original #37 regression — a
        // bad DBC clearing the store — was the wrong behaviour; it left every
        // channel unresolved. The fatal path that #37 cared about, a corrupt
        // .m1prj, is covered by `corrupt_m1prj_clears_store` below.)
        let dbcdir = tmp.path().join("dbc");
        std::fs::create_dir_all(&dbcdir).unwrap();
        std::fs::File::create(dbcdir.join("bad.m1dbc"))
            .unwrap()
            .write_all(b"<<< not valid xml")
            .unwrap();
        assert!(
            store.discover_and_load(tmp.path()).unwrap(),
            "a malformed .m1dbc is skipped, not fatal"
        );
        store.with_project(|p| {
            let lp = p.expect("store stays populated despite the bad DBC");
            assert!(
                lp.project.symbols().get("Root.Speed Glonk").is_some(),
                "non-DBC symbols still resolve"
            );
        });
    }

    #[test]
    fn corrupt_m1prj_clears_store() {
        let tmp = tempfile::tempdir().unwrap();
        let prj = write_project(tmp.path());
        let store = ProjectStore::new();
        assert!(store.discover_and_load(tmp.path()).unwrap());
        assert!(store.with_project(|p| p.is_some()));
        // A corrupt project file IS fatal: the reload must fail AND clear the
        // store so we don't keep serving the now-stale project. (Regression for
        // #37, applied to the file that genuinely defines the model.)
        std::fs::write(&prj, b"<<< not valid xml").unwrap();
        assert!(store.discover_and_load(tmp.path()).is_err());
        assert!(
            store.with_project(|p| p.is_none()),
            "store must be cleared after a fatal load failure"
        );
    }

    #[test]
    fn project_diagnostics_flag_param_missing_from_cfg() {
        // A parameter declared in the `.m1prj` but absent from the `.m1cfg`
        // should surface as a T041 project-level diagnostic (#139) — the same
        // audit the CLI runs, made available to the LSP.
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join("Project.m1prj"),
            "<?xml version=\"1.0\"?>\n<Project>\n\
             <Component Classname=\"BuiltIn.GroupCompound\" Name=\"Root\"/>\n\
             <Component Classname=\"BuiltIn.GroupCompound\" Name=\"Root.A\"/>\n\
             <Component Classname=\"BuiltIn.Parameter\" Name=\"Root.A.Covered\"><Props Type=\"u32\"/></Component>\n\
             <Component Classname=\"BuiltIn.Parameter\" Name=\"Root.A.Missing\"><Props Type=\"u32\"/></Component>\n\
             </Project>",
        )
        .unwrap();
        // A real `.m1cfg` lists parameters with the `Root.` prefix stripped.
        std::fs::write(
            tmp.path().join("parameters.m1cfg"),
            "<?xml version=\"1.0\"?>\n<Configuration>\n <Group Name=\"\">\n\
             <Parameter Name=\"A.Covered\"><Cell Type=\"u32\"><![CDATA[1]]></Cell></Parameter>\n\
             </Group>\n</Configuration>",
        )
        .unwrap();
        let store = ProjectStore::new();
        assert!(store.discover_and_load(tmp.path()).unwrap());
        let diags = store.project_diagnostics();
        assert!(
            diags
                .iter()
                .any(|d| d.code == m1_typecheck::diagnostics::TypeCode::T041
                    && d.inner.message.contains("Root.A.Missing")),
            "param missing from cfg should be flagged T041; got {diags:?}"
        );
        assert!(
            !diags
                .iter()
                .any(|d| d.code == m1_typecheck::diagnostics::TypeCode::T041
                    && d.inner.message.contains("Root.A.Covered")),
            "a cfg-covered parameter must not be flagged T041 (it may still draw \
             a default-on T094 'never read', which is correct — no script reads it)"
        );
    }

    #[test]
    fn project_diagnostics_empty_when_no_project() {
        let store = ProjectStore::new();
        assert!(store.project_diagnostics().is_empty());
    }

    #[test]
    fn tag_and_usage_audits_are_default_on() {
        // The M1-Build-parity checks now run by default (matching the CLI and a
        // *Validate Project* run): an untagged channel flags T092 in both tag
        // groups, and a channel no script assigns flags T093 — all without any
        // `select` opt-in. Only T089 (rate-inversion, which M1 Build does not
        // emit) stays gated on the `rate_inversion` flag.
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join("Project.m1prj"),
            "<?xml version=\"1.0\"?>\n<Project>\n\
             <Component Classname=\"BuiltIn.GroupCompound\" Name=\"Root.A\"/>\n\
             <Component Classname=\"BuiltIn.Channel\" Name=\"Root.A.Chan\"><Props Type=\"u32\"/></Component>\n\
             </Project>",
        )
        .unwrap();
        let store = ProjectStore::new();
        assert!(store.discover_and_load(tmp.path()).unwrap());
        let count = |v: &[m1_typecheck::diagnostics::TypeDiagnostic],
                     code: m1_typecheck::diagnostics::TypeCode| {
            v.iter().filter(|d| d.code == code).count()
        };
        let diags = store.project_diagnostics();
        assert!(
            count(&diags, m1_typecheck::diagnostics::TypeCode::T092) >= 2,
            "default-on: the untagged channel flags both tag groups; got {diags:?}"
        );
        assert_eq!(
            count(&diags, m1_typecheck::diagnostics::TypeCode::T093),
            1,
            "default-on: the never-assigned channel flags T093; got {diags:?}"
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
