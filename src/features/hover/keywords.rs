//! Static documentation tables for M1 language keywords/constructs (#166),
//! primitive type names inside `<…>` annotations (#164), and reference/scope
//! keywords at the head of a reference (#167), drawn from the M1 Development
//! Manual. The seam consumed by [`super::hover_with_eval`] for tokens that are
//! not part of a resolvable dotted path.
use m1_core::Kind;

/// Hover docs for the M1 language keywords/constructs (#166), keyed by CST kind.
/// Drawn from the M1 Development Manual.
const LANGUAGE_KEYWORD_DOCS: &[(Kind, &str)] = &[
    (
        Kind::If,
        "**if** `keyword`\n\nTests the parenthesised condition and, when true, executes the braced block. Combine with `else` / `else if` for alternative branches.",
    ),
    (
        Kind::Else,
        "**else** `keyword`\n\nThe alternative branch of an `if`: its block runs when the `if` condition (and any `else if` conditions) are false.",
    ),
    (
        Kind::When,
        "**when** `keyword`\n\nBegins a `when … is` construct — a multi-branch match on an enumerated value. Each `is (Value)` block runs when the argument equals that enumerator. It is an enum match, not a fall-through C `switch`.",
    ),
    (
        Kind::Is,
        "**is** `keyword`\n\nIntroduces one branch of a `when … is` construct: `is (Value) { … }` runs when the `when` argument equals `Value`.",
    ),
    (
        Kind::Expand,
        "**expand** `keyword`\n\nBegins an `expand ([name] = [start] to [end])` construct: the body is unrolled at **compile time**, once per value in the range. It is code generation, not a runtime loop.",
    ),
    (
        Kind::To,
        "**to** `keyword`\n\nSeparates the start and end bounds of an `expand ([name] = [start] to [end])` range.",
    ),
    (
        Kind::Local,
        "**local** `keyword`\n\nDefines a local variable inside a function. Locals are not visible outside the function and cannot be logged in M1 Tune; a local must be defined before it is used.",
    ),
    (
        Kind::Static,
        "**static** `keyword`\n\nWith `local`, makes a local variable retain its value across executions: it is assigned its initial value on the first run and keeps the last value on subsequent runs (a plain `local` is re-initialised every execution).",
    ),
];

/// Hover docs for M1 primitive type names appearing inside a `<…>` type
/// annotation (#164), drawn from the M1 Development Manual. A non-primitive (e.g.
/// an enum-type annotation) isn't listed, so the lookup misses and the caller
/// falls through to the enum-type description.
const PRIMITIVE_TYPE_DOCS: &[(&str, &str)] = &[
    (
        "Boolean",
        "**Boolean** `primitive type`\n\nA truth value (`true` / `false`). Restricted to local variables.",
    ),
    (
        "Integer",
        "**Integer** `primitive type`\n\nA signed whole number (positive, negative, or zero).",
    ),
    (
        "Unsigned Integer",
        "**Unsigned Integer** `primitive type`\n\nA non-negative whole number.",
    ),
    (
        "Floating Point",
        "**Floating Point** `primitive type`\n\nA real number, supporting a wide range of values with fractional precision.",
    ),
    (
        "Fixed Point 7dps",
        "**Fixed Point 7dps** `primitive type`\n\nAn integer scaled by 1e-7 — a signed number with seven fixed decimal places.",
    ),
    (
        "String",
        "**String** `primitive type`\n\nA text value, used for display in information windows. Restricted to local variables.",
    ),
];

/// Hover docs for the M1 reference/scope keywords used at the head of an object
/// reference (#167), drawn from the M1 Development Manual. Matched by exact text,
/// since these are ordinary identifier segments in the grammar.
const REFERENCE_KEYWORD_DOCS: &[(&str, &str)] = &[
    (
        "Root",
        "**Root** `reference keyword`\n\nThe root group of the Project — the first constituent of an absolute object reference (`Root.Group.Channel`). Use it to disambiguate when a nearer object shares the same name.",
    ),
    (
        "Parent",
        "**Parent** `reference keyword`\n\nThe object containing the current one. Unqualified, it resolves to the parent of the group the current object is stored in (`Parent.Channel`).",
    ),
    (
        "This",
        "**This** `reference keyword`\n\nThe group the current object is stored within. Use it to disambiguate when an object of the same name exists in an enclosing scope (`This.Channel`).",
    ),
    (
        "In",
        "**In** `reference keyword`\n\nThe object holding a function's input arguments; reference them with the `.` operator (`In.Argument`).",
    ),
    (
        "Out",
        "**Out** `reference keyword`\n\nThe object holding a function's return value; assign it with `=` (`Out.Result = …`).",
    ),
    (
        "Library",
        "**Library** `reference keyword`\n\nForms a library-function reference (`Library.Calculate.Max(…)`). Use it to disambiguate when an object name conflicts with a library-function name.",
    ),
];

/// Documentation for an M1 language keyword/construct, by CST kind (#166).
pub(super) fn language_keyword_doc(kind: Kind) -> Option<&'static str> {
    LANGUAGE_KEYWORD_DOCS
        .iter()
        .find(|(k, _)| *k == kind)
        .map(|(_, doc)| *doc)
}

/// Documentation for an M1 primitive type name inside a `<…>` annotation (#164).
pub(super) fn primitive_type_doc(name: &str) -> Option<&'static str> {
    PRIMITIVE_TYPE_DOCS
        .iter()
        .find(|(n, _)| *n == name)
        .map(|(_, doc)| *doc)
}

/// Documentation for an M1 reference/scope keyword at the head of a reference (#167).
pub(super) fn reference_keyword_doc(name: &str) -> Option<&'static str> {
    REFERENCE_KEYWORD_DOCS
        .iter()
        .find(|(n, _)| *n == name)
        .map(|(_, doc)| *doc)
}
