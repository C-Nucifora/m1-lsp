//! Real formatter backed by m1-fmt.
use crate::format::Formatter;
use m1_fmt::FormatOptions;
use std::sync::RwLock;

pub struct M1Fmt {
    /// Behind a lock so the backend can swap in `[format]` options resolved from
    /// `m1-tools.toml` (line width, blank lines) at runtime.
    opts: RwLock<FormatOptions>,
}

impl M1Fmt {
    pub fn new() -> Self {
        Self {
            opts: RwLock::new(FormatOptions::default()),
        }
    }
}

impl Default for M1Fmt {
    fn default() -> Self {
        Self::new()
    }
}

/// Whether `src` nests deeper than is safe to format.
///
/// m1-fmt's pretty-printer recurses over expression nesting, so a pathologically
/// deep document overflows the stack and aborts the whole language server — an
/// uncatchable crash that would take down language support for the entire
/// workspace. The server must not hand such input to the formatter; declining to
/// format it (no edit) is the safe response to adversarial input. Computed with
/// m1-core's iterative `max_depth`, so the guard itself never overflows.
fn too_deep_to_format(src: &str) -> bool {
    m1_core::parse(src).root().max_depth() > m1_core::MAX_RECURSION_DEPTH
}

impl Formatter for M1Fmt {
    fn format(&self, src: &str) -> Option<String> {
        if too_deep_to_format(src) {
            return None;
        }
        let opts = self.opts.read().unwrap();
        match m1_fmt::format_str_with(src, &opts) {
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
        if too_deep_to_format(src) {
            return None;
        }
        let opts = self.opts.read().unwrap();
        match m1_fmt::format_range(src, start_line as usize, end_line as usize, &opts) {
            Ok(Some(r)) if r.changed => Some((r.start_line as u32, r.end_line as u32, r.output)),
            _ => None, // nothing overlaps, unchanged, or syntax errors
        }
    }

    fn set_format_options(&self, opts: &FormatOptions) {
        *self.opts.write().unwrap() = opts.clone();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reformats_spacing() {
        let out = M1Fmt::new().format("x=1+2;\n");
        assert_eq!(out.as_deref(), Some("x = 1 + 2;\n"));
    }

    #[test]
    fn already_formatted_is_none() {
        assert!(M1Fmt::new().format("x = 1 + 2;\n").is_none());
    }

    #[test]
    fn syntax_error_is_none() {
        assert!(M1Fmt::new().format("local <Integer> = 1;\n").is_none());
    }

    #[test]
    fn range_formats_only_the_targeted_statement() {
        // line 1 (0-based) is the messy one; the others stay as-is.
        let src = "local a=1;\nlocal b   =   2;\nlocal c=3;\n";
        let (start, end, text) = M1Fmt::new().format_range(src, 1, 1).expect("range change");
        assert_eq!((start, end), (1, 1));
        assert_eq!(text, "local b = 2;\n");
    }

    #[test]
    fn range_already_clean_is_none() {
        assert!(
            M1Fmt::new()
                .format_range("local a = 1;\nlocal b = 2;\n", 0, 0)
                .is_none()
        );
    }

    #[test]
    fn pathologically_deep_input_is_declined_not_crashed() {
        // A 50k-deep expression overflows m1-fmt's recursive printer; the backend
        // must decline to format it (None) rather than abort the server.
        let src = format!("x = {}1{};\n", "(".repeat(50_000), ")".repeat(50_000));
        let f = M1Fmt::new();
        assert!(f.format(&src).is_none());
        assert!(f.format_range(&src, 0, 0).is_none());
    }
}
