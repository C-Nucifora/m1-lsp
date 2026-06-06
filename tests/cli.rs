//! CLI behaviour of the `m1-lsp` binary: `--help` / `--version` / unknown flags
//! must print and exit, never fall through to the stdio serve loop (#176).
use std::io::Read;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// Run the built `m1-lsp` binary with `args` and a hard timeout. `stdin` is
/// `null` (EOF) so that if a flag is wrongly ignored and the server starts, it
/// exits on EOF rather than hanging the test. Returns `(success, combined
/// stdout+stderr)`, or `None` if it had to be killed after the timeout.
fn run(args: &[&str], secs: u64) -> Option<(bool, String)> {
    let mut child = Command::new(env!("CARGO_BIN_EXE_m1-lsp"))
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn m1-lsp");
    let deadline = Instant::now() + Duration::from_secs(secs);
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            let mut out = String::new();
            if let Some(mut so) = child.stdout.take() {
                let _ = so.read_to_string(&mut out);
            }
            let mut err = String::new();
            if let Some(mut se) = child.stderr.take() {
                let _ = se.read_to_string(&mut err);
            }
            return Some((status.success(), out + &err));
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return None;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn version_flag_prints_and_exits() {
    let (ok, out) = run(&["--version"], 15).expect("--version must exit, not hang");
    assert!(ok, "--version should exit 0; got: {out}");
    assert!(out.contains(env!("CARGO_PKG_VERSION")), "got: {out}");
    assert!(out.contains("m1-lsp"), "got: {out}");
}

#[test]
fn short_version_flag_works() {
    let (ok, out) = run(&["-V"], 15).expect("-V must exit, not hang");
    assert!(ok && out.contains(env!("CARGO_PKG_VERSION")), "got: {out}");
}

#[test]
fn help_flag_prints_usage_and_exits() {
    let (ok, out) = run(&["--help"], 15).expect("--help must exit, not hang");
    assert!(ok, "--help should exit 0; got: {out}");
    assert!(out.to_lowercase().contains("usage"), "got: {out}");
}

#[test]
fn unknown_flag_errors_and_exits() {
    let (ok, out) = run(&["--definitely-not-a-flag"], 15)
        .expect("an unknown flag must exit, not hang on stdin");
    assert!(!ok, "an unknown flag should exit non-zero; got: {out}");
    assert!(
        out.to_lowercase().contains("usage") || out.to_lowercase().contains("unknown"),
        "got: {out}"
    );
}
