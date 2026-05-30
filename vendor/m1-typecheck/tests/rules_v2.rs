use m1_typecheck::diagnostics::TypeCode;
use m1_typecheck::project::Project;
use m1_typecheck::rules::check_script;
use std::path::Path;

fn proj() -> Project {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    Project::load(&dir.join("enums.m1prj"))
        .unwrap()
        .with_config(&dir.join("enums.m1cfg"))
        .unwrap()
}

fn codes(p: &Project, src: &str) -> Vec<TypeCode> {
    check_script(p, Path::new("Foo Update.m1scr"), src)
        .diagnostics
        .iter()
        .map(|d| d.code)
        .collect()
}

#[test]
fn t020_flags_non_member_typed_path() {
    let p = proj();
    // "Drive State.Nope" has the enum-type head but no such member.
    assert!(codes(&p, "driveMode = Drive State.Nope;\n").contains(&TypeCode::T020));
}

#[test]
fn t020_no_flag_for_real_member() {
    let p = proj();
    assert!(!codes(&p, "driveMode = Drive State.Idle;\n").contains(&TypeCode::T020));
}

#[test]
fn t021_flags_enum_vs_integer() {
    let p = proj();
    // SwitchMode.Value is enum; comparing to integer literal 1.
    assert!(codes(&p, "if (SwitchMode.Value eq 1) {\n}\n").contains(&TypeCode::T021));
}

#[test]
fn t021_no_flag_enum_vs_member() {
    let p = proj();
    assert!(
        !codes(&p, "if (SwitchMode.Value eq Switch State.On) {\n}\n").contains(&TypeCode::T021)
    );
}

#[test]
fn t021_no_flag_int_vs_int() {
    let p = proj();
    assert!(!codes(&p, "local iX = 1;\nif (iX eq 2) {\n}\n").contains(&TypeCode::T021));
}

#[test]
fn t030_flags_enum_member_into_wrong_enum() {
    let p = proj();
    // SwitchMode.Value is Switch State; assigning a Drive State member is a mismatch.
    assert!(codes(&p, "SwitchMode.Value = Drive State.Idle;\n").contains(&TypeCode::T030));
}

#[test]
fn t030_no_flag_same_enum() {
    let p = proj();
    assert!(!codes(&p, "SwitchMode.Value = Switch State.On;\n").contains(&TypeCode::T030));
}

#[test]
fn t030_no_flag_unknown_target() {
    let p = proj();
    // driveMode has no known type -> silent.
    assert!(!codes(&p, "driveMode = 1.0;\n").contains(&TypeCode::T030));
}

// NOTE (deviation from plan): the `LHS is (Member)` clause that the plan's
// Trigger 2 targeted is NOT valid syntax in the tree-sitter-m1 grammar — `is`
// always parses as an ERROR node, and the runner short-circuits on syntax
// errors, so the clause can never reach a rule. It also does not appear in the
// real EV-M1 corpus (every `is` there is inside a comment). T020 therefore fires
// only on the corpus-real typed-member-path idiom `<EnumType>.<Member>`.
