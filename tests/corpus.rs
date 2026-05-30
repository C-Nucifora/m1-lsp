//! analyze() must not panic on any real script. Corpus path via M1_CORPUS_PATH,
//! else the sibling EV-M1 example project. Skipped if the dir is absent.
use m1_lsp::analysis::{analyze, NoLint, NoTypes};
use m1_lsp::features::locate::path_at_byte;
use m1_lsp::line_index::{LineIndex, PositionEncoding};
use std::path::{Path, PathBuf};
use tower_lsp::lsp_types::Url;

fn corpus_dir() -> PathBuf {
    match std::env::var_os("M1_CORPUS_PATH") {
        Some(p) => PathBuf::from(p),
        None => Path::new(env!("CARGO_MANIFEST_DIR")).join("../EV-M1/UQR-EV/01.00/Scripts"),
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
        let _ = analyze(&uri, &src, &li, PositionEncoding::Utf16, &NoLint, &NoTypes);
        // locate smoke pass: path_at_byte at strided offsets must not panic.
        let cst = m1_core::parse(&src);
        for off in (0..src.len()).step_by(64) {
            let _ = path_at_byte(cst.root(), off);
        }
        checked += 1;
    }
    assert!(checked > 0, "no scripts found in {}", dir.display());
}
