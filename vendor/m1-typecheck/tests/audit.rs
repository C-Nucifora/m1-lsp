use m1_typecheck::diagnostics::TypeCode;
use m1_typecheck::project::Project;
use std::path::Path;

fn proj() -> Project {
    Project::load(&Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/naming.m1prj"))
        .unwrap()
}

#[test]
fn t050_flags_only_violators() {
    let p = proj();
    let msgs: Vec<String> = p
        .audit()
        .iter()
        .filter(|d| d.code == TypeCode::T050)
        .map(|d| d.inner.message.clone())
        .collect();
    let all = msgs.join("\n");
    // Violators present.
    assert!(
        all.contains("BrakePressure"),
        "channel UpperCamel should flag: {all}"
    );
    assert!(
        all.contains("brakeBias"),
        "parameter lowerCamel should flag"
    );
    assert!(all.contains("trackWidth"), "constant non-caps should flag");
    assert!(
        all.contains("CheckLimit"),
        "function UpperCamel should flag"
    );
    assert!(all.contains("bad state"), "enum with space should flag");
    // Conformers absent.
    assert!(
        !all.contains("`brakePressure`"),
        "conforming channel must not flag"
    );
    assert!(
        !all.contains("`BrakeBias`"),
        "conforming parameter must not flag"
    );
    assert!(!all.contains("`TRACKWIDTH`"));
    assert!(!all.contains("`checkLimit`"));
    assert!(!all.contains("`GoodState`"));
}
