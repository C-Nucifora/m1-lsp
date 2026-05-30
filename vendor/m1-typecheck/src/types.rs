//! The type lattice and literal/arithmetic typing rules.

pub type EnumId = usize;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ValueType {
    Boolean,
    Integer,
    Unsigned,
    Float,
    Enum(EnumId),
    String,
    Unknown,
}

impl ValueType {
    pub fn is_float(self) -> bool {
        matches!(self, ValueType::Float)
    }
    pub fn is_integral(self) -> bool {
        matches!(self, ValueType::Integer | ValueType::Unsigned)
    }
    pub fn is_known(self) -> bool {
        !matches!(self, ValueType::Unknown)
    }
    pub fn is_enum(self) -> bool {
        matches!(self, ValueType::Enum(_))
    }
}

/// Type of a numeric literal from its source text.
pub fn type_of_number_literal(text: &str) -> ValueType {
    let t = text.trim();
    let lower = t.to_ascii_lowercase();
    if lower.starts_with("0x") {
        return ValueType::Unsigned; // hex (optionally with trailing u)
    }
    if lower.ends_with('u') {
        return ValueType::Unsigned;
    }
    // Float if it has a decimal point or a (non-hex) exponent.
    if t.contains('.') || lower.contains('e') {
        return ValueType::Float;
    }
    ValueType::Integer
}

/// Result type of an arithmetic binary op over two operand types.
pub fn numeric_join(a: ValueType, b: ValueType) -> ValueType {
    use ValueType::*;
    match (a, b) {
        (Unknown, _) | (_, Unknown) => Unknown,
        (Float, x) | (x, Float) if x.is_float() || x.is_integral() => Float,
        (Unsigned, Unsigned) => Unsigned,
        (x, y) if x.is_integral() && y.is_integral() => Integer,
        _ => Unknown, // non-numeric operands: not our concern here
    }
}

/// Infer a local's type from its Hungarian prefix (prefix letter + UpperCase).
pub fn type_from_hungarian(name: &str) -> Option<ValueType> {
    let mut chars = name.chars();
    let first = chars.next()?;
    let second = chars.next()?;
    if !second.is_ascii_uppercase() {
        return None;
    }
    match first {
        'b' => Some(ValueType::Boolean),
        'u' => Some(ValueType::Unsigned),
        'i' => Some(ValueType::Integer),
        'f' => Some(ValueType::Float),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn number_literals() {
        assert_eq!(type_of_number_literal("1"), ValueType::Integer);
        assert_eq!(type_of_number_literal("42"), ValueType::Integer);
        assert_eq!(type_of_number_literal("0u"), ValueType::Unsigned);
        assert_eq!(type_of_number_literal("0xFF"), ValueType::Unsigned);
        assert_eq!(type_of_number_literal("0x1Fu"), ValueType::Unsigned);
        assert_eq!(type_of_number_literal("1.0"), ValueType::Float);
        assert_eq!(type_of_number_literal("1e3"), ValueType::Float);
        assert_eq!(type_of_number_literal("2.5e-3"), ValueType::Float);
    }

    #[test]
    fn joins() {
        use ValueType::*;
        assert_eq!(numeric_join(Integer, Float), Float);
        assert_eq!(numeric_join(Float, Unsigned), Float);
        assert_eq!(numeric_join(Unsigned, Unsigned), Unsigned);
        assert_eq!(numeric_join(Integer, Unsigned), Integer);
        assert_eq!(numeric_join(Unknown, Float), Unknown);
        assert_eq!(numeric_join(Boolean, Integer), Unknown);
    }

    #[test]
    fn prefix_types() {
        assert_eq!(type_from_hungarian("bReady"), Some(ValueType::Boolean));
        assert_eq!(type_from_hungarian("uCount"), Some(ValueType::Unsigned));
        assert_eq!(type_from_hungarian("iDelta"), Some(ValueType::Integer));
        assert_eq!(type_from_hungarian("fGain"), Some(ValueType::Float));
        assert_eq!(type_from_hungarian("count"), None); // no prefix
        assert_eq!(type_from_hungarian("freq"), None); // 'f' not followed by upper
        assert_eq!(type_from_hungarian("x"), None);
    }
}
