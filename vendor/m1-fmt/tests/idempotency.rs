mod common;

#[test]
fn idempotency_corpus() {
    let scripts = common::corpus_scripts();
    assert!(!scripts.is_empty(), "no corpus scripts found");

    let mut failures = Vec::new();
    for (path, src) in &scripts {
        // Skip files with syntax errors
        let cst = m1_core::parse(src);
        if !cst.syntax_diagnostics().is_empty() {
            continue;
        }

        let once = match m1_fmt::format_str(src) {
            Ok(r) => r.output,
            Err(e) => {
                failures.push(format!("{}: format error: {}", path.display(), e));
                continue;
            }
        };
        let twice = match m1_fmt::format_str(&once) {
            Ok(r) => r.output,
            Err(e) => {
                failures.push(format!("{}: second format error: {}", path.display(), e));
                continue;
            }
        };
        if once != twice {
            failures.push(format!(
                "{}: not idempotent\n--- first ---\n{}\n--- second ---\n{}",
                path.display(),
                &once[..once.len().min(500)],
                &twice[..twice.len().min(500)],
            ));
        }
    }

    if !failures.is_empty() {
        panic!("idempotency failures:\n{}", failures.join("\n\n"));
    }
}
