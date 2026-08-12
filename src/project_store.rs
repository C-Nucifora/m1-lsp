//! Discovery, loading, caching, and reload of the m1-typecheck Project.
use crate::eval::Trace;
use crate::eval::config::EvalConfig;
use crate::eval::engine::{EvalOutcome, Provenance};
use crate::features::call_hierarchy::CallGraph;
use m1_typecheck::Project;
use m1_typecheck::symbols::Symbol;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};
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
/// workspace-wide features (cross-file references, rename). Discovery goes
/// through the shared hardened walk ([`m1_workspace::find_scripts`]:
/// symlink-skip, depth cap — the m1-workspace#7 guarantees — #256).
fn walk_scripts(root: &Path) -> Vec<PathBuf> {
    m1_workspace::find_scripts(root)
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

/// Identifies the inputs a cached [`m1_eval::Trace`] was built from, so a stale
/// cache entry can be detected on the next request.
///
/// The trace is a pure function of two things: the **loaded project model** and
/// the **resolved [`EvalConfig`]**. The project model has no cheap content hash,
/// so its identity is tracked by a monotonic *reload generation*
/// ([`ProjectStore::generation`]) that bumps on every (re)load; the config is
/// hashed directly (it derives [`Hash`]). When either changes the key differs and
/// [`ProjectStore::with_eval`] rebuilds. This mirrors how the call-graph cache is
/// invalidated, but keyed rather than blindly dropped, so a no-op config
/// re-application does not force a needless rebuild.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct EvalKey {
    /// The project reload generation the trace was built against.
    generation: u64,
    /// A hash of the resolved [`EvalConfig`] the trace was built under.
    config_hash: u64,
}

impl EvalKey {
    /// The key for a trace built at reload `generation` under `cfg`.
    fn new(generation: u64, cfg: &EvalConfig) -> Self {
        let mut h = std::collections::hash_map::DefaultHasher::new();
        cfg.hash(&mut h);
        EvalKey {
            generation,
            config_hash: h.finish(),
        }
    }
}

/// A cached evaluation: the single shared [`m1_eval::Trace`], its provenance (for
/// honest rendering downstream), and the [`EvalKey`] it was built under. Held in
/// an `RwLock<Option<…>>` on the store and reused across hover/inlay requests; a
/// hover never triggers a fresh run unless the key changed (see
/// [`ProjectStore::with_eval`]).
struct CachedEval {
    /// The shared trace. `Arc` so a handler can cheaply clone a reference out
    /// from under the cache lock if it needs to drop the guard before rendering.
    trace: Arc<Trace>,
    /// Where the trace came from, carried so rendering can be honest about
    /// offline-default vs configured values.
    provenance: Provenance,
    /// The inputs this trace was built from; a mismatch on the next request
    /// triggers a rebuild.
    key: EvalKey,
}

#[derive(Default)]
pub struct ProjectStore {
    /// The loaded project, handed out as an `Arc` snapshot (#341): accessors
    /// clone the handle under a briefly-held read guard and work on the clone,
    /// so no request holds this lock while it computes. That matters because a
    /// project reload takes the write lock, and writer-preferring rwlocks
    /// (macOS pthread, glibc default) block **new readers** behind a queued
    /// writer — a guard held across a whole-corpus build would stall every
    /// other handler for the build's duration.
    inner: RwLock<Option<Arc<LoadedProject>>>,
    /// Monotonic project **reload generation**, bumped on every successful
    /// (re)load. It stands in for a content hash of the loaded project model
    /// (which has none) inside [`EvalKey`]: a reload changes the generation, so
    /// the eval cache built against the old generation is detected as stale and
    /// rebuilt. Starts at 0; the first load bumps it to 1.
    generation: AtomicU64,
    /// Cached, debounced evaluation for the loaded project (E3). Built lazily on
    /// the first hover/inlay request after an invalidation and reused by every
    /// subsequent request with the same [`EvalKey`] — so hover/inlay never run a
    /// trace per request. Dropped by [`Self::invalidate_call_graph`] at the same
    /// edit/open/close/save/reload points as the other per-project caches (the
    /// backend also drops it on `did_change_configuration`), and superseded when
    /// the config or reload generation changes the key. `None` until the first
    /// build; `None` again after any invalidation.
    eval_cache: RwLock<Option<CachedEval>>,
    /// Cached call-hierarchy data-flow graph for the loaded project, built lazily
    /// on the first call-hierarchy request and reused across the
    /// `prepare`/`incoming`/`outgoing` requests of one "Show Call Hierarchy"
    /// interaction (each is a separate LSP request, so without this the graph
    /// would be rebuilt — every script re-parsed — once per request). Invalidated
    /// by [`Self::invalidate_call_graph`] on any document edit/open/close and on
    /// every project (re)load, so a rebuild always reflects the live buffers.
    call_graph: RwLock<Option<CallGraph>>,
    /// Cross-script analysis derived from the loaded project's on-disk scripts:
    /// the shared parse, the solved channel taints (so cross-file T080/T081 reach
    /// each file's sinks), and the memoized project-scope diagnostics. Keyed by a
    /// **content hash** of every project script's bytes, so an unsaved-buffer
    /// keystroke — which does not change disk — is a cache hit and the
    /// whole-corpus re-read/re-parse/re-solve reruns only when a file's content
    /// actually changes on disk (the finding-2 fix: pull diagnostics poll this per
    /// `workspace/diagnostic` request and pull-capable clients re-poll on every
    /// open/change). Cleared outright by [`Self::invalidate_analysis`] on project
    /// (re)load and script-set change — the points where the project model or file
    /// set changes without the surviving files' content hash moving; plain buffer
    /// edits leave it alone.
    analysis: RwLock<Option<ProjectAnalysis>>,
    /// Serializes [`Self::ensure_analysis`]'s rebuild (double-checked locking,
    /// #340): concurrent first-requests after an invalidation run the
    /// whole-corpus parse + taint solve once — the losers wait here, then read
    /// the freshly built cache. A dedicated mutex (not the `analysis` RwLock)
    /// so the long build holds no lock that any reader nests with.
    analysis_rebuild: std::sync::Mutex<()>,
}

