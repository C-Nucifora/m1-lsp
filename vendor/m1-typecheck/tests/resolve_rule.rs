use m1_typecheck::diagnostics::TypeCode;
use m1_typecheck::project::Project;
use m1_typecheck::rules::check_script;
use std::path::Path;

fn proj() -> Project {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    Project::load(&dir.join("mini.m1prj")).unwrap()
}

#[test]
fn t001_flags_project_rooted_miss() {
    let p = proj();
    // "Foo Update.m1scr" maps to group Root.Foo; "Root.Foo.Missing" is project-rooted but absent.
    let src = "Root.Foo.Missing = 1;\n";
    let codes: Vec<_> = check_script(&p, Path::new("Foo Update.m1scr"), src)
        .diagnostics
        .iter()
        .map(|d| d.code)
        .collect();
    assert!(codes.contains(&TypeCode::T001));
    // Exactly one T001 (not one per path prefix).
    assert_eq!(codes.iter().filter(|c| **c == TypeCode::T001).count(), 1);
}

#[test]
fn t001_no_flag_for_known_symbol_or_library() {
    let p = proj();
    let src = "Speed = Calculate.Min(1, 2);\n"; // Speed resolves (group-relative); Calculate is opaque
    let codes: Vec<_> = check_script(&p, Path::new("Foo Update.m1scr"), src)
        .diagnostics
        .iter()
        .map(|d| d.code)
        .collect();
    assert!(!codes.contains(&TypeCode::T001));
}
