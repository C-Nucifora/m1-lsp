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

    fn format_range(
        &self,
        src: &str,
        start_line: u32,
        end_line: u32,
    ) -> Option<(u32, u32, String)> {
        let opts = m1_fmt::FormatOptions::default();
        match m1_fmt::format_range(src, start_line as usize, end_line as usize, &opts) {
            Ok(Some(r)) if r.changed => Some((r.start_line as u32, r.end_line as u32, r.output)),
            _ => None, // nothing overlaps, unchanged, or syntax errors
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

    #[test]
    fn range_formats_only_the_targeted_statement() {
        // line 1 (0-based) is the messy one; the others stay as-is.
        let src = "local a=1;\nlocal b   =   2;\nlocal c=3;\n";
        let (start, end, text) = M1Fmt.format_range(src, 1, 1).expect("range change");
        assert_eq!((start, end), (1, 1));
        assert_eq!(text, "local b = 2;\n");
    }

    #[test]
    fn range_already_clean_is_none() {
        assert!(
            M1Fmt
                .format_range("local a = 1;\nlocal b = 2;\n", 0, 0)
                .is_none()
        );
    }
}
