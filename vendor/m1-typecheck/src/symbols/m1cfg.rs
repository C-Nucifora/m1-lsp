//! Parse parameters.m1cfg and augment matching symbols with value type + unit.
use super::SymbolTable;
use crate::types::ValueType;

#[derive(Debug)]
pub enum ConfigError {
    Xml(roxmltree::Error),
}
impl std::fmt::Display for ConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConfigError::Xml(e) => write!(f, "invalid .m1cfg XML: {e}"),
        }
    }
}
impl std::error::Error for ConfigError {}

pub fn cell_type(t: &str) -> ValueType {
    match t {
        "f32" | "f64" => ValueType::Float,
        "u8" | "u16" | "u32" | "u64" => ValueType::Unsigned,
        "s8" | "s16" | "s32" | "s64" => ValueType::Integer,
        "bool" => ValueType::Boolean,
        "enum" => ValueType::Unknown, // enum association deferred to v2
        _ => ValueType::Unknown,
    }
}

pub fn augment(table: &mut SymbolTable, xml: &str) -> Result<(), ConfigError> {
    let doc = roxmltree::Document::parse(xml).map_err(ConfigError::Xml)?;
    for param in doc.descendants().filter(|n| n.has_tag_name("Parameter")) {
        let Some(name) = param.attribute("Name") else {
            continue;
        };
        let Some(cell) = param.children().find(|c| c.has_tag_name("Cell")) else {
            continue;
        };
        let type_attr = cell.attribute("Type").unwrap_or("");
        let unit = cell.attribute("Unit").map(str::to_string);

        if type_attr == "enum" {
            // Back-resolve the cell's member value to a *unique* enum type.
            let member = cell.text().map(str::trim).unwrap_or("");
            let candidates = table.enums_with_member(member);
            if candidates.len() == 1 {
                let id = candidates[0];
                table.set_enum_assoc(name, id);
            }
            // zero or many -> leave Unknown (silent), but still record the unit.
            if let Some(sym) = table.get_mut(name) {
                if sym.unit.is_none() {
                    sym.unit = unit;
                }
            }
            continue;
        }

        let vt = cell_type(type_attr);
        if let Some(sym) = table.get_mut(name) {
            sym.value_type = vt;
            sym.unit = unit;
        }
    }
    Ok(())
}
