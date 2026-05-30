//! Loads the example project and checks every script: must not panic. Skipped
//! unless both the project and corpus are present. Records a baseline count.
use m1_typecheck::project::Project;
use m1_typecheck::rules::check_script;
use std::path::{Path, PathBuf};

fn project_path() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("M1_PROJECT") {
        return Some(PathBuf::from(p));
    }
    let cand = Path::new(env!("CARGO_MANIFEST_DIR")).join("../EV-M1/UQR-EV/01.00/Project.m1prj");
    cand.is_file().then_some(cand)
}
fn corpus_dir() -> PathBuf {
    match std::env::var_os("M1_CORPUS_PATH") {
        Some(p) => PathBuf::from(p),
        None => Path::new(env!("CARGO_MANIFEST_DIR")).join("../EV-M1/UQR-EV/01.00/Scripts"),
    }
}

#[test]
fn checks_corpus_without_panicking() {
    let Some(proj_path) = project_path() else {
        eprintln!("project absent; skipping");
        return;
    };
    let dir = corpus_dir();
    if !dir.is_dir() {
        eprintln!("corpus absent; skipping");
        return;
    }
    let project = Project::load(&proj_path).expect("load project");
    let mut checked = 0;
    let mut total_diags = 0usize;
    for entry in std::fs::read_dir(&dir).expect("read corpus") {
        let path = entry.expect("entry").path();
        if path.extension().and_then(|e| e.to_str()) != Some("m1scr") {
            continue;
        }
        let src = std::fs::read_to_string(&path).expect("read");
        let r = check_script(&project, &path, &src);
        total_diags += r.diagnostics.len();
        checked += 1;
    }
    assert!(checked > 0, "no scripts found");
    eprintln!("checked {checked} scripts, {total_diags} type diagnostics");
}

#[test]
fn audits_project_names_without_panicking() {
    let Some(proj_path) = project_path() else {
        return;
    };
    let project = Project::load(&proj_path).expect("load project");
    let n = project.audit().len();
    eprintln!("project name-audit produced {n} T050 diagnostics");
    // No assertion on the exact count: the example project intentionally violates
    // the conventions. This guards only against a panic / API regression.
}
