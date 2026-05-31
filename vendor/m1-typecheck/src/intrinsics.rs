//! The M1 built-in intrinsic library: the firmware global objects
//! (`Calculate`, `CanComms`, …), their method overloads, the integrated-only
//! object names, the language tables (keywords/operators/unsupported), and the
//! data-driven diagnostic rules.
//!
//! The data is `m1-intrinsics.json`, vendored from an M1 Build firmware export
//! (see `docs/OBJECTS.md` and the EV-M1 `M1-docs/M1-Intrinsics-LSP.md` spec). It
//! is the single source of truth — nothing here hard-codes symbol names.
use serde::Deserialize;
use std::collections::HashMap;
use std::sync::OnceLock;

#[derive(Debug, Deserialize)]
pub struct Param {
    pub name: String,
    #[serde(rename = "type")]
    pub ty: String,
    #[serde(default)]
    pub doc: String,
}

/// One callable signature. A function with several signatures appears as several
/// `Overload`s with the same `name` (overloading is modelled by repetition).
#[derive(Debug, Deserialize)]
pub struct Overload {
    pub name: String,
    pub returns: String,
    #[serde(default)]
    pub params: Vec<Param>,
    #[serde(default)]
    pub doc: String,
    /// Stateful (purple-icon) functions must be called every execution, never
    /// conditionally or inside a comparison (diagnostic `stateful-conditional`).
    #[serde(default)]
    pub stateful: bool,
    #[serde(default)]
    pub deprecated: bool,
}

#[derive(Debug, Deserialize)]
pub struct LibraryObject {
    #[serde(default)]
    pub doc: String,
    #[serde(default)]
    pub functions: Vec<Overload>,
}

#[derive(Debug, Deserialize, Default)]
pub struct Language {
    /// role -> keywords (control / declaration / logical / comparison / literal / scope).
    #[serde(default)]
    pub keywords: HashMap<String, Vec<String>>,
    #[serde(default, rename = "scopeAnchors")]
    pub scope_anchors: HashMap<String, String>,
    #[serde(default)]
    pub operators: HashMap<String, String>,
    /// C token -> the M1 replacement message (diagnostic `unsupported-c-token`).
    #[serde(default)]
    pub unsupported: HashMap<String, String>,
}

#[derive(Debug, Deserialize)]
pub struct DiagRule {
    pub id: String,
    pub severity: String,
    pub rule: String,
    #[serde(default)]
    pub source: String,
}

#[derive(Debug, Deserialize)]
pub struct Intrinsics {
    pub version: u32,
    #[serde(default, rename = "dataTypes")]
    pub data_types: Vec<String>,
    /// object name -> { doc, functions } (the 13 firmware library objects).
    #[serde(default)]
    pub library: HashMap<String, LibraryObject>,
    /// Methods available on project objects (AsInteger/Set/Validate/Lookup/…).
    #[serde(default, rename = "objectMethods")]
    pub object_methods: Vec<Overload>,
    /// Object names that exist in firmware but must not be called directly.
    #[serde(default, rename = "integratedOnly")]
    pub integrated_only: Vec<String>,
    #[serde(default)]
    pub language: Language,
    #[serde(default)]
    pub diagnostics: Vec<DiagRule>,
}

static INTRINSICS_JSON: &str = include_str!("../assets/m1-intrinsics.json");
static INTRINSICS: OnceLock<Intrinsics> = OnceLock::new();

/// The vendored intrinsic library, parsed once.
pub fn get() -> &'static Intrinsics {
    INTRINSICS.get_or_init(|| {
        serde_json::from_str(INTRINSICS_JSON).expect("vendored m1-intrinsics.json must be valid")
    })
}

impl Intrinsics {
    /// `Some(&'static name)` if `name` is one of the 13 library objects.
    pub fn library_object_name(&'static self, name: &str) -> Option<&'static str> {
        self.library.get_key_value(name).map(|(k, _)| k.as_str())
    }
    pub fn library_object(&self, name: &str) -> Option<&LibraryObject> {
        self.library.get(name)
    }
    pub fn is_integrated_only(&self, name: &str) -> bool {
        self.integrated_only.iter().any(|n| n == name)
    }
    /// All overloads of `method` on library object `object`.
    pub fn library_overloads(&'static self, object: &str, method: &str) -> Vec<&'static Overload> {
        self.library
            .get(object)
            .map(|o| o.functions.iter().filter(|f| f.name == method).collect())
            .unwrap_or_default()
    }
    /// Overloads of a project-object method (AsInteger/Set/Lookup/…).
    pub fn object_method(&'static self, method: &str) -> Vec<&'static Overload> {
        self.object_methods
            .iter()
            .filter(|f| f.name == method)
            .collect()
    }
    /// If `token` is an unsupported C token, the M1 replacement message.
    pub fn unsupported_c_token(&self, token: &str) -> Option<&str> {
        self.language.unsupported.get(token).map(String::as_str)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loads_and_has_the_library() {
        let i = get();
        assert_eq!(i.library.len(), 13, "13 firmware library objects");
        let total: usize = i.library.values().map(|o| o.functions.len()).sum();
        assert_eq!(total, 128, "128 library overloads");
        assert!(i.library_object("Calculate").is_some());
        assert!(i.library_object("CanComms").is_some());
        assert!(i.library_object("NotAnObject").is_none());
    }

    #[test]
    fn looks_up_overloads_and_stateful() {
        let i = get();
        let max = i.library_overloads("Calculate", "Max");
        assert!(!max.is_empty(), "Calculate.Max exists");
        assert_eq!(max[0].returns, "Integer|FloatingPoint");
        // Delay.Rising is a stateful (purple) function.
        let rising = i.library_overloads("Delay", "Rising");
        assert!(
            !rising.is_empty() && rising[0].stateful,
            "Delay.Rising is stateful"
        );
        // Calculate.Max is not stateful.
        assert!(!max[0].stateful);
    }

    #[test]
    fn integrated_only_and_unsupported_tokens() {
        let i = get();
        assert!(i.is_integrated_only("PDM"));
        assert!(!i.is_integrated_only("Calculate"));
        assert!(i.unsupported_c_token("==").is_some());
        assert!(i.unsupported_c_token("&&").is_some());
        assert!(i.unsupported_c_token("+").is_none());
    }

    #[test]
    fn object_methods_present() {
        let i = get();
        assert!(!i.object_method("AsInteger").is_empty());
        assert!(!i.object_method("Lookup").is_empty());
    }
}
