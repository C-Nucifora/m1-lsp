use m1_typecheck::symbols::{m1prj, SymbolKind};
use std::path::Path;

fn fixture() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/mini.m1prj")
}

#[test]
fn parses_symbols_and_kinds() {
    let parsed = m1prj::parse(&std::fs::read_to_string(fixture()).unwrap()).unwrap();
    let table = &parsed.table;
    assert_eq!(
        table.get("Root.Foo.Speed").unwrap().kind,
        SymbolKind::Channel
    );
    assert_eq!(
        table.get("Root.Foo.Gain.Value").unwrap().kind,
        SymbolKind::Parameter
    );
    assert_eq!(
        table.get("Root.Foo.Limit").unwrap().kind,
        SymbolKind::Constant
    );
    assert_eq!(
        table.get("Root.Foo.Update").unwrap().kind,
        SymbolKind::Function
    );
    assert_eq!(
        table.get("Root.Foo.Startup").unwrap().kind,
        SymbolKind::Method
    );
    assert_eq!(table.get("Root.Foo").unwrap().kind, SymbolKind::Group);
}

#[test]
fn parses_enum_types() {
    let parsed = m1prj::parse(&std::fs::read_to_string(fixture()).unwrap()).unwrap();
    let e = parsed
        .table
        .enums()
        .iter()
        .find(|e| e.name == "Switch State")
        .unwrap();
    assert_eq!(e.default.as_deref(), Some("Off"));
    assert_eq!(
        e.members,
        vec![("Off".to_string(), 0), ("On".to_string(), 1)]
    );
}

#[test]
fn maps_filename_to_enclosing_group() {
    let parsed = m1prj::parse(&std::fs::read_to_string(fixture()).unwrap()).unwrap();
    assert_eq!(
        parsed
            .file_to_group
            .get("Foo Update.m1scr")
            .map(String::as_str),
        Some("Root.Foo")
    );
}

#[test]
fn collects_module_set_roots() {
    let parsed = m1prj::parse(&std::fs::read_to_string(fixture()).unwrap()).unwrap();
    assert!(parsed.module_roots.contains("MoTeC Comms"));
}

#[test]
fn m1cfg_augments_parameter_types() {
    use m1_typecheck::symbols::{m1cfg, m1prj};
    use m1_typecheck::types::ValueType;
    let mut parsed = m1prj::parse(&std::fs::read_to_string(fixture()).unwrap()).unwrap();
    let cfg = std::fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/mini.m1cfg"),
    )
    .unwrap();
    m1cfg::augment(&mut parsed.table, &cfg).unwrap();
    let gain = parsed.table.get("Root.Foo.Gain.Value").unwrap();
    assert_eq!(gain.value_type, ValueType::Float);
    assert_eq!(gain.unit.as_deref(), Some("ratio"));
}
