use m1_typecheck::resolve::Scope;
use m1_typecheck::typer::type_of_source_expr;
use m1_typecheck::types::ValueType;
use std::collections::HashMap;

// Helper: type the single expression in `<expr>;` with the given locals.
fn ty(src_expr: &str, locals: &[(&str, ValueType)]) -> ValueType {
    let mut m = HashMap::new();
    for (k, v) in locals {
        m.insert(k.to_string(), *v);
    }
    let scope = Scope {
        locals: m,
        group: None,
        project: None,
    };
    type_of_source_expr(&format!("{src_expr};\n"), &scope)
}

#[test]
fn literals() {
    assert_eq!(ty("1", &[]), ValueType::Integer);
    assert_eq!(ty("1.0", &[]), ValueType::Float);
    assert_eq!(ty("0u", &[]), ValueType::Unsigned);
    assert_eq!(ty("true", &[]), ValueType::Boolean);
    assert_eq!(ty("\"hi\"", &[]), ValueType::String);
}

#[test]
fn local_and_arithmetic() {
    assert_eq!(
        ty("fGain", &[("fGain", ValueType::Float)]),
        ValueType::Float
    );
    assert_eq!(ty("1 + 2", &[]), ValueType::Integer);
    assert_eq!(
        ty("fGain * 2", &[("fGain", ValueType::Float)]),
        ValueType::Float
    );
}

#[test]
fn comparisons_are_boolean() {
    assert_eq!(ty("1 < 2", &[]), ValueType::Boolean);
    assert_eq!(ty("a eq b", &[]), ValueType::Boolean); // a,b opaque -> still Boolean
}

#[test]
fn calls_are_unknown() {
    assert_eq!(ty("Calculate.Min(1, 2)", &[]), ValueType::Unknown);
}
