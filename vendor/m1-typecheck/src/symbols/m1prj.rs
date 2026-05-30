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
                table.push(Symbol {
                    path: name.to_string(),
                    kind,
                    value_type,
                    unit: None,
                    filename,
                    enum_assoc: None,
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