/// The cached cross-script analysis; see [`ProjectStore::analysis`].
struct ProjectAnalysis {
    /// Combined hash of every project script's path + on-disk content.
    content_hash: u64,
    /// Each script parsed exactly once (m1-typecheck's parse-once API), shared by
    /// reference across every project-wide pass below.
    parsed: Vec<m1_typecheck::parsed::ParsedScript>,
    /// Solved cross-script channel taints (T080/T081 provenance).
    taints: m1_typecheck::cross_script::ChannelTaints,
    /// Project-scope diagnostics, memoized per `rate_inversion` (T089) flag.
    diags: std::collections::HashMap<bool, Vec<m1_typecheck::diagnostics::TypeDiagnostic>>,
}

impl ProjectStore {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn project_loaded(&self) -> bool {
        self.inner.read().unwrap().is_some()
    }

    /// Clone the current project handle under a briefly-held read guard (#341).
    /// Every accessor goes through this, so nothing holds the `inner` lock
    /// while it computes; a queued reload writer only ever waits for a pointer
    /// clone.
    fn snapshot(&self) -> Option<Arc<LoadedProject>> {
        self.inner.read().unwrap().clone()
    }

    /// Re-walk the project root for `*.m1scr` files, refreshing the cached set
    /// without re-parsing the project. Cheap; called when a script file is
    /// created or deleted (an edit to an existing script doesn't change the set).
    pub fn refresh_scripts(&self) {
        let stale_prj = {
            let mut guard = self.inner.write().unwrap();
            match guard.as_mut() {
                // The common case: no request holds a snapshot right now, so
                // the handle is unique and the set updates in place.
                Some(arc) => match Arc::get_mut(arc) {
                    Some(lp) => {
                        lp.script_files = walk_scripts(&lp.root);
                        None
                    }
                    // A request (or an in-flight graph/eval build) still holds
                    // a snapshot. `Project` isn't `Clone`, so fall back to a
                    // full reload from disk below — rare (a script was created
                    // or deleted in the same instant) and always correct.
                    None => Some(arc.m1prj_path.clone()),
                },
                None => None,
            }
        };
        if let Some(prj) = stale_prj {
            let _ = self.load_from(&prj);
        }
        // The script set changed (file created/deleted) — drop the stale graph
        // and the cross-script analysis (its parse/taints cover the old set).
        self.invalidate_call_graph();
        self.invalidate_analysis();
    }

    /// Read access to the loaded project for the feature handlers. `f` runs on
    /// an `Arc` snapshot with no store lock held (#341), so a slow `f` (a
    /// project-wide rename, a reference walk) cannot convoy other handlers
    /// behind a queued reload writer.
    pub fn with_project<R>(&self, f: impl FnOnce(Option<&LoadedProject>) -> R) -> R {
        let lp = self.snapshot();
        f(lp.as_deref())
    }

    /// Drop the cached call-hierarchy graph so the next call-hierarchy request
    /// rebuilds it from the live buffers. Cheap (clears one cell); called on any
    /// document edit/open/close/save and on every project reload — the graph reads
    /// open buffers, so it must reflect the latest keystroke.
    ///
    /// The cross-script analysis cache (`Self::analysis`) is deliberately *not*
    /// dropped here: it reads disk, not buffers, so a keystroke on an unsaved
    /// buffer cannot change it, and its own content-hash key rebuilds it when a
    /// file's on-disk bytes actually change. It is cleared only at the project-
    /// model / file-set boundaries via [`Self::invalidate_analysis`].
    ///
    /// The cached eval trace (E3) *is* dropped here, conservatively: like the
    /// analysis it reads disk (the loaded project + saved scripts + any
    /// scenario/log), but unlike the analysis it carries no content hash over
    /// those inputs, so there is no cheap "did the world change" key to rebuild
    /// on. Clearing the cell is free and only costs anything when eval is
    /// enabled (opt-in); the next hover/inlay request rebuilds once.
    pub fn invalidate_call_graph(&self) {
        *self.call_graph.write().unwrap() = None;
        *self.eval_cache.write().unwrap() = None;
    }

