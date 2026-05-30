//! Enum data types declared in <DataTypes>.
pub type EnumId = usize;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnumType {
    pub name: String,
    pub members: Vec<(String, i64)>, // (name, ContainerOrder)
    pub default: Option<String>,
}
