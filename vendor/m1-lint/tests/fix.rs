//! Autofix acceptance tests: fix(in) == out, and fix is idempotent.

use std::path::Path;

use m1_lint::registry::Registry;
use m1_lint::runner::Runner;

fn run_fix(stem: &str) {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures_fix");
    let input = std::fs::read_to_string(dir.join(format!("{stem}.in.m1scr"))).unwrap();
    let expected = std::fs::read_to_string(dir.join(format!("{stem}.out.m1scr"))).unwrap();

    let runner = Runner::new(Registry::default_v2());
    let fixed = runner.fix_source(&input).unwrap().unwrap_or(input.clone());
    assert_eq!(fixed, expected, "fix mismatch for {stem}");

    // Idempotency: fixing the output yields no further change.
    assert_eq!(runner.fix_source(&expected).unwrap(), None, "not idempotent: {stem}");
}

#[test] fn fix_eq_op() { run_fix("eq_op"); }
#[test] fn fix_logical() { run_fix("logical"); }
#[test] fn fix_spacing() { run_fix("spacing"); }
#[test] fn fix_trailing() { run_fix("trailing"); }
#[test] fn fix_comment() { run_fix("comment"); }
