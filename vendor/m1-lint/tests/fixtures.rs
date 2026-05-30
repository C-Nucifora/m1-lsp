//! Fixture-based acceptance tests.
//!
//! Each fixture is a pair `<stem>.m1scr` (input) and `<stem>.diag` (expected
//! diagnostics, one `line:col:code:message-fragment` per line). The harness
//! asserts every expected diagnostic is present (by line, code, and message
//! fragment).

use m1_lint::registry::Registry;
use m1_lint::runner::Runner;
use std::path::Path;

fn runner() -> Runner {
    Runner::new(Registry::default_v2())
}

fn run_fixture(stem: &str) {
    let fixture_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    let source_path = fixture_dir.join(format!("{}.m1scr", stem));
    let diag_path = fixture_dir.join(format!("{}.diag", stem));

    let source = std::fs::read_to_string(&source_path)
        .unwrap_or_else(|_| panic!("fixture source not found: {}", source_path.display()));

    let expected_raw = std::fs::read_to_string(&diag_path)
        .unwrap_or_else(|_| panic!("fixture diag not found: {}", diag_path.display()));

    let result = runner().run_source(&source);

    for line in expected_raw.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let parts: Vec<&str> = line.splitn(4, ':').collect();
        assert_eq!(parts.len(), 4, "malformed fixture line: {}", line);
        let exp_line: u32 = parts[0].parse().expect("line number");
        let exp_code = parts[2];
        let exp_msg_frag = parts[3];

        let found = result.diagnostics.iter().any(|d| {
            d.inner.range.start.line + 1 == exp_line
                && d.code.to_string() == exp_code
                && d.inner.message.contains(exp_msg_frag)
        });

        assert!(
            found,
            "expected diagnostic {}:{} '{}' not found for fixture '{}'.\nActual:\n{:#?}",
            exp_line, exp_code, exp_msg_frag, stem, result.diagnostics
        );
    }
}

#[test]
fn fixture_l001_long_line() {
    run_fixture("l001_long_line");
}
#[test]
fn fixture_l002_trailing_ws() {
    run_fixture("l002_trailing_ws");
}
#[test]
fn fixture_l003_no_final_newline() {
    run_fixture("l003_no_final_newline");
}
#[test]
fn fixture_l004_eq_eq() {
    run_fixture("l004_eq_eq");
}
#[test]
fn fixture_l005_logical_ops() {
    run_fixture("l005_logical_ops");
}
#[test]
fn fixture_l006_float_eq() {
    run_fixture("l006_float_eq");
}
#[test]
fn fixture_l007_op_spacing() {
    run_fixture("l007_op_spacing");
}
#[test]
fn fixture_l008_nesting() {
    run_fixture("l008_nesting");
}
#[test]
fn fixture_l010_tabs() {
    run_fixture("l010_tabs");
}
#[test]
fn fixture_l011_comment() {
    run_fixture("l011_comment");
}
