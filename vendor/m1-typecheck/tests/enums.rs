use m1_typecheck::symbols::m1prj;
use std::path::Path;

fn fixture(name: &str) -> String {
    std::fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures")
            .join(name),
    )
    .unwrap()
}

#[test]
fn enum_by_name_and_members() {
    let parsed = m1prj::parse(&fixture("enums.m1prj")).unwrap();
    let t = &parsed.table;
    let id = t.enum_by_name("Drive State").expect("Drive State enum");
    assert!(t.enum_has_member(id, "Latching Fault"));
    assert!(!t.enum_has_member(id, "Nope"));
    assert!(t.enum_by_name("No Such Type").is_none());
}

#[test]
fn member_index_unique_and_ambiguous() {
    let parsed = m1prj::parse(&fixture("enums.m1prj")).unwrap();
    let t = &parsed.table;
    // "On" is only in Switch State -> unique.
    assert_eq!(t.enums_with_member("On").len(), 1);
    // "Off" is in both Switch State and Other State -> ambiguous.
    assert_eq!(t.enums_with_member("Off").len(), 2);
    // unknown member -> empty.
    assert!(t.enums_with_member("Zzz").is_empty());
}

#[test]
fn m1cfg_enum_cell_associates_unique_member() {
    use m1_typecheck::symbols::m1cfg;
    use m1_typecheck::types::ValueType;
    let mut parsed = m1prj::parse(&fixture("enums.m1prj")).unwrap();
    m1cfg::augment(&mut parsed.table, &fixture("enums.m1cfg")).unwrap();
    let sym = parsed.table.get("Root.Foo.SwitchMode.Value").unwrap();
    let switch_id = parsed.table.enum_by_name("Switch State").unwrap();
    assert_eq!(sym.enum_assoc, Some(switch_id));
    assert_eq!(sym.value_type, ValueType::Enum(switch_id));
}
