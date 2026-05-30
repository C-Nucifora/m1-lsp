use std::path::{Path, PathBuf};

pub fn corpus_scripts() -> Vec<(PathBuf, String)> {
    // Corpus path is overridable via the `M1_CORPUS_PATH` env var; otherwise it
    // defaults to the sibling m1-example example project.
    let corpus_dir = match std::env::var_os("M1_CORPUS_PATH") {
        Some(p) => PathBuf::from(p),
        None => Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .join("m1-example/UQR-EV/01.00/Scripts"),
    };

    let mut scripts = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&corpus_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) == Some("m1scr") {
                if let Ok(src) = std::fs::read_to_string(&path) {
                    scripts.push((path, src));
                }
            }
        }
    }
    scripts
}