    /// Run `f` against the loaded project and its cached evaluation, building (and
    /// caching) the trace with `build` on a cache miss and reusing it on a hit.
    /// Shaped exactly like [`Self::with_call_graph`], so hover/inlay never run a
    /// trace per request: the first request after an invalidation (or after a
    /// config / reload-generation change) builds once; every subsequent request
    /// with the same `EvalKey` reads the cache.
    ///
    /// `build` produces an [`EvalOutcome`] (trace + provenance + fail-loud issues);
    /// only the trace and provenance are cached here. The caller surfaces the
    /// outcome's `issues` once via `window/logMessage` — but since `build` runs
    /// only on a miss, those issues are logged once per (re)build, not per hover.
    ///
    /// `f` receives `None` only when no project is loaded (then `build` never
    /// runs), mirroring [`Self::with_call_graph`]. On a hit or a fresh build it
    /// receives the loaded project, the shared [`Arc<Trace>`], and the
    /// [`Provenance`].
    ///
    /// Note: the build runs under the cache's write lock, so a slow whole-project
    /// run serialises concurrent first-requests (the second waits, then hits the
    /// just-built cache rather than building again). Callers that must not block
    /// the async runtime wrap this in `tokio::task::block_in_place` (the pattern
    /// used for the call-graph reads), so the run happens off the runtime worker.
    pub fn with_eval<R>(
        &self,
        cfg: &EvalConfig,
        build: impl FnOnce(&LoadedProject) -> EvalOutcome,
        f: impl FnOnce(Option<(&LoadedProject, &Arc<Trace>, &Provenance)>, &[String]) -> R,
    ) -> R {
        // Build from an Arc snapshot, holding only the cache lock (#341): a
        // reload racing the build swaps the project freely, then its
        // invalidation blocks on the cache lock until the build stores — so a
        // stale trace never outlives the invalidation that supersedes it.
        let Some(lp) = self.snapshot() else {
            return f(None, &[]);
        };
        let lp = lp.as_ref();
        let key = EvalKey::new(self.generation.load(Ordering::Acquire), cfg);
        let mut cache = self.eval_cache.write().unwrap();
        // Rebuild on a miss or a stale key (config or reload generation changed).
        // The build's fail-loud issues (e.g. a configured scenario that failed
        // and fell back to the offline default) are handed to `f` on exactly the
        // request that rebuilt — never dropped (a configured-source fallback
        // used to be completely silent), never repeated on cache hits.
        let mut fresh_issues: Vec<String> = Vec::new();
        if cache.as_ref().map(|c| c.key) != Some(key) {
            let outcome = build(lp);
            fresh_issues = outcome.issues;
            *cache = Some(CachedEval {
                trace: Arc::new(outcome.trace),
                provenance: outcome.provenance,
                key,
            });
        }
        let cached = cache.as_ref().expect("just built or hit");
        f(Some((lp, &cached.trace, &cached.provenance)), &fresh_issues)
    }

    /// Drop the cross-script analysis cache so the next diagnostics/type request
    /// rebuilds the parse, the channel-taint solve, and the project-scope
    /// diagnostics from the current project + on-disk scripts. Called only where
    /// the project model or the script *set* changes without the surviving files'
    /// content hash moving (project (re)load, `.m1cfg`/`.m1dbc` change, a script
    /// created/deleted) — a plain edit to an existing script is picked up by the
    /// content hash instead.
    pub fn invalidate_analysis(&self) {
        *self.analysis.write().unwrap() = None;
    }

