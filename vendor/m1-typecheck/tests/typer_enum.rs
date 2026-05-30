use m1_typecheck::project::Project;
use m1_typecheck::resolve::Scope;
use m1_typecheck::typer::type_of_source_expr;
use m1_typecheck::types::ValueType;
use std::collections::HashMap;
use std::path::Path;

fn proj() -> Project {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    Project::load(&dir.join("enums.m1prj"))
        .unwrap()
        .with_config(&dir.join("enums.m1cfg"))
        .unwrap()
}

fn ty(p: &Project, expr: &str) -> ValueType {
    let scope = Scope {
        locals: HashMap::new(),
        group: Some("Root.Foo".into()),
        project: Some(p),
    };
    type_of_source_expr(&format!("{expr};\n"), &scope)
}

#[test]
fn typed_member_path_is_enum() {
    let p = proj();
    let drive_id = p.symbols().enum_by_name("Drive State").unwrap();
    assert_eq!(
        ty(&p, "Drive State.Latching Fault"),
        ValueType::Enum(drive_id)
    );
}

#[test]
fn non_member_typed_path_is_not_that_enum() {
    let p = proj();
    // "Drive State.Nope" has the enum-type head but no such member -> not Enum.
    assert!(!ty(&p, "Drive State.Nope").is_enum());
}

#[test]
fn associated_parameter_is_enum() {
    let p = proj();
    let switch_id = p.symbols().enum_by_name("Switch State").unwrap();
    // Group-relative "SwitchMode.Value" resolves to the associated parameter.
    assert_eq!(ty(&p, "SwitchMode.Value"), ValueType::Enum(switch_id));
}
