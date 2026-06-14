//! Shared rendering of intrinsics signatures, so the same `Overload` displays
//! identically wherever the LSP surfaces it.
//!
//! `hover` shows the signature in a markdown code block and `signatureHelp`
//! builds it as the `SignatureInformation` label — both want the canonical
//! `Name(param: Type, …) -> Return` form. Keeping the one formatter here means a
//! future tweak to how signatures display (parameter separator, return-arrow
//! style, …) can't silently diverge between the hover popup and the
//! signature-help popup for the same call.
//!
//! Note: `completion`'s `signature_detail` is *deliberately* a different format
//! (parameter names only, no types, no path) and is intentionally not unified
//! here.
use m1_typecheck::intrinsics::Overload;

/// The canonical `path(param: Type, …) -> Return` label for one overload.
pub(crate) fn signature_label(path: &str, ov: &Overload) -> String {
    let params: Vec<String> = ov
        .params
        .iter()
        .map(|p| format!("{}: {}", p.name, p.ty))
        .collect();
    format!("{path}({}) -> {}", params.join(", "), ov.returns)
}

#[cfg(test)]
mod tests {
    use super::*;
    use m1_typecheck::intrinsics::Overload;

    /// Build an `Overload` from JSON so the test does not depend on the exact
    /// (non-`Default`) field set of the struct.
    fn ov(json: &str) -> Overload {
        serde_json::from_str(json).unwrap()
    }

    #[test]
    fn renders_path_params_and_return() {
        let o = ov(r#"{
            "name": "Max",
            "returns": "Float",
            "params": [
                {"name": "a", "type": "Float"},
                {"name": "b", "type": "Float"}
            ]
        }"#);
        assert_eq!(
            signature_label("Calculate.Max", &o),
            "Calculate.Max(a: Float, b: Float) -> Float"
        );
    }

    #[test]
    fn renders_zero_params() {
        let o = ov(r#"{"name": "Now", "returns": "Float", "params": []}"#);
        assert_eq!(signature_label("System.Now", &o), "System.Now() -> Float");
    }
}
