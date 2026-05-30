use m1_typecheck::project::Project;
use m1_typecheck::resolve::{resolve, Resolution, Scope};
use m1_typecheck::types::ValueType;
use std::collections::HashMap;
use std::path::Path;

fn proj() -> Project {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    Project::load(&dir.join("mini.m1prj")).unwrap()
}

#[test]
fn resolves_absolute() {
    let p = proj();
    let scope = Scope {
        locals: HashMap::new(),
        group: None,
        project: Some(&p),
    };
    assert!(matches!(
        resolve("Root.Foo.Speed", &scope),
        Resolution::Symbol(_)
    ));
}

#[test]
fn resolves_group_relative() {
    let p = proj();
    let scope = Scope {
        locals: HashMap::new(),
        group: Some("Root.Foo".into()),
        project: Some(&p),
    };
    // "Speed" relative to Root.Foo -> Root.Foo.Speed
    assert!(matches!(resolve("Speed", &scope), Resolution::Symbol(_)));
}

#[test]
fn resolves_local() {
    let p = proj();
    let mut locals = HashMap::new();
    locals.insert("fGain".to_string(), ValueType::Float);
    let scope = Scope {
        locals,
        group: None,
        project: Some(&p),
    };
    assert!(matches!(
        resolve("fGain", &scope),
        Resolution::Local(ValueType::Float)
    ));
}

#[test]
fn opaque_root_passes_through() {
    let p = proj();
    let scope = Scope {
        locals: HashMap::new(),
        group: Some("Root.Foo".into()),
        project: Some(&p),
    };
    assert!(matches!(
        resolve("Calculate.Min", &scope),
        Resolution::Opaque
    ));
}

#[test]
fn unresolved_when_project_rooted_but_absent() {
    let p = proj();
    let scope = Scope {
        locals: HashMap::new(),
        group: Some("Root.Foo".into()),
        project: Some(&p),
    };
    assert!(matches!(
        resolve("Root.Foo.Missing", &scope),
        Resolution::Unresolved
    ));
}

#[test]
fn accessor_on_resolved_channel_is_opaque() {
    let p = proj();
    let scope = Scope {
        locals: HashMap::new(),
        group: Some("Root.Foo".into()),
        project: Some(&p),
    };
    // `Speed` is a channel; `Speed.AsInteger` is a built-in accessor, not a miss.
    assert!(matches!(
        resolve("Speed.AsInteger", &scope),
        Resolution::Opaque
    ));
    // Absolute form too.
    assert!(matches!(
        resolve("Root.Foo.Speed.AsInteger", &scope),
        Resolution::Opaque
    ));
}

#[test]
fn unknown_root_is_opaque_not_unresolved() {
    let p = proj();
    let scope = Scope {
        locals: HashMap::new(),
        group: Some("Root.Foo".into()),
        project: Some(&p),
    };
    // "Bar" is neither a project group root nor opaque-listed; treat as opaque
    // (conservative: only project-rooted misses are flagged).
    assert!(matches!(
        resolve("Bar.Whatever", &scope),
        Resolution::Opaque
    ));
}
