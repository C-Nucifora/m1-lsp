//! Symbol model loaded from .m1prj / .m1cfg.
pub mod enums;
pub mod m1cfg;
pub mod m1prj;

use crate::types::ValueType;
pub use enums::{EnumId, EnumType};
use std::collections::HashMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SymbolKind {
    Channel,
    Parameter,
    Constant,
    Function,
    Method,
    Table,
    Group,
    Reference,
    /// An instance of a package-defined object class (a non-`BuiltIn.*`
    /// component, e.g. `MoTeC Input.Sensor`). Its class name is on
    /// [`Symbol::class`]; its members are the symbols whose path is prefixed by
    /// this object's path (see [`SymbolTable::immediate_children`]).
    Object,
    Other,
}

#[derive(Debug, Clone)]
pub struct Symbol {
    pub path: String,
    pub kind: SymbolKind,
    pub value_type: ValueType,
    pub unit: Option<String>,
    pub filename: Option<String>,
    /// Enum type this symbol's value belongs to, if known (set during .m1cfg
    /// back-resolution). When set, the typer reports `ValueType::Enum(_)`.
    pub enum_assoc: Option<EnumId>,
    /// For [`SymbolKind::Object`], the component's package class name
    /// (`Classname`, e.g. `"MoTeC Input.Sensor"`). `None` otherwise.
    pub class: Option<String>,
}

#[derive(Debug, Default)]
pub struct SymbolTable {
    symbols: Vec<Symbol>,
    by_path: HashMap<String, usize>,
    enums: Vec<EnumType>,
    /// member name -> enum type ids that declare a member of that name.
    member_index: HashMap<String, Vec<EnumId>>,
}

impl SymbolTable {
    pub fn push(&mut self, sym: Symbol) {
        let idx = self.symbols.len();
        self.by_path.insert(sym.path.clone(), idx);
        self.symbols.push(sym);
    }
    pub fn get(&self, path: &str) -> Option<&Symbol> {
        self.by_path.get(path).map(|&i| &self.symbols[i])
    }
    pub fn get_mut(&mut self, path: &str) -> Option<&mut Symbol> {
        self.by_path.get(path).map(|&i| &mut self.symbols[i])
    }
    pub fn len(&self) -> usize {
        self.symbols.len()
    }
    pub fn is_empty(&self) -> bool {
        self.symbols.is_empty()
    }
    pub fn iter(&self) -> impl Iterator<Item = &Symbol> {
        self.symbols.iter()
    }
    pub fn add_enum(&mut self, e: EnumType) -> EnumId {
        let id = self.enums.len();
        for (member, _order) in &e.members {
            self.member_index
                .entry(member.clone())
                .or_default()
                .push(id);
        }
        self.enums.push(e);
        id
    }
    pub fn enums(&self) -> &[EnumType] {
        &self.enums
    }
    /// The enum type whose *name* equals `name`, if any.
    pub fn enum_by_name(&self, name: &str) -> Option<EnumId> {
        self.enums.iter().position(|e| e.name == name)
    }
    /// Enum type ids that declare a member of this exact name.
    pub fn enums_with_member(&self, member: &str) -> &[EnumId] {
        self.member_index
            .get(member)
            .map(|v| v.as_slice())
            .unwrap_or(&[])
    }
    pub fn enum_has_member(&self, id: EnumId, member: &str) -> bool {
        self.enums
            .get(id)
            .map(|e| e.members.iter().any(|(m, _)| m == member))
            .unwrap_or(false)
    }
    pub fn enum_type(&self, id: EnumId) -> &EnumType {
        &self.enums[id]
    }
    /// Associate a symbol (by path) with an enum type id. No-op if absent.
    pub fn set_enum_assoc(&mut self, path: &str, id: EnumId) {
        if let Some(&i) = self.by_path.get(path) {
            self.symbols[i].enum_assoc = Some(id);
            self.symbols[i].value_type = ValueType::Enum(id);
        }
    }
    /// True if `path` exists as a symbol with a direct child of the given leaf
    /// name (e.g. a group `…Drive State` that contains `…Drive State.Value`).
    /// Used to recognise value-bearing enum/channel compounds, whose members
    /// expose built-in accessors that are not themselves stored symbols.
    pub fn has_child(&self, path: &str, leaf: &str) -> bool {
        self.by_path.contains_key(&format!("{path}.{leaf}"))
    }

    /// The immediate child symbols of `path` — symbols whose path is
    /// `"{path}.{leaf}"` where `leaf` is a single segment (segments are
    /// separated by `.`; a segment may contain spaces). Used to enumerate an
    /// object's members for hover/completion.
    pub fn immediate_children(&self, path: &str) -> Vec<&Symbol> {
        let prefix = format!("{path}.");
        self.symbols
            .iter()
            .filter(|s| {
                s.path
                    .strip_prefix(&prefix)
                    .is_some_and(|leaf| !leaf.contains('.'))
            })
            .collect()
    }
}