    /// Drop `Self::analysis` when the on-disk corpus no longer matches the hash
    /// it was built from — the **once-per-request** freshness probe (#339).
    /// Callers are the request boundaries (the pull handlers, the push publish,
    /// [`Self::project_diagnostics_with`]); the per-script hot path
    /// ([`Self::with_cross_script`]) never probes, so one `workspace/diagnostic`
    /// poll over N scripts pays N probe reads once, not N × N.
    ///
    /// The probe streams every script's bytes through the hasher without
    /// materializing them — sources are read into memory only by an actual
    /// rebuild (`ensure_analysis`) — so a warm probe allocates nothing
    /// that outlives it. No project loaded clears the cache and returns.
    pub fn revalidate_analysis(&self) {
        use std::hash::{Hash, Hasher};
        let Some(script_files) = self.with_project(|p| p.map(|lp| lp.script_files.clone())) else {
            *self.analysis.write().unwrap() = None;
            return;
        };
        if self.analysis.read().unwrap().is_none() {
            return; // nothing to invalidate; the next ensure_analysis builds
        }
        // Hash exactly the inputs ensure_analysis hashes (same skip conditions),
        // so a matching corpus produces a matching hash. Tolerant decode
        // (UTF-8 → Windows-1252) so a `°`-bearing MoTeC script is included; an
        // unreadable file is skipped (it simply contributes nothing).
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        for path in &script_files {
            if path.file_name().and_then(|s| s.to_str()).is_none() {
                continue;
            }
            let Some(src) = crate::disk_read::read_disk(path) else {
                continue;
            };
            path.hash(&mut hasher);
            src.hash(&mut hasher);
        }
        let content_hash = hasher.finish();
        let stale = self
            .analysis
            .read()
            .unwrap()
            .as_ref()
            .is_some_and(|a| a.content_hash != content_hash);
        if stale {
            let mut guard = self.analysis.write().unwrap();
            // Re-check under the write lock: a concurrent rebuild may have just
            // stored an analysis matching the disk state we hashed.
            if guard
                .as_ref()
                .is_some_and(|a| a.content_hash != content_hash)
            {
                *guard = None;
            }
        }
    }

    /// Build `Self::analysis` if the cache is empty; a no-op when populated.
    /// Freshness is the caller's job via [`Self::revalidate_analysis`] at
    /// request boundaries — this deliberately does NOT probe the disk, so the
    /// per-script paths inside one request reuse the cache with zero I/O
    /// (#339).
    ///
    /// The rebuild is serialized on `analysis_rebuild` (double-checked, #340):
    /// concurrent first-requests after an invalidation run the whole-corpus
    /// parse + taint solve once — the losers wait, then read the freshly built
    /// cache. The expensive work itself runs with no RwLock held, as before.
    fn ensure_analysis(&self) {
        use std::hash::{Hash, Hasher};
        if self.analysis.read().unwrap().is_some() {
            return;
        }
        let _rebuild = self.analysis_rebuild.lock().unwrap();
        // Double-check: the winner may have built while we waited on the mutex.
        if self.analysis.read().unwrap().is_some() {
            return;
        }
        let Some(script_files) = self.with_project(|p| p.map(|lp| lp.script_files.clone())) else {
            return; // no project: nothing to build
        };
        // The taints below are solved against the project model as of NOW;
        // remember which model that is so the store can be skipped if a reload
        // supersedes it mid-build (#341 — previously the build implicitly
        // blocked the reload by holding the project read lock).
        let generation = self.generation.load(Ordering::Acquire);
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        let mut scripts: Vec<(String, String)> = Vec::with_capacity(script_files.len());
        for path in &script_files {
            let Some(name) = path.file_name().and_then(|s| s.to_str()) else {
                continue;
            };
            let Some(src) = crate::disk_read::read_disk(path) else {
                continue;
            };
            path.hash(&mut hasher);
            src.hash(&mut hasher);
            scripts.push((name.to_string(), src));
        }
        let content_hash = hasher.finish();

        // Reparse once and re-solve the cross-script taint graph (both
        // expensive, both run here with no RwLock held).
        let parsed = m1_typecheck::parsed::parse_all(&scripts);
        let taints = self
            .with_project(|p| p.map(|lp| m1_typecheck::cross_script::solve(&lp.project, &parsed)))
            .unwrap_or_default();
        // A reload changed the model mid-build: this analysis was solved
        // against the superseded project, so drop it — the next request
        // rebuilds against the fresh one (its invalidate_analysis already
        // cleared the cell, and storing here would resurrect stale taints).
        if self.generation.load(Ordering::Acquire) != generation {
            return;
        }
        *self.analysis.write().unwrap() = Some(ProjectAnalysis {
            content_hash,
            parsed,
            taints,
            diags: std::collections::HashMap::new(),
        });
    }

