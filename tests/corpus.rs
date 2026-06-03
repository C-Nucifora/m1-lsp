//! analyze() must not panic on any real script. Corpus path via M1_CORPUS_PATH,
//! else the sibling m1-example example project. Skipped if the dir is absent.
use m1_lsp::analysis::{NoLint, NoTypes, analyze};
use m1_lsp::features::call_hierarchy::CallGraph;
use m1_lsp::features::locate::path_at_byte;
use m1_lsp::line_index::{LineIndex, PositionEncoding};
use m1_lsp::project_store::ProjectStore;
use std::path::{Path, PathBuf};
use tower_lsp::lsp_types::Url;

fn corpus_dir() -> PathBuf {
    match std::env::var_os("M1_CORPUS_PATH") {
        Some(p) => PathBuf::from(p),
        None => Path::new(env!("CARGO_MANIFEST_DIR")).join("../m1-example/UQR-EV/01.00/Scripts"),
    }
}

#[test]
fn analyze_never_panics_on_corpus() {
    let dir = corpus_dir();
    if !dir.is_dir() {
        eprintln!("corpus dir absent ({}); skipping", dir.display());
        return;
    }
    let mut checked = 0;
    for entry in std::fs::read_dir(&dir).expect("read corpus dir") {
        let path = entry.expect("dir entry").path();
        if path.extension().and_then(|e| e.to_str()) != Some("m1scr") {
            continue;
        }
        let src = std::fs::read_to_string(&path).expect("read script");
        let li = LineIndex::new(&src);
        let uri = Url::from_file_path(&path).unwrap();
        let _ = analyze(
            &uri,
            &src,
            &li,
            PositionEncoding::Utf16,
            &NoLint,
            &NoTypes,
            &m1_lsp::config::DiagFilter::default(),
        );
        // locate smoke pass: path_at_byte at strided offsets must not panic.
        let cst = m1_core::parse(&src);
        for off in (0..src.len()).step_by(64) {
            let _ = path_at_byte(cst.root(), off);
        }
        checked += 1;
    }
    assert!(checked > 0, "no scripts found in {}", dir.display());
}

/// Build the call-hierarchy data-flow graph over the whole real project and
/// assert it found cross-script channel dependencies (some channel is written by
/// one script and read by another). Skipped when the corpus is absent.
#[test]
fn call_graph_builds_over_corpus() {
    // The project root is the corpus Scripts dir's grandparent (…/01.00).
    let scripts = corpus_dir();
    let Some(root) = scripts.parent() else {
        return;
    };
    if !root.join("Project.m1prj").is_file() {
        eprintln!("corpus project absent ({}); skipping", root.display());
        return;
    }
    let store = ProjectStore::new();
    if !store.discover_and_load(root).unwrap_or(false) {
        eprintln!("could not load corpus project; skipping");
        return;
    }
    let no_open = |_: &Url| None;
    store.with_project(|p| {
        let lp = p.expect("loaded");
        let g = CallGraph::build(lp, PositionEncoding::Utf16, &no_open);
        assert!(!g.scripts.is_empty(), "graph must contain scripts");
        // At least one channel is both produced by one script and consumed by
        // another — a genuine cross-script data-flow edge.
        let cross = g.scripts.iter().enumerate().any(|(i, s)| {
            s.writes.keys().any(|ch| {
                g.scripts
                    .iter()
                    .enumerate()
                    .any(|(j, o)| j != i && o.reads.contains_key(ch))
            })
        });
        assert!(
            cross,
            "expected at least one cross-script channel dependency"
        );
    });
}
