//! Rendering for built-in library objects, library functions, and object
//! accessor methods (`Calculate.Max`, `.AsInteger()`), drawn from the
//! help-capture intrinsics catalogue. The seam consumed by
//! [`super::hover_with_eval`] for the `BuiltinObject` / `BuiltinFn` resolutions
//! and the trailing-accessor fallback.
use crate::features::intrinsics_render::signature_label;
use m1_typecheck::intrinsics::Overload;

/// `path(p1: T1, p2: T2) -> Ret` for one overload signature. Shares the single
/// renderer with `signatureHelp` so the two popups can't drift (see
/// [`crate::features::intrinsics_render`]).
fn signature(path: &str, ov: &Overload) -> String {
    signature_label(path, ov)
}

pub(super) fn builtin_object_markdown(name: &str) -> String {
    let doc = m1_typecheck::intrinsics::get()
        .library_object(name)
        .map(|o| o.doc.as_str())
        .unwrap_or("");
    format!("**{name}** `library object`\n\n{doc}")
}

/// Hover for a built-in object *method* accessor (`.AsInteger()`, `.AsString()`,
/// `.Set()`, `.Lookup()`, …) called on a project object — the methods the M1
/// manual documents on every channel/enumerated object. Distinct from a library
/// *function* (`Calculate.Max`): a method is bound to the object on its left.
pub(super) fn object_method_markdown(name: &str, overloads: &[&Overload]) -> String {
    let mut s = format!("**{name}** `method`\n\n");
    for ov in overloads {
        s.push_str(&format!("```\n{}\n```\n", signature(name, ov)));
    }
    if let Some(first) = overloads.first()
        && !first.doc.is_empty()
    {
        s.push_str(&format!("\n{}\n", first.doc));
    }
    s
}

pub(super) fn builtin_fn_markdown(path: &str, overloads: &[&Overload]) -> String {
    let mut s = format!("**{path}** `library function`\n\n");
    for ov in overloads {
        s.push_str(&format!("```\n{}\n```\n", signature(path, ov)));
    }
    if let Some(first) = overloads.first() {
        if !first.doc.is_empty() {
            s.push_str(&format!("\n{}\n", first.doc));
        }
        if first.stateful {
            s.push_str(
                "\n⚠ **stateful** — call it on every execution; never inside an `if`/`when` or a comparison.",
            );
        }
        if first.deprecated {
            s.push_str("\n⚠ **deprecated**");
        }
        if first.calibration_only {
            s.push_str(
                "\n⚠ **calibration-only** — usable only in M1 Tune calibration methods, not in ECU `.m1scr` scripts.",
            );
        }
    }
    s
}