    /// Run `f` against the loaded project and its solved cross-script channel
    /// taints, so a per-script editor check can seed the invalid-value analysis
    /// with the project-wide solve (cross-file T080/T081 reach this file's sinks
    /// with their provenance — parity with the CLI's `check_script_with_channels`,
    /// #78 P3). The taints are built and cached lazily by `Self::ensure_analysis`;
    /// `f` receives `None` only when no project is loaded.
    pub fn with_cross_script<R>(
        &self,
        f: impl FnOnce(Option<(&LoadedProject, &m1_typecheck::cross_script::ChannelTaints)>) -> R,
    ) -> R {
        self.ensure_analysis();
        let Some(lp) = self.snapshot() else {
            return f(None);
        };
        let analysis = self.analysis.read().unwrap();
        let empty = m1_typecheck::cross_script::ChannelTaints::default();
        let taints = analysis.as_ref().map(|a| &a.taints).unwrap_or(&empty);
        f(Some((&lp, taints)))
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
        // Build from an Arc snapshot, holding only the cache lock (#341): the
        // whole-corpus read+parse no longer blocks a reload writer (nor, via
        // writer preference, every other handler queued behind it). An
        // invalidation racing the build blocks on the cache lock until the
        // build stores, then clears — a stale graph never survives it.
        let Some(lp) = self.snapshot() else {
            return f(None);
        };
        let mut graph = self.call_graph.write().unwrap();
        if graph.is_none() {
            *graph = Some(build(&lp));
        }
        f(Some((&lp, graph.as_ref().unwrap())))
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
    /// - **T107** dbc-init-missing (error 1375) — `dbc_init::check`.
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
    /// `Project::function_symbol_for_script`.
    ///
    /// This whole-workspace re-read used to run on project (re)load only, but
    /// pull diagnostics (#140) call it per `workspace/diagnostic` poll, and
    /// pull-capable clients (VS Code, nvim ≥0.10) re-poll on every open/change —
    /// so it *is* now on a hot path. The result is therefore memoized (keyed by
    /// the `rate_inversion` flag, its only non-project input) and dropped by
    /// [`Self::invalidate_call_graph`] on any edit/open/close/save/reload.
    pub fn project_diagnostics_with(
        &self,
        rate_inversion: bool,
    ) -> Vec<m1_typecheck::diagnostics::TypeDiagnostic> {
        // A request boundary (#339): drop the shared parse/taint cache if the
        // corpus changed on disk, then build it if missing. The probe is a
        // no-op read-through on the common unchanged-corpus path.
        self.revalidate_analysis();
        self.ensure_analysis();

        // Serve the memoized set when it was already computed under this flag.
        if let Some(diags) = self
            .analysis
            .read()
            .unwrap()
            .as_ref()
            .and_then(|a| a.diags.get(&rate_inversion).cloned())
        {
            return diags;
        }

        // Miss for this flag: run the project-wide passes over the *cached* parse
        // (no re-read, no re-parse) and memoize the result.
        let computed = self.with_project(|p| {
            let lp = p?;
            let analysis = self.analysis.read().unwrap();
            let parsed: &[m1_typecheck::parsed::ParsedScript] = analysis
                .as_ref()
                .map(|a| a.parsed.as_slice())
                .unwrap_or(&[]);
            let mut v = lp.project.missing_cfg_parameters();
            v.extend(lp.project.audit());
            v.extend(lp.project.audit_tags());
            v.extend(lp.project.audit_display_units());
            // T088 and T097 (recursive-call, m1-typecheck#187) are default-on like
            // the CLI; T089 stays behind its opt-in.
            v.extend(m1_typecheck::schedule::check(
                &lp.project,
                parsed,
                true,
                rate_inversion,
                true,
            ));
            v.extend(m1_typecheck::schedule::check_usage(
                &lp.project,
                parsed,
                true,
                true,
            ));
            // T107 dbc-init-missing (M1 Build Error 1375): a DBC used but never
            // Init'd — default-on, whole-project.
            v.extend(m1_typecheck::dbc_init::check(&lp.project, parsed));
            Some(v)
        });
        // Only cache when a project is loaded; with none loaded the empty result
        // is cheap and a project may load before the next poll.
        match computed {
            Some(v) => {
                if let Some(a) = self.analysis.write().unwrap().as_mut() {
                    a.diags.insert(rate_inversion, v.clone());
                }
                v
            }
            None => Vec::new(),
        }
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
            Ok(mut project) => {
                let script_files = walk_scripts(&root);
                // Infer user-function/method return types from the whole project's
                // scripts once at load — parity with the CLI, which runs
                // `infer_return_types` before checking callers so their call sites
                // resolve to a concrete type instead of staying Unknown (#110).
                // Done at load (not per keystroke) since it reads disk; refreshed
                // by a reload when the project model changes.
                let scripts = scripts_from_disk(&script_files);
                project.infer_return_types(&m1_typecheck::parsed::parse_all(&scripts));
                *self.inner.write().unwrap() = Some(Arc::new(LoadedProject {
                    project,
                    root,
                    m1prj_path: m1prj_path.to_path_buf(),
                    m1cfg_path,
                    dbc_paths,
                    script_files,
                }));
                self.bump_generation();
                self.invalidate_call_graph();
                self.invalidate_analysis();
                Ok(true)
            }
            Err(e) => {
                *self.inner.write().unwrap() = None;
                self.invalidate_call_graph();
                self.invalidate_analysis();
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
        let mut project = Project::from_xml(m1prj_text)
            .map_err(|e| e.to_string())
            .and_then(|p| augment(p, &root, &m1cfg_path, &dbc_paths))?;
        let script_files = walk_scripts(&root);
        // Re-infer return types against the refreshed model (as at load, #110).
        let scripts = scripts_from_disk(&script_files);
        project.infer_return_types(&m1_typecheck::parsed::parse_all(&scripts));
        *self.inner.write().unwrap() = Some(Arc::new(LoadedProject {
            project,
            root,
            m1prj_path,
            m1cfg_path,
            dbc_paths,
            script_files,
        }));
        self.bump_generation();
        self.invalidate_call_graph();
        self.invalidate_analysis();
        Ok(true)
    }

    /// Bump the monotonic reload generation. Called on every successful project
    /// (re)load so the eval cache's [`EvalKey`] — which folds the generation in —
    /// detects the model change and rebuilds, even if the resolved [`EvalConfig`]
    /// is byte-identical across the reload.
    fn bump_generation(&self) {
        self.generation.fetch_add(1, Ordering::Release);
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

    // #259 + finding-2: project-scope diagnostics are memoized over a shared,
    // content-hashed parse cache so a pull-diagnostic poll on an unchanged corpus
    // is a cache hit. Crucially, a buffer edit (which fires `invalidate_call_graph`
    // but does NOT change disk) must NOT drop the cache — only a project reload /
    // script-set change (`invalidate_analysis`) or an actual on-disk content change
    // rebuilds it.
    #[test]
    fn project_diagnostics_are_memoized_and_invalidated() {
        let tmp = tempfile::tempdir().unwrap();
        write_project(tmp.path());
        std::fs::write(tmp.path().join("A.m1scr"), "x = 1;\n").unwrap();
        let store = ProjectStore::new();
        assert!(store.discover_and_load(tmp.path()).unwrap());

        // Load (-> invalidate_analysis) leaves the cache empty.
        assert!(
            store.analysis.read().unwrap().is_none(),
            "cache starts empty after load"
        );

        // First call builds the analysis and memoizes the diags for this flag.
        let first_len = store.project_diagnostics_with(false).len();
        {
            let cached = store.analysis.read().unwrap();
            let a = cached.as_ref().expect("first call populates the cache");
            assert_eq!(
                a.diags.get(&false).map(Vec::len),
                Some(first_len),
                "cached set matches the returned set"
            );
        }

        // A second call with the same flag is served from the cache.
        assert_eq!(store.project_diagnostics_with(false).len(), first_len);

        // A buffer edit fires this hook; the disk-sourced analysis must survive it.
        store.invalidate_call_graph();
        assert!(
            store.analysis.read().unwrap().is_some(),
            "invalidate_call_graph must NOT drop the disk-sourced analysis cache"
        );
        assert_eq!(store.project_diagnostics_with(false).len(), first_len);

        // A project-model / script-set change drops it explicitly.
        store.invalidate_analysis();
        assert!(
            store.analysis.read().unwrap().is_none(),
            "invalidate_analysis drops the analysis cache"
        );

        // An actual on-disk content change rebuilds it on the next poll (the
        // content hash moved), even without an explicit invalidation.
        let _ = store.project_diagnostics_with(false); // repopulate
        std::fs::write(tmp.path().join("A.m1scr"), "y = 2;\n// changed\n").unwrap();
        let _ = store.project_diagnostics_with(false);
        let cached = store.analysis.read().unwrap();
        assert!(
            cached.is_some(),
            "a content change rebuilds the analysis cache"
        );
    }

    // #341: refresh_scripts updates the script set even while a request still
    // holds a project snapshot (the Arc isn't unique, so the in-place path is
    // unavailable and it falls back to a full reload). The held snapshot keeps
    // its point-in-time view — that's the contract that lets requests run
    // without holding the store lock.
    #[test]
    fn refresh_scripts_updates_even_while_a_snapshot_is_held() {
        let tmp = tempfile::tempdir().unwrap();
        write_project(tmp.path());
        let scripts = tmp.path().join("Scripts");
        std::fs::create_dir_all(&scripts).unwrap();
        std::fs::write(scripts.join("A.m1scr"), "x = 1;\n").unwrap();
        let store = ProjectStore::new();
        store.discover_and_load(tmp.path()).unwrap();

        // Simulate an in-flight request pinning the current project snapshot.
        let held = store.snapshot().expect("project loaded");
        assert_eq!(held.script_files.len(), 1);

        std::fs::write(scripts.join("B.m1scr"), "y = 2;\n").unwrap();
        store.refresh_scripts();
        store.with_project(|p| {
            assert_eq!(
                p.unwrap().script_files.len(),
                2,
                "refresh must see the new script despite the held snapshot"
            );
        });
        assert_eq!(
            held.script_files.len(),
            1,
            "the held snapshot keeps its point-in-time view"
        );
    }

    // #339: the per-script hot path (`with_cross_script`) must serve the warm
    // analysis cache without probing the disk — freshness is the request
    // boundary's job via `revalidate_analysis`. An on-disk edit therefore
    // becomes visible to the hot path only after a revalidation; before it,
    // the warm cache is reused as-is (zero corpus I/O per script).
    #[test]
    fn with_cross_script_serves_warm_cache_until_revalidated() {
        let tmp = tempfile::tempdir().unwrap();
        write_project(tmp.path());
        std::fs::write(tmp.path().join("A.m1scr"), "x = 1;\n").unwrap();
        let store = ProjectStore::new();
        assert!(store.discover_and_load(tmp.path()).unwrap());

        // Warm the cache and capture the hash it was built from.
        let _ = store.project_diagnostics_with(false);
        let warm_hash = store
            .analysis
            .read()
            .unwrap()
            .as_ref()
            .unwrap()
            .content_hash;

        // Change the corpus on disk. The hot path must NOT notice (no probe).
        std::fs::write(tmp.path().join("A.m1scr"), "y = 2;\n").unwrap();
        store.with_cross_script(|cs| assert!(cs.is_some()));
        assert_eq!(
            store
                .analysis
                .read()
                .unwrap()
                .as_ref()
                .unwrap()
                .content_hash,
            warm_hash,
            "with_cross_script must reuse the warm cache without re-probing disk"
        );

        // A request-boundary revalidation drops the stale cache…
        store.revalidate_analysis();
        assert!(
            store.analysis.read().unwrap().is_none(),
            "revalidation drops the stale analysis"
        );

        // …and the next hot-path call rebuilds against the new corpus.
        store.with_cross_script(|cs| assert!(cs.is_some()));
        let new_hash = store
            .analysis
            .read()
            .unwrap()
            .as_ref()
            .unwrap()
            .content_hash;
        assert_ne!(new_hash, warm_hash, "rebuild reflects the changed corpus");
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

    use crate::eval::config::EvalConfig;
    use crate::eval::engine::Provenance;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// The mini fixture directory used by the eval-cache tests — a self-contained
    /// synthetic project (no proprietary content) that the engine can run.
    fn mini_dir() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/mini")
    }

    // E3: two `with_eval` calls in a row build the trace exactly once — the second
    // hits the cache. A shared build counter proves no second run happened.
    #[test]
    fn with_eval_builds_once_then_hits_cache() {
        let store = ProjectStore::new();
        assert!(store.discover_and_load(&mini_dir()).unwrap());
        let cfg = EvalConfig::default();
        let builds = AtomicUsize::new(0);
        let build = |lp: &LoadedProject| {
            builds.fetch_add(1, Ordering::SeqCst);
            crate::eval::engine::evaluate(lp, &cfg)
        };

        // First request: a cache miss, so it builds once.
        let prov1 = store.with_eval(&cfg, build, |e, _issues| {
            e.map(|(_, _, prov)| prov.clone()).expect("project loaded")
        });
        assert_eq!(builds.load(Ordering::SeqCst), 1, "first request builds");
        assert_eq!(prov1, Provenance::OfflineDefault);

        // Second request with the same config + generation: a cache hit, no rebuild.
        let prov2 = store.with_eval(&cfg, build, |e, _issues| {
            e.map(|(_, _, prov)| prov.clone()).expect("project loaded")
        });
        assert_eq!(
            builds.load(Ordering::SeqCst),
            1,
            "second request reuses the cache — no second build"
        );
        assert_eq!(prov2, prov1, "same cached trace, same provenance");
    }

    // Fail-loud issues from a rebuild (e.g. "configured scenario failed; fell
    // back to the offline default") must surface to the caller ON the build —
    // previously `with_eval` dropped `outcome.issues` on the floor, so a
    // configured-source fallback was completely silent (2026-07-19 review B3).
    // A cache hit hands back no issues, so the backend notifies once per
    // rebuild, not once per hover.
    #[test]
    fn with_eval_surfaces_build_issues_once() {
        let store = ProjectStore::new();
        assert!(store.discover_and_load(&mini_dir()).unwrap());
        // A configured scenario path that does not exist: evaluate() fails loud
        // on it and falls back to the offline default, carrying one issue line.
        let cfg = EvalConfig {
            enabled: true,
            scenario: Some(PathBuf::from("no/such/scenario.toml")),
            ..EvalConfig::default()
        };
        let build = |lp: &LoadedProject| crate::eval::engine::evaluate(lp, &cfg);

        // First request: the build's fallback issue is handed to the callback.
        let issues1 = store.with_eval(&cfg, build, |_e, issues| issues.to_vec());
        assert_eq!(issues1.len(), 1, "fallback issue surfaced: {issues1:?}");
        assert!(
            issues1[0].contains("scenario"),
            "issue names the failed source: {issues1:?}"
        );

        // Second request: cache hit — no fresh issues, so no re-notification.
        let issues2 = store.with_eval(&cfg, build, |_e, issues| issues.to_vec());
        assert!(
            issues2.is_empty(),
            "a cache hit must not repeat the notification: {issues2:?}"
        );
    }

    // E3: a configuration change bumps the `EvalKey` (it hashes the resolved
    // `EvalConfig`), forcing exactly one rebuild on the next request.
    #[test]
    fn config_change_forces_rebuild() {
        let store = ProjectStore::new();
        assert!(store.discover_and_load(&mini_dir()).unwrap());
        let builds = AtomicUsize::new(0);

        let cfg_a = EvalConfig::default();
        store.with_eval(
            &cfg_a,
            |lp| {
                builds.fetch_add(1, Ordering::SeqCst);
                crate::eval::engine::evaluate(lp, &cfg_a)
            },
            |e, _issues| assert!(e.is_some()),
        );
        assert_eq!(builds.load(Ordering::SeqCst), 1);

        // A different config (inlay_values toggled) has a different hash → miss.
        let cfg_b = EvalConfig {
            inlay_values: true,
            ..EvalConfig::default()
        };
        store.with_eval(
            &cfg_b,
            |lp| {
                builds.fetch_add(1, Ordering::SeqCst);
                crate::eval::engine::evaluate(lp, &cfg_b)
            },
            |e, _issues| assert!(e.is_some()),
        );
        assert_eq!(
            builds.load(Ordering::SeqCst),
            2,
            "a config change forces a rebuild"
        );

        // Reverting to the first config rebuilds again (the cache holds cfg_b's key).
        store.with_eval(
            &cfg_a,
            |lp| {
                builds.fetch_add(1, Ordering::SeqCst);
                crate::eval::engine::evaluate(lp, &cfg_a)
            },
            |e, _issues| assert!(e.is_some()),
        );
        assert_eq!(builds.load(Ordering::SeqCst), 3);
    }

    // E3: a project reload bumps the reload generation, which is part of the
    // `EvalKey`, so the cached trace is stale and the next request rebuilds. The
    // shared `invalidate_call_graph` hook also drops the cache outright.
    #[test]
    fn reload_invalidates_eval_cache() {
        let tmp = tempfile::tempdir().unwrap();
        write_project(tmp.path());
        let store = ProjectStore::new();
        assert!(store.discover_and_load(tmp.path()).unwrap());
        let cfg = EvalConfig::default();
        let builds = AtomicUsize::new(0);
        let build = |lp: &LoadedProject| {
            builds.fetch_add(1, Ordering::SeqCst);
            crate::eval::engine::evaluate(lp, &cfg)
        };

        store.with_eval(&cfg, build, |e, _issues| assert!(e.is_some()));
        assert_eq!(builds.load(Ordering::SeqCst), 1);

        // Reload the project (same config): the generation bumps, cache is stale.
        assert!(store.discover_and_load(tmp.path()).unwrap());
        store.with_eval(&cfg, build, |e, _issues| assert!(e.is_some()));
        assert_eq!(
            builds.load(Ordering::SeqCst),
            2,
            "a reload invalidates the cached trace"
        );
    }

    // E3: the shared `invalidate_call_graph` hook (edit/open/close/save) drops the
    // eval cache too, so the next request rebuilds.
    #[test]
    fn invalidate_call_graph_drops_eval_cache() {
        let store = ProjectStore::new();
        assert!(store.discover_and_load(&mini_dir()).unwrap());
        let cfg = EvalConfig::default();
        let builds = AtomicUsize::new(0);
        let build = |lp: &LoadedProject| {
            builds.fetch_add(1, Ordering::SeqCst);
            crate::eval::engine::evaluate(lp, &cfg)
        };

        store.with_eval(&cfg, build, |e, _issues| assert!(e.is_some()));
        assert_eq!(builds.load(Ordering::SeqCst), 1);
        assert!(
            store.eval_cache.read().unwrap().is_some(),
            "the build populated the eval cache"
        );

        // A buffer edit / open / close / save funnels through this hook.
        store.invalidate_call_graph();
        assert!(
            store.eval_cache.read().unwrap().is_none(),
            "invalidate_call_graph drops the eval cache"
        );
        store.with_eval(&cfg, build, |e, _issues| assert!(e.is_some()));
        assert_eq!(
            builds.load(Ordering::SeqCst),
            2,
            "after invalidation the next request rebuilds"
        );
    }

    // E3: `with_eval` passes `None` to its closure when no project is loaded, and
    // never builds — mirroring `with_call_graph`'s project-less behaviour.
    #[test]
    fn with_eval_no_project_yields_none_without_building() {
        let store = ProjectStore::new();
        let cfg = EvalConfig::default();
        let builds = AtomicUsize::new(0);
        let got = store.with_eval(
            &cfg,
            |lp| {
                builds.fetch_add(1, Ordering::SeqCst);
                crate::eval::engine::evaluate(lp, &cfg)
            },
            |e, _issues| e.is_some(),
        );
        assert!(!got, "no project → None");
        assert_eq!(builds.load(Ordering::SeqCst), 0, "no project → no build");
    }

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
