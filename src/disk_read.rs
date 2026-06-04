//! Tolerant disk reads for project files that feed cross-file features.
//!
//! Cross-file references / rename / call-hierarchy read sibling `.m1scr` (and
//! the `.m1prj`) from disk for any file the client has not opened. M1 source is
//! frequently Windows-1252 (MoTeC's editor encoding) — a comment containing a
//! `°` (`0xB0`) is enough to make a plain UTF-8 `read_to_string` fail. When that
//! read was swallowed with `.ok()`, the file was *silently* dropped from the
//! result — most damagingly from a rename's edit set, leaving that file's
//! occurrences un-renamed with no warning (#125).
//!
//! [`read_disk`] decodes tolerantly via [`m1_workspace::read_text`] (UTF-8, then
//! Windows-1252 — it never fails on bad encoding, only on genuine IO errors) and
//! logs a warning to stderr on a real IO failure before returning `None`, so a
//! dropped file is at least visible rather than silent.
use std::path::Path;

/// Read `path` with tolerant (UTF-8 → Windows-1252) decoding. On a genuine IO
/// error (e.g. the file vanished) log a warning to stderr and return `None`;
/// encoding is never an error. Use for disk-sourced project files that feed
/// cross-file features, so a non-UTF-8 script is decoded and included.
pub fn read_disk(path: &Path) -> Option<String> {
    match m1_workspace::read_text(path) {
        Ok(text) => Some(text),
        Err(e) => {
            eprintln!(
                "m1-lsp: cannot read {} ({e}); excluding it from cross-file references/rename",
                path.display()
            );
            None
        }
    }
}
