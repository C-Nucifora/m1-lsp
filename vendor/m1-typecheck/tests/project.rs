use m1_typecheck::project::Project;
use std::path::Path;

fn proj() -> Project {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    Project::load(&dir.join("mini.m1prj"))
        .unwrap()
        .with_config(&dir.join("mini.m1cfg"))
        .unwrap()
}

#[test]
fn loads_table_and_config() {
    let p = proj();
    assert!(p.symbols().get("Root.Foo.Speed").is_some());
    assert_eq!(
        p.symbols()
            .get("Root.Foo.Gain.Value")
            .unwrap()
            .unit
            .as_deref(),
        Some("ratio")
    );
}

#[test]
fn opaque_roots_include_modules_and_builtins() {
    let p = proj();
    assert!(p.is_opaque_root("MoTeC Comms")); // from module sets
    assert!(p.is_opaque_root("Calculate")); // static builtin
    assert!(p.is_opaque_root("This"));
    assert!(!p.is_opaque_root("Root")); // Root is a project group, not opaque
}

#[test]
fn group_for_script() {
    let p = proj();
    assert_eq!(
        p.group_for_script("Foo Update.m1scr").as_deref(),
        Some("Root.Foo")
    );
    assert_eq!(p.group_for_script("Unknown.m1scr"), None);
}
