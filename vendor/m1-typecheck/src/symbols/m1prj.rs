//! Parse Project.m1prj into a SymbolTable, enum types, and a file->group map.
use super::{EnumType, Symbol, SymbolKind, SymbolTable};
use crate::types::ValueType;
use std::collections::{HashMap, HashSet};

pub struct Parsed {
    pub table: SymbolTable,
    pub file_to_group: HashMap<String, String>,
    pub module_roots: HashSet<String>,
}

#[derive(Debug)]
pub enum ParseError {
    Xml(roxmltree::Error),
}
impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ParseError::Xml(e) => write!(f, "invalid .m1prj XML: {e}"),
        }
    }
}
impl std::error::Error for ParseError {}

pub fn classify(classname: &str) -> SymbolKind {
    use SymbolKind::*;
    if classname.starts_with("BuiltIn.Channel") || classname == "BuiltIn.CalibrationChannel" {
        Channel
    } else if classname.starts_with("BuiltIn.Parameter")
        || classname == "BuiltIn.CalibrationParameter"
        || classname == "BuiltIn.IOResourceParameter"
    {
        Parameter
    } else if classname.starts_with("BuiltIn.Constant") || classname == "BuiltIn.IOResourceConstant"
    {
        Constant
    } else if classname.starts_with("BuiltIn.FuncUser")
        || classname.starts_with("BuiltIn.CalFuncUser")
        || classname.starts_with("BuiltIn.FuncGenerated")
    {
        Function
    } else if classname == "BuiltIn.MethodUser" || classname == "BuiltIn.EventKernel" {
        Method
    } else if classname.starts_with("BuiltIn.Table") || classname == "BuiltIn.CalibrationTable" {
        Table
    } else if classname == "BuiltIn.GroupCompound" {
        Group
    } else if classname == "BuiltIn.Reference" {
        Reference
    } else if !classname.starts_with("BuiltIn.") && !classname.is_empty() {
        // Any non-`BuiltIn.*` component is an instance of a package-defined
        // object class (e.g. "MoTeC Input.Sensor", "MoTeC Output.Switched
        // Output", "_IOMethod.abs"). Its members are separate components whose
        // path is prefixed by this object's path.
        Object
    } else {
        Other
    }
}

/// Enclosing group path for a fully-qualified symbol path (drop the last segment,
/// honouring space-containing segments — segments are split on '.').
fn parent_group(path: &str) -> Option<String> {
    let idx = path.rfind('.')?;
    Some(path[..idx].to_string())
}

pub fn parse(xml: &str) -> Result<Parsed, ParseError> {
    let doc = roxmltree::Document::parse(xml).map_err(ParseError::Xml)?;
    let mut table = SymbolTable::default();
    let mut file_to_group = HashMap::new();
    let mut module_roots = HashSet::new();

    for node in doc.descendants() {
        match node.tag_name().name() {
            "File" if node.parent().map(|p| p.tag_name().name()) == Some("SelectedModuleSets") => {
                if let Some(name) = node.attribute("Name") {
                    module_roots.insert(name.to_string());
                }
            }
            "Type" if node.attribute("Storage") == Some("enum") => {
                let name = node.attribute("Name").unwrap_or("").to_string();
                let default = node.attribute("Default").map(str::to_string);
                let members = node
                    .children()
                    .filter(|c| c.has_tag_name("Enum"))
                    .map(|c| {
                        (
                            c.attribute("Name").unwrap_or("").to_string(),
                            c.attribute("ContainerOrder")
                                .and_then(|s| s.parse().ok())
                                .unwrap_or(0),
                        )
                    })
                    .collect();
                table.add_enum(EnumType {
                    name,
                    members,
                    default,
                });
            }
            "Component" => {
                let (Some(classname), Some(name)) =
                    (node.attribute("Classname"), node.attribute("Name"))
                else {
                    continue;
                };
                let kind = classify(classname);
                let filename = node.attribute("Filename").map(str::to_string);
                if matches!(kind, SymbolKind::Function | SymbolKind::Method) {
                    if let (Some(f), Some(g)) = (filename.clone(), parent_group(name)) {
                        file_to_group.insert(f, g);
                    }
                }
                // Constant value type from Props/@Value literal, if any.
                let value_type = if kind == SymbolKind::Constant {
                    node.children()
                        .find(|c| c.has_tag_name("Props"))
                        .and_then(|p| p.attribute("Value"))
                        .map(constant_value_type)
                        .unwrap_or(ValueType::Unknown)
                } else {
                    ValueType::Unknown
                };
                let class = if kind == SymbolKind::Object {
                    Some(classname.to_string())
                } else {
                    None
                };
                table.push(Symbol {
                    path: name.to_string(),
                    kind,
                    value_type,
                    unit: None,
                    filename,
                    enum_assoc: None,
                    class,
                });
            }
            _ => {}
        }
    }
    Ok(Parsed {
        table,
        file_to_group,
        module_roots,
    })
}

fn constant_value_type(value: &str) -> ValueType {
    let v = value.trim();
    if v.eq_ignore_ascii_case("true") || v.eq_ignore_ascii_case("false") {
        ValueType::Boolean
    } else if v.parse::<i64>().is_ok() {
        ValueType::Integer
    } else if v.parse::<f64>().is_ok() {
        ValueType::Float
    } else {
        ValueType::Unknown // e.g. an enum member name like "CAN Bus 2"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_package_objects_and_keeps_builtins() {
        assert_eq!(classify("MoTeC Input.Sensor"), SymbolKind::Object);
        assert_eq!(classify("MoTeC Output.Switched Output"), SymbolKind::Object);
        assert_eq!(classify("_IOMethod.abs"), SymbolKind::Object);
        // BuiltIn.* primitives are unchanged.
        assert_eq!(classify("BuiltIn.Channel"), SymbolKind::Channel);
        assert_eq!(classify("BuiltIn.MethodUser"), SymbolKind::Method);
        // An unhandled BuiltIn.* stays Other, not Object.
        assert_eq!(classify("BuiltIn.IOCharacteristic"), SymbolKind::Other);
    }

    #[test]
    fn parses_object_with_class_and_members() {
        let xml = r#"<?xml version="1.0"?>
<Project>
  <Component Classname="MoTeC Input.Sensor" Name="Root.Throttle"/>
  <Component Classname="BuiltIn.Channel" Name="Root.Throttle.Value"/>
  <Component Classname="BuiltIn.MethodUser" Name="Root.Throttle.Calculation" Filename="Calc.m1scr"/>
  <Component Classname="BuiltIn.Channel" Name="Root.Throttle.Diagnostic.Value"/>
</Project>"#;
        let parsed = parse(xml).unwrap();
        let obj = parsed.table.get("Root.Throttle").expect("object symbol");
        assert_eq!(obj.kind, SymbolKind::Object);
        assert_eq!(obj.class.as_deref(), Some("MoTeC Input.Sensor"));

        // Immediate members are Value and Calculation (not the nested Diagnostic.Value).
        let mut members: Vec<&str> = parsed
            .table
            .immediate_children("Root.Throttle")
            .iter()
            .map(|s| s.path.as_str())
            .collect();
        members.sort();
        assert_eq!(
            members,
            ["Root.Throttle.Calculation", "Root.Throttle.Value"]
        );
    }
}
