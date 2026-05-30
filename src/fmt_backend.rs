//! Real formatter backed by m1-fmt.
use crate::format::Formatter;

pub struct M1Fmt;

impl Formatter for M1Fmt {
    fn format(&self, src: &str) -> Option<String> {
        match m1_fmt::format_str(src) {
            Ok(result) if result.changed => Some(result.output),
            _ => None, // unchanged, or syntax-error pass-through, or error
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reformats_spacing() {
        let out = M1Fmt.format("x=1+2;\n");
        assert_eq!(out.as_deref(), Some("x = 1 + 2;\n"));
    }

    #[test]
    fn already_formatted_is_none() {
        assert!(M1Fmt.format("x = 1 + 2;\n").is_none());
    }

    #[test]
    fn syntax_error_is_none() {
        assert!(M1Fmt.format("local <Integer> = 1;\n").is_none());
    }
}
