//! textDocument/hover: describe the symbol/local/opaque under the cursor.
use crate::convert::range;
use crate::features::locate::{
    build_scope, node_at_byte, path_at_byte, segment_at_byte, segment_nodes,
};
use crate::line_index::{LineIndex, PositionEncoding};
use m1_core::Kind;
use m1_typecheck::project::Project;
use m1_typecheck::resolve::{Resolution, Scope, resolve};
use m1_typecheck::symbols::{Symbol, SymbolKind, TableMeta};
use m1_typecheck::types::ValueType;
use tower_lsp::lsp_types::{Hover, HoverContents, MarkupContent, MarkupKind};

pub(crate) fn value_type_str(t: ValueType) -> &'static str {
    match t {
        ValueType::Boolean => "Boolean",
        ValueType::Integer => "Integer",
        ValueType::Unsigned => "Unsigned",
        ValueType::Float => "Float",
        ValueType::Enum(_) => "Enum",
        ValueType::String => "String",
        ValueType::Unknown => "Unknown",
    }
}

fn kind_str(k: SymbolKind) -> &'static str {
    match k {
        SymbolKind::Channel => "channel",
        SymbolKind::Parameter => "parameter",
        SymbolKind::Constant => "constant",
        SymbolKind::Function => "function",
        SymbolKind::Method => "method",
        SymbolKind::Table => "table",
        SymbolKind::Group => "group",
        SymbolKind::Reference => "reference",
        SymbolKind::Object => "object",
        SymbolKind::Other => "symbol",
    }
}

/// The header line: bold path + kind badge (`**Root.X** \`channel\``).
fn header_markdown(sym: &Symbol) -> String {
    format!("**{}** `{}`\n\n", sym.path, kind_str(sym.kind))
}

/// Object hover: the package class (not a value type — an object isn't
/// value-bearing) plus, for a CAN message object, the frame id + payload size
/// (#80). `None` when `sym` isn't an object.
fn object_markdown(sym: &Symbol) -> Option<String> {
    if sym.kind != SymbolKind::Object {
        return None;
    }
    let mut s = match &sym.class {
        Some(class) => format!("class: `{class}`"),
        None => "object".to_string(),
    };
    if let Some(can) = &sym.can
        && let (Some(id), Some(dlc)) = (can.can_id, can.dlc)
    {
        s.push_str(&format!("\n\nCAN id: `0x{id:X}`  ·  `{dlc}` bytes"));
    }
    Some(s)
}

/// The value-type fragment for the badge row (`type: \`Enum (Drive State)\``),
/// plus, for an enum channel, the rendered list of members (default marked) so
/// the caller can append a `values:` line. `enum_values` is `None` for a
/// non-enum or an unresolved enum.
fn type_markdown(sym: &Symbol, project: Option<&Project>) -> (String, Option<String>) {
    let mut enum_values: Option<String> = None;
    let type_str = match sym.value_type {
        ValueType::Enum(id) => match project.map(|p| p.symbols().enum_type(id)) {
            Some(et) => {
                // List members in ContainerOrder, marking the default.
                let mut members: Vec<&(String, i64)> = et.members.iter().collect();
                members.sort_by_key(|(_, order)| *order);
                let list = members
                    .iter()
                    .map(|(name, _)| {
                        if et.default.as_deref() == Some(name.as_str()) {
                            format!("`{name}` (default)")
                        } else {
                            format!("`{name}`")
                        }
                    })
                    .collect::<Vec<_>>()
                    .join(", ");
                if !list.is_empty() {
                    enum_values = Some(list);
                }
                format!("Enum ({})", et.name)
            }
            None => "Enum".to_string(),
        },
        other => value_type_str(other).to_string(),
    };
    let mut s = format!("type: `{type_str}`");
    // Distinguish *why* a type is Unknown (#177): a declared-but-unresolvable
    // type (e.g. a cross-module `MoTeC Types.*` / `::Hardware.*` enum the model
    // can't resolve) preserves what the project wrote; no declaration at all is a
    // genuine inference gap. Only annotate the truly-Unknown case — a resolved
    // type renders unchanged.
    if sym.value_type == ValueType::Unknown {
        match &sym.declared_type {
            Some(decl) => s.push_str(&format!(" (declared: `{decl}`, not resolved by the model)")),
            None => s.push_str(" (no type declared — not inferred)"),
        }
    }
    (s, enum_values)
}

/// The trailing badge fragments after the type — unit, security, call/log rate,
/// tags — each prefixed with `  ·  `. Empty when none apply.
fn meta_badges_markdown(sym: &Symbol) -> String {
    let mut s = String::new();
    if let Some(unit) = &sym.unit {
        s.push_str(&format!("  ·  unit: `{unit}`"));
    }
    // Security / access level from the `.m1prj` `<Props Security>` (#77).
    if let Some(security) = &sym.security {
        s.push_str(&format!("  ·  security: `{security}`"));
    }
    // Execution rate of a script/function, from its event trigger (#76).
    if let Some(hz) = sym.call_rate_hz {
        s.push_str(&format!("  ·  call rate: `{} Hz`", fmt_num(hz)));
    }
    // Default logging rate of a channel, from `<Props DefaultLogRate>` (#171).
    if let Some(hz) = sym.log_rate_hz {
        s.push_str(&format!("  ·  log rate: `{} Hz`", fmt_num(hz)));
    }
    // Tags (own + inherited) from `<Props SelectedTags>` (#170).
    if !sym.tags.is_empty() {
        let badge = sym
            .tags
            .iter()
            .map(|t| format!("`{t}`"))
            .collect::<Vec<_>>()
            .join(" ");
        s.push_str(&format!("  ·  tags: {badge}"));
    }
    s
}

fn symbol_markdown(sym: &Symbol, project: Option<&Project>) -> String {
    let mut s = header_markdown(sym);
    // For objects, show the package class instead of a (meaningless) value type.
    if let Some(obj) = object_markdown(sym) {
        s.push_str(&obj);
        return s;
    }
    // A table's own hover shows its shape (from the `.m1cfg`), not a value type —
    // the table object isn't value-bearing; its interpolated result is the
    // separate `.Value` channel (#25).
    if sym.kind == SymbolKind::Table
        && let Some(meta) = &sym.table_meta
    {
        s.push_str(&table_markdown(meta));
        return s;
    }
    // Name the concrete enum type when known (e.g. `Enum (Drive State)`), and
    // collect its valid values to list below.
    let (type_frag, enum_values) = type_markdown(sym, project);
    s.push_str(&type_frag);
    s.push_str(&meta_badges_markdown(sym));
    if let Some(values) = enum_values {
        s.push_str(&format!("\n\nvalues: {values}"));
    }
    // CAN/DBC signal layout: range, scale/offset, parent frame, bit position (#80).
    if let Some(dbc) = dbc_signal_markdown(sym, project) {
        s.push_str(&format!("\n\n{dbc}"));
    }
    s
}

/// Hover for an enum-member token, e.g. the trailing `Off` in `Drive State.Off`
/// — which resolves to neither a project symbol nor a built-in method, but which
/// the project model fully defines as `EnumType.Member` (name + integer value).
/// Renders `**{Enum}.{Member}** \`enum member\`\n\n= {value}`.
///
/// The segment under the cursor (`seg`, index `i` in `segs`) is the member name.
/// The enum is identified by the segment to its left when that is the enum's name
/// (`Drive State.Off`); a bare member (`Off`) is resolved via `enums_with_member`
/// when it is unambiguous (declared by exactly one enum). Returns `None` when the
/// segment is not an enum member.
fn enum_member_markdown(
    seg: m1_core::Node,
    i: usize,
    segs: &[m1_core::Node],
    project: Option<&Project>,
) -> Option<String> {
    let table = project?.symbols();
    let member = seg.text();
    // Prefer the explicit `Enum.Member` form: the immediately-preceding segment
    // names the enum type.
    let id = if i > 0
        && let Some(id) = table.enum_by_name(segs[i - 1].text())
        && table.enum_has_member(id, member)
    {
        id
    } else {
        // Bare member: accept only when exactly one enum declares it (no ambiguity).
        match table.enums_with_member(member) {
            [only] => *only,
            _ => return None,
        }
    };
    let et = table.enum_type(id);
    let value = et
        .members
        .iter()
        .find(|(m, _)| m == member)
        .map(|(_, v)| v)?;
    Some(format!(
        "**{}.{member}** `enum member`\n\n= {value}",
        et.name
    ))
}

/// Hover for the *head* of an enum literal, e.g. the `Gear State` in
/// `Gear State.Driving` (or AV-M1's `ASSI.Driving`). The enum *type* name is not
/// a project symbol — it lives in the type table, not the channel table — so it
/// resolves Opaque and would otherwise fall back to "type not modelled". When the
/// segment names a known enum, describe the enum: its name and the valid members
/// (ContainerOrder, default marked), matching the channel-side enum rendering.
/// Returns `None` when the segment is not an enum name.
fn enum_type_markdown(name: &str, project: Option<&Project>) -> Option<String> {
    let table = project?.symbols();
    let id = table.enum_by_name(name)?;
    let et = table.enum_type(id);
    let mut members: Vec<&(String, i64)> = et.members.iter().collect();
    members.sort_by_key(|(_, order)| *order);
    let list = members
        .iter()
        .map(|(name, _)| {
            if et.default.as_deref() == Some(name.as_str()) {
                format!("`{name}` (default)")
            } else {
                format!("`{name}`")
            }
        })
        .collect::<Vec<_>>()
        .join(", ");
    let mut s = format!("**{}** `enum type`", et.name);
    if !list.is_empty() {
        s.push_str(&format!("\n\nvalues: {list}"));
    }
    Some(s)
}

/// `ASSI` in `ASSI.Driving`: the *head* of an `EnumName.Member` literal. An enum
/// type routinely shares its name with a group/channel (the AV-M1 `ASSI` enum
/// lives alongside the `Root.Control.AV.ASSI` group), so plain per-segment
/// resolution returns that shadowing group (`type: Unknown`). The disambiguator
/// is the *next* segment: when it names a member of the enum this segment names,
/// the pair is unambiguously an enum literal, so describe the enum type rather
/// than the group. Returns `None` when there is no following member of a matching
/// enum (e.g. `ASSI.Status`, a real group-relative path).
fn enum_literal_head_markdown(
    i: usize,
    segs: &[m1_core::Node],
    project: Option<&Project>,
    scope: &Scope,
) -> Option<String> {
    let table = project?.symbols();
    let id = table.enum_by_name(segs[i].text())?;
    let next = segs.get(i + 1)?;
    if table.enum_has_member(id, next.text()) {
        return enum_type_markdown(segs[i].text(), project);
    }
    // The next segment is not a member. Decide whether this is still an enum
    // literal with a *misspelled* member (`Color.Grren`) or a genuine path:
    let head_path = segs[..=i]
        .iter()
        .map(|n| n.text())
        .collect::<Vec<_>>()
        .join(".");
    // If the head resolves to a *value* symbol (a channel/parameter), the
    // construct is `<value>.<accessor>` (e.g. `Drive State.AsInteger`), not an
    // enum literal — defer to that symbol's hover.
    if let Resolution::Symbol(s) = resolve(&head_path, scope)
        && !matches!(
            s.kind,
            m1_typecheck::symbols::SymbolKind::Group | m1_typecheck::symbols::SymbolKind::Object
        )
    {
        return None;
    }
    // If `head.next` resolves to a real symbol, it is a genuine group-relative
    // path that merely shares the enum's name (`ASSI.Status`) — let the symbol
    // hover handle it. Otherwise the member is misspelled and the head is an
    // enum (optionally colliding with a group): show the enum's valid members and
    // flag the bad member — the most useful thing on a broken line (#163).
    let path = format!("{head_path}.{}", next.text());
    if matches!(resolve(&path, scope), Resolution::Symbol(_)) {
        return None;
    }
    enum_type_markdown(segs[i].text(), project)
        .map(|md| format!("{md}\n\n⚠ `{}` is not a member of this enum", next.text()))
}

/// Compact decimal: up to 6 places, trailing zeros trimmed (`0.010000` → `0.01`,
/// `60.000000` → `60`). Keeps `.m1dbc` multipliers like `9.999e-03` readable.
fn fmt_num(x: f64) -> String {
    let s = format!("{x:.6}");
    s.trim_end_matches('0').trim_end_matches('.').to_string()
}

/// Render a table's shape for hover (#25): dimensionality and breakpoint counts
/// (`2-D table · shape: 11 × 7`), the per-axis units when declared, and the
/// interpolated output unit. The output *type* shows on the table's `.Value`
/// channel, not here.
fn table_markdown(meta: &TableMeta) -> String {
    let shape = meta
        .axes
        .iter()
        .map(|a| a.size.to_string())
        .collect::<Vec<_>>()
        .join(" × ");
    let mut s = if shape.is_empty() {
        "\n\ntable".to_string()
    } else {
        format!("\n\n{}-D table  ·  shape: `{shape}`", meta.axes.len())
    };
    let axis_units: Vec<String> = meta
        .axes
        .iter()
        .enumerate()
        .filter_map(|(i, a)| {
            let label = ["X", "Y", "Z"].get(i).copied().unwrap_or("?");
            a.unit.as_ref().map(|u| format!("{label} `{u}`"))
        })
        .collect();
    if !axis_units.is_empty() {
        s.push_str(&format!("\n\naxes: {}", axis_units.join(", ")));
    }
    if let Some(u) = &meta.output_unit {
        s.push_str(&format!("\n\noutput: `{u}`"));
    }
    s
}

/// Layout detail for a CAN signal channel (#80): physical range, scale/offset,
/// the parent message's frame (id + byte count, looked up in `project`), and the
/// signal's bit position/length. Returns `None` for symbols without signal-level
/// CAN metadata (i.e. anything not sourced from a `.m1dbc` signal).
fn dbc_signal_markdown(sym: &Symbol, project: Option<&Project>) -> Option<String> {
    let can = sym.can.as_ref()?;
    // Signal-level metadata distinguishes a signal from a message object.
    if can.start_bit.is_none() && can.length.is_none() && sym.dbc_range.is_none() {
        return None;
    }
    let mut lines = vec!["Kind: `CAN Signal`".to_string()];
    if let Some((lo, hi)) = sym.dbc_range {
        lines.push(format!("Range: `{} – {}`", fmt_num(lo), fmt_num(hi)));
    }
    if let (Some(m), Some(o)) = (can.multiplier, can.offset) {
        let (m, o) = (fmt_num(m), fmt_num(o));
        lines.push(format!("Scale: `{m}`  ·  Offset: `{o}`"));
    }
    // Parent message frame: strip the signal leaf, look the message up by path.
    if let Some((parent, _)) = sym.path.rsplit_once('.') {
        let msg_name = parent.rsplit_once('.').map_or(parent, |(_, n)| n);
        let frame = project
            .and_then(|p| p.symbols().get(parent))
            .and_then(|m| m.can.as_ref())
            .map(|c| match (c.can_id, c.dlc) {
                (Some(id), Some(dlc)) => format!(" (0x{id:X}, {dlc} bytes)"),
                (Some(id), None) => format!(" (0x{id:X})"),
                _ => String::new(),
            })
            .unwrap_or_default();
        lines.push(format!("Message: `{msg_name}`{frame}"));
    }
    if let (Some(bit), Some(len)) = (can.start_bit, can.length) {
        lines.push(format!("Bit pos: `{bit}`  ·  Length: `{len}` bits"));
    }
    Some(lines.join("\n\n"))
}

use m1_typecheck::intrinsics::Overload;

/// `(p1: T1, p2: T2) -> Ret` for one overload signature.
fn signature(path: &str, ov: &Overload) -> String {
    let params: Vec<String> = ov
        .params
        .iter()
        .map(|p| format!("{}: {}", p.name, p.ty))
        .collect();
    format!("{path}({}) -> {}", params.join(", "), ov.returns)
}

fn builtin_object_markdown(name: &str) -> String {
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
fn object_method_markdown(name: &str, overloads: &[&Overload]) -> String {
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

fn builtin_fn_markdown(path: &str, overloads: &[&Overload]) -> String {
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

/// Render hover for the path at `byte`. `project`/`file_name` drive resolution.
///
/// Resolution is *per-segment*: in `Control.Drive State.AsInteger()` the cursor's
/// segment decides what is described — `Control` the group, `Drive State` the
/// enum channel, `AsInteger` the built-in accessor method — rather than treating
/// the whole dotted expression as one opaque object.
/// Documentation for an M1 language keyword/construct token (#166), drawn from
/// the M1 Development Manual. Keyword tokens are not part of a dotted path, so
/// without this the hover provider returns nothing over them. `None` for any
/// non-documented kind.
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
fn language_keyword_doc(kind: Kind) -> Option<&'static str> {
    LANGUAGE_KEYWORD_DOCS
        .iter()
        .find(|(k, _)| *k == kind)
        .map(|(_, doc)| *doc)
}

/// Documentation for an M1 primitive type name inside a `<…>` annotation (#164).
fn primitive_type_doc(name: &str) -> Option<&'static str> {
    PRIMITIVE_TYPE_DOCS
        .iter()
        .find(|(n, _)| *n == name)
        .map(|(_, doc)| *doc)
}

/// Documentation for an M1 reference/scope keyword at the head of a reference (#167).
fn reference_keyword_doc(name: &str) -> Option<&'static str> {
    REFERENCE_KEYWORD_DOCS
        .iter()
        .find(|(n, _)| *n == name)
        .map(|(_, doc)| *doc)
}

pub fn hover(
    root: m1_core::Node,
    byte: usize,
    project: Option<&Project>,
    file_name: Option<&str>,
    li: &LineIndex,
    enc: PositionEncoding,
) -> Option<Hover> {
    // Language-keyword/construct docs (#166). Keyword tokens (`if`, `when`,
    // `expand`, `local`, …) are not part of a dotted path, so `path_at_byte`
    // below would miss them; handle them up front.
    if let Some(node) = node_at_byte(root, byte)
        && let Some(doc) = language_keyword_doc(node.kind())
    {
        return Some(Hover {
            contents: HoverContents::Markup(MarkupContent {
                kind: MarkupKind::Markdown,
                value: doc.to_string(),
            }),
            range: Some(range(&node.byte_range(), li, enc)),
        });
    }

    let (top, _full) = path_at_byte(root, byte)?;
    let segs = segment_nodes(top);
    if segs.is_empty() {
        return None;
    }
    let i = segment_at_byte(top, byte).unwrap_or(segs.len() - 1);
    let scope = build_scope(root, project, file_name);

    // The dotted prefix up to and including the segment under the cursor.
    let prefix = segs[..=i]
        .iter()
        .map(|n| n.text())
        .collect::<Vec<_>>()
        .join(".");
    let seg = segs[i];
    let seg_text = seg.text();

    // Enum-literal head (`ASSI` in `ASSI.Driving`) must win over plain resolution:
    // the enum type often shares its name with a group/channel, so resolving the
    // segment alone would describe that shadowing symbol. Decided by the following
    // member segment, so it never misfires on a real group-relative path.
    // Primitive type name inside a `<…>` annotation (#164): `local <Integer>`.
    // The type name resolves as an opaque project path and would hover as "type
    // not modelled"; describe the primitive instead. A non-primitive annotation
    // (an enum type) returns None here and falls through to the enum handling.
    let md = if seg.parent().map(|p| p.kind()) == Some(Kind::TypeAnnotation)
        && let Some(doc) = primitive_type_doc(seg_text)
    {
        doc.to_string()
    }
    // Reference/scope keyword at the head of the reference (#167): `Root`,
    // `Parent`, `This`, `In`, `Out`, `Library`. These resolve to unhelpful or
    // misleading hovers (Root → "group / Unknown", Parent → the parent group's
    // own hover), so when the cursor is on the anchor itself, describe the
    // keyword's meaning instead.
    else if i == 0
        && let Some(doc) = reference_keyword_doc(seg_text)
    {
        doc.to_string()
    } else if let Some(md) = enum_literal_head_markdown(i, &segs, project, &scope) {
        md
    } else {
        match resolve(&prefix, &scope) {
            Resolution::Local(t) => {
                format!("**{prefix}** `local`\n\ntype: `{}`", value_type_str(t))
            }
            Resolution::Symbol(sym) => symbol_markdown(sym, project),
            Resolution::BuiltinObject(name) => builtin_object_markdown(name),
            Resolution::BuiltinFn(overloads) => builtin_fn_markdown(&prefix, &overloads),
            Resolution::Opaque | Resolution::Unresolved => {
                // A trailing accessor (`object.AsInteger`) doesn't resolve to a
                // project symbol, but the object on its left does. Describe the
                // built-in method itself, with the manual's docs.
                let methods = m1_typecheck::intrinsics::get().object_method(seg_text);
                let object_resolves = i > 0
                    && matches!(
                        resolve(
                            &segs[..i]
                                .iter()
                                .map(|n| n.text())
                                .collect::<Vec<_>>()
                                .join("."),
                            &scope
                        ),
                        Resolution::Symbol(_)
                            | Resolution::Opaque
                            | Resolution::BuiltinObject(_)
                            | Resolution::Local(_)
                    );
                if object_resolves && !methods.is_empty() {
                    object_method_markdown(seg_text, &methods)
                } else if let Some(md) = enum_member_markdown(seg, i, &segs, project) {
                    // An enum-member token (`Drive State.Off`, or a bare `Off`) — the
                    // project model defines the enum + member, so describe it rather
                    // than fall through to "type not modelled" (#127).
                    md
                } else if let Some(md) = enum_type_markdown(seg_text, project) {
                    // The head of an `EnumName.Member` literal (e.g. `ASSI` in
                    // `ASSI.Driving`): the enum type itself. Describe the enum rather
                    // than fall through to "type not modelled".
                    md
                } else if matches!(resolve(&prefix, &scope), Resolution::Opaque) {
                    format!("**{prefix}**\n\nbuilt-in symbol — type not modelled")
                } else {
                    return None;
                }
            }
        }
    };
    Some(Hover {
        contents: HoverContents::Markup(MarkupContent {
            kind: MarkupKind::Markdown,
            value: md,
        }),
        // Highlight just the hovered segment, not the whole expression.
        range: Some(range(&seg.byte_range(), li, enc)),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Hover markdown at the first occurrence of `find`, with no project loaded.
    fn kw_hover(src: &str, find: &str) -> Option<String> {
        let cst = m1_core::parse(src);
        let li = LineIndex::new(src);
        let byte = src.find(find).unwrap();
        hover(cst.root(), byte, None, None, &li, PositionEncoding::Utf16).map(|h| {
            match h.contents {
                HoverContents::Markup(m) => m.value,
                _ => String::new(),
            }
        })
    }

    #[test]
    fn primitive_type_name_in_annotation_has_doc_not_unmodelled() {
        // #164: the type name inside `local <Integer>` resolved as an opaque path
        // and hovered as "built-in symbol — type not modelled". It is a primitive
        // type; describe it.
        let md = kw_hover("local <Integer> myValue = 0;\n", "Integer").unwrap();
        assert!(md.contains("primitive type"), "got: {md}");
        assert!(
            !md.contains("not modelled"),
            "should not show the unmodelled fallback: {md}"
        );
    }

    #[test]
    fn multiword_primitive_type_name_has_doc() {
        let md = kw_hover("local <Floating Point> r = 0.0;\n", "Floating Point").unwrap();
        assert!(md.to_lowercase().contains("floating point"), "got: {md}");
        assert!(md.contains("primitive type"), "got: {md}");
    }

    #[test]
    fn unsigned_integer_primitive_has_doc() {
        let md = kw_hover("local <Unsigned Integer> u = 0;\n", "Unsigned Integer").unwrap();
        assert!(md.contains("primitive type"), "got: {md}");
        assert!(md.to_lowercase().contains("non-negative"), "got: {md}");
    }

    #[test]
    fn language_keyword_local_has_doc() {
        // #166: a keyword token is not part of a path, so hover used to return null.
        let md = kw_hover("local x = 1;\n", "local").expect("local should have a doc");
        assert!(md.contains("local variable"), "got: {md}");
        assert!(md.to_lowercase().contains("function"), "got: {md}");
    }

    #[test]
    fn language_keyword_when_explains_enum_match() {
        let md = kw_hover("when (Mode)\n{\nis (Red)\n{\n}\n}\n", "when").unwrap();
        assert!(md.contains("when"), "got: {md}");
        assert!(md.to_lowercase().contains("match"), "got: {md}");
    }

    #[test]
    fn language_keyword_expand_explains_compile_time_unroll() {
        let md = kw_hover("expand (i = 1 to 3)\n{\n}\n", "expand").unwrap();
        assert!(md.to_lowercase().contains("compile"), "got: {md}");
    }

    #[test]
    fn language_keyword_static_explains_persistence() {
        let md = kw_hover("static local x = 1;\n", "static").unwrap();
        assert!(
            md.to_lowercase().contains("across executions") || md.to_lowercase().contains("retain"),
            "got: {md}"
        );
    }

    #[test]
    fn reference_keyword_root_has_doc() {
        // #167: `Root` used to show only "group / type: Unknown".
        let md = kw_hover("Root.Demo.X = 0;\n", "Root").unwrap();
        assert!(md.to_lowercase().contains("root group"), "got: {md}");
    }

    #[test]
    fn reference_keyword_in_explains_input_args() {
        let md = kw_hover("In.Widget Count = 0;\n", "In").unwrap();
        assert!(md.to_lowercase().contains("input"), "got: {md}");
    }

    #[test]
    fn reference_keyword_parent_explains_container() {
        let md = kw_hover("Parent.X = 0;\n", "Parent").unwrap();
        assert!(
            md.to_lowercase().contains("containing") || md.to_lowercase().contains("parent"),
            "got: {md}"
        );
    }

    #[test]
    fn reference_keyword_doc_only_at_head_not_on_trailing_member() {
        // Hovering `Demo` (a real-ish group segment, not the anchor) must NOT
        // produce the Root keyword doc — the ref-keyword doc is head-only.
        let md = kw_hover("Root.Demo.X = 0;\n", "Demo");
        assert!(
            md.as_deref()
                .map(|m| !m.contains("root group"))
                .unwrap_or(true),
            "trailing segment wrongly got the Root doc: {md:?}"
        );
    }

    #[test]
    fn hovers_local_with_inferred_type() {
        let src = "local fGain = 1.0;\nfGain = fGain + 1.0;\n";
        let cst = m1_core::parse(src);
        let li = LineIndex::new(src);
        let byte = src.rfind("fGain").unwrap();
        let h = hover(cst.root(), byte, None, None, &li, PositionEncoding::Utf16).unwrap();
        if let HoverContents::Markup(m) = h.contents {
            assert!(m.value.contains("local"));
            assert!(m.value.contains("Float"));
        } else {
            panic!("expected markup");
        }
    }

    #[allow(clippy::field_reassign_with_default)]
    fn channel(value_type: ValueType, declared_type: Option<&str>) -> Symbol {
        Symbol {
            path: "Root.Demo.X".into(),
            kind: SymbolKind::Channel,
            value_type,
            declared_type: declared_type.map(Into::into),
            unit: None,
            qty: None,
            display_unit: None,
            security: None,
            filename: None,
            enum_assoc: None,
            class: None,
            classname: None,
            def_line: None,
            dbc_range: None,
            can: None,
            call_rate_hz: None,
            log_rate_hz: None,
            tags: Vec::new(),
            return_type: None,
            in_params: None,
            table_meta: None,
        }
    }

    #[test]
    fn hover_unknown_with_declared_type_shows_the_declaration() {
        // #177: a channel whose declared type the model cannot resolve should say
        // so — the declared string is preserved — instead of a bare `Unknown`.
        let sym = channel(
            ValueType::Unknown,
            Some("MoTeC Types.Direction Enumeration"),
        );
        let md = symbol_markdown(&sym, None);
        assert!(md.contains("Unknown"), "got: {md}");
        assert!(
            md.contains("declared:") && md.contains("MoTeC Types.Direction Enumeration"),
            "should surface the unresolved declared type: {md}"
        );
    }

    #[test]
    fn hover_unknown_without_declaration_says_not_inferred() {
        // #177: no declared type at all is a different case — say so, so the two
        // are distinguishable in hover.
        let sym = channel(ValueType::Unknown, None);
        let md = symbol_markdown(&sym, None);
        assert!(md.contains("Unknown"), "got: {md}");
        assert!(
            md.to_lowercase().contains("not inferred") || md.contains("no type declared"),
            "should indicate nothing was declared/inferred: {md}"
        );
        assert!(!md.contains("declared:"), "no declaration to show: {md}");
    }

    #[test]
    fn hover_known_type_is_unaffected_by_declared_type() {
        // A resolved type renders exactly as before, with no Unknown annotation.
        let sym = channel(ValueType::Float, Some("f32"));
        let md = symbol_markdown(&sym, None);
        assert!(md.contains("type: `Float`"), "got: {md}");
        assert!(!md.contains("declared:"), "got: {md}");
        assert!(!md.to_lowercase().contains("not inferred"), "got: {md}");
    }

    #[test]
    fn hover_shows_security_level() {
        let sym = Symbol {
            path: "Root.Engine.Throttle".into(),
            kind: SymbolKind::Channel,
            value_type: ValueType::Float,
            declared_type: None,
            unit: Some("%".into()),
            qty: None,
            display_unit: None,
            security: Some("Protected".into()),
            filename: None,
            enum_assoc: None,
            class: None,
            classname: None,
            def_line: None,
            dbc_range: None,
            can: None,
            call_rate_hz: None,
            log_rate_hz: None,
            tags: Vec::new(),
            return_type: None,
            in_params: None,
            table_meta: None,
        };
        let md = symbol_markdown(&sym, None);
        assert!(md.contains("security: `Protected`"), "got: {md}");
    }

    #[test]
    fn hover_shows_script_call_rate() {
        let sym = Symbol {
            path: "Root.Engine.Control".into(),
            kind: SymbolKind::Method,
            value_type: ValueType::Unknown,
            declared_type: None,
            unit: None,
            qty: None,
            display_unit: None,
            security: None,
            filename: None,
            enum_assoc: None,
            class: None,
            classname: None,
            def_line: None,
            dbc_range: None,
            can: None,
            call_rate_hz: Some(100.0),
            log_rate_hz: None,
            tags: Vec::new(),
            return_type: None,
            in_params: None,
            table_meta: None,
        };
        let md = symbol_markdown(&sym, None);
        assert!(md.contains("call rate: `100 Hz`"), "got: {md}");
    }

    // #171: a channel's default log rate (Hz) appears as a hover badge.
    #[test]
    fn hover_shows_default_log_rate() {
        let mut sym = channel(ValueType::Unsigned, Some("u32"));
        sym.log_rate_hz = Some(200.0);
        let md = symbol_markdown(&sym, None);
        assert!(md.contains("log rate: `200 Hz`"), "got: {md}");
    }

    // #170: a channel's tags appear as a hover badge, space-separated.
    #[test]
    fn hover_shows_tags() {
        let mut sym = channel(ValueType::Unsigned, Some("u32"));
        sym.tags = vec!["Engine".into(), "Normal".into()];
        let md = symbol_markdown(&sym, None);
        assert!(md.contains("tags: `Engine` `Normal`"), "got: {md}");
    }

    #[test]
    fn hover_shows_table_shape() {
        use m1_typecheck::symbols::TableAxis;
        let sym = Symbol {
            path: "Root.Control.Limiting.Torque".into(),
            kind: SymbolKind::Table,
            value_type: ValueType::Unknown,
            declared_type: None,
            unit: None,
            qty: None,
            display_unit: None,
            security: None,
            filename: None,
            enum_assoc: None,
            class: None,
            classname: None,
            def_line: None,
            dbc_range: None,
            can: None,
            call_rate_hz: None,
            log_rate_hz: None,
            tags: Vec::new(),
            return_type: None,
            in_params: None,
            table_meta: Some(TableMeta {
                axes: vec![
                    TableAxis {
                        size: 11,
                        unit: Some("A".into()),
                    },
                    TableAxis {
                        size: 7,
                        unit: Some("rpm".into()),
                    },
                ],
                output_unit: Some("N.m".into()),
            }),
        };
        let md = symbol_markdown(&sym, None);
        assert!(md.contains("2-D table"), "got: {md}");
        assert!(md.contains("shape: `11 × 7`"), "got: {md}");
        assert!(md.contains("X `A`"), "got: {md}");
        assert!(md.contains("output: `N.m`"), "got: {md}");
    }

    #[test]
    fn hover_shows_dbc_signal_layout() {
        use m1_typecheck::symbols::CanMeta;
        let sym = Symbol {
            path: "SBG DBC.Auto Slip.Angle Slip".into(),
            kind: SymbolKind::Channel,
            value_type: ValueType::Integer,
            declared_type: None,
            unit: Some("deg".into()),
            qty: None,
            display_unit: None,
            security: None,
            filename: Some("dbc/SBG DBC.m1dbc".into()),
            enum_assoc: None,
            class: None,
            classname: None,
            def_line: None,
            dbc_range: Some((-51.2, 51.1)),
            can: Some(CanMeta {
                can_id: None,
                dlc: None,
                start_bit: Some(10),
                length: Some(10),
                multiplier: Some(0.1),
                offset: Some(0.0),
            }),
            call_rate_hz: None,
            log_rate_hz: None,
            tags: Vec::new(),
            return_type: None,
            in_params: None,
            table_meta: None,
        };
        let md = symbol_markdown(&sym, None);
        assert!(md.contains("CAN Signal"), "got: {md}");
        assert!(md.contains("Range: `-51.2 – 51.1`"), "got: {md}");
        assert!(md.contains("Scale: `0.1`"), "got: {md}");
        assert!(md.contains("Offset: `0`"), "got: {md}");
        assert!(md.contains("Bit pos: `10`"), "got: {md}");
        assert!(md.contains("Length: `10` bits"), "got: {md}");
        // Unit still rendered from Qty.
        assert!(md.contains("unit: `deg`"), "got: {md}");
    }

    #[test]
    fn hover_dbc_signal_shows_parent_message_frame() {
        use std::io::Write;
        let tmp = tempfile::tempdir().unwrap();
        let prj = tmp.path().join("Project.m1prj");
        std::fs::File::create(&prj)
            .unwrap()
            .write_all(br#"<?xml version="1.0"?><Project></Project>"#)
            .unwrap();
        let dbc = tmp.path().join("Bus.m1dbc");
        std::fs::File::create(&dbc)
            .unwrap()
            .write_all(
                br#"<?xml version="1.0"?>
<DBC><ComponentStream><List>
 <Component Classname="BuiltIn.CAN.DBC" Name="Bus"/>
 <Component Classname="BuiltIn.CAN.Message" Name="Bus.BMS Status">
  <Props CANId="291" DLC="8"/>
 </Component>
 <Component Classname="BuiltIn.CAN.Signal" Name="Bus.BMS Status.Battery Voltage">
  <Props Type="u32" Qty="V" StartBit="16" Length="16" Multiplier="0.01" Offset="0.0"/>
 </Component>
</List></ComponentStream></DBC>"#,
            )
            .unwrap();
        let project = m1_typecheck::Project::load(&prj)
            .unwrap()
            .with_dbc(&dbc, "Bus.m1dbc")
            .unwrap();
        let key = "Bus.BMS Status.Battery Voltage";
        let sig = project.symbols().get(key).unwrap();
        let md = symbol_markdown(sig, Some(&project));
        assert!(
            md.contains("Message: `BMS Status` (0x123, 8 bytes)"),
            "got: {md}"
        );
        assert!(md.contains("Scale: `0.01`"), "got: {md}");
        assert!(md.contains("Bit pos: `16`"), "got: {md}");
        assert!(md.contains("unit: `V`"), "got: {md}");
    }

    #[test]
    fn hover_names_the_enum_type() {
        use std::io::Write;
        let tmp = tempfile::tempdir().unwrap();
        let prj = tmp.path().join("Project.m1prj");
        std::fs::File::create(&prj)
            .unwrap()
            .write_all(
                br#"<?xml version="1.0"?>
<Project>
  <DataTypes>
    <Type Name="Drive State" Storage="enum" Default="Idle">
      <Enum Name="Idle" ContainerOrder="1"/>
      <Enum Name="Off" ContainerOrder="0"/>
      <Enum Name="Running" ContainerOrder="2"/>
    </Type>
  </DataTypes>
  <Component Classname="BuiltIn.GroupCompound" Name="Root"/>
  <Component Classname="BuiltIn.GroupCompound" Name="Root.Control"/>
  <Component Classname="BuiltIn.Channel" Name="Root.Control.State"><Props Type="::This.Drive State"/></Component>
</Project>"#,
            )
            .unwrap();
        let project = m1_typecheck::Project::load(&prj).unwrap();
        let src = "Control.State = 1;\n";
        let cst = m1_core::parse(src);
        let li = LineIndex::new(src);
        let byte = src.find("State").unwrap();
        let h = hover(
            cst.root(),
            byte,
            Some(&project),
            Some("X.m1scr"),
            &li,
            PositionEncoding::Utf16,
        )
        .unwrap();
        if let HoverContents::Markup(m) = h.contents {
            assert!(
                m.value.contains("Drive State"),
                "hover should name the enum type, got: {}",
                m.value
            );
            // Lists every valid value, in ContainerOrder, with the default marked.
            assert!(m.value.contains("values:"), "got: {}", m.value);
            assert!(m.value.contains("`Off`"), "got: {}", m.value);
            assert!(m.value.contains("`Idle` (default)"), "got: {}", m.value);
            assert!(m.value.contains("`Running`"), "got: {}", m.value);
            let off = m.value.find("`Off`").unwrap();
            let idle = m.value.find("`Idle`").unwrap();
            let running = m.value.find("`Running`").unwrap();
            assert!(
                off < idle && idle < running,
                "values not in ContainerOrder: {}",
                m.value
            );
        } else {
            panic!("expected markup");
        }
    }

    #[test]
    fn opaque_hover_does_not_say_type_unknown() {
        // "Output" has no project context — resolves as Opaque.
        let src = "Output.Value = 1;\n";
        let cst = m1_core::parse(src);
        let li = LineIndex::new(src);
        let byte = src.find("Output").unwrap();
        let h = hover(cst.root(), byte, None, None, &li, PositionEncoding::Utf16).unwrap();
        if let HoverContents::Markup(m) = h.contents {
            assert!(
                !m.value.contains("type unknown"),
                "hover should not say 'type unknown' for opaque symbols: {}",
                m.value
            );
        } else {
            panic!("expected markup");
        }
    }

    #[test]
    fn library_function_hover_shows_signature() {
        let src = "x = Calculate.Max(a, b);\n";
        let cst = m1_core::parse(src);
        let li = LineIndex::new(src);
        let byte = src.find("Max").unwrap();
        let h = hover(cst.root(), byte, None, None, &li, PositionEncoding::Utf16).unwrap();
        if let HoverContents::Markup(m) = h.contents {
            assert!(m.value.contains("library function"), "{}", m.value);
            assert!(m.value.contains("Calculate.Max"), "{}", m.value);
            assert!(m.value.contains("->"), "{}", m.value);
        } else {
            panic!("expected markup");
        }
    }

    /// A project mirroring the EV-M1 sample line
    /// `Control.Drive State.AsInteger()`: a `Control` group, a `Drive State`
    /// channel under it typed as the `Drive State` enum, and the enum's members.
    fn drive_state_project() -> (tempfile::TempDir, Project) {
        use std::io::Write;
        let tmp = tempfile::tempdir().unwrap();
        let prj = tmp.path().join("Project.m1prj");
        std::fs::File::create(&prj)
            .unwrap()
            .write_all(
                br#"<?xml version="1.0"?>
<Project>
  <DataTypes>
    <Type Name="Drive State" Storage="enum" Default="Idle">
      <Enum Name="Idle" ContainerOrder="1"/>
      <Enum Name="Off" ContainerOrder="0"/>
    </Type>
  </DataTypes>
  <Component Classname="BuiltIn.GroupCompound" Name="Root"/>
  <Component Classname="BuiltIn.GroupCompound" Name="Root.Control"/>
  <Component Classname="BuiltIn.Channel" Name="Root.Control.Drive State"><Props Type="::This.Drive State"/></Component>
</Project>"#,
            )
            .unwrap();
        let project = m1_typecheck::Project::load(&prj).unwrap();
        (tmp, project)
    }

    fn hover_value_at(project: &Project, src: &str, find: &str, occurrence: usize) -> String {
        let cst = m1_core::parse(src);
        let li = LineIndex::new(src);
        // Byte of the `occurrence`-th match of `find` (0-based).
        let byte = src.match_indices(find).nth(occurrence).unwrap().0;
        let h = hover(
            cst.root(),
            byte,
            Some(project),
            Some("Control.Update.m1scr"),
            &li,
            PositionEncoding::Utf16,
        )
        .unwrap_or_else(|| panic!("no hover for `{find}`#{occurrence}"));
        match h.contents {
            HoverContents::Markup(m) => m.value,
            _ => panic!("expected markup"),
        }
    }

    #[test]
    fn hover_resolves_each_segment_of_a_dotted_accessor_separately() {
        // `Control.Drive State.AsInteger()` — hovering each segment must describe
        // that segment, not the whole expression: a group, an enum channel, a
        // built-in method.
        let (_tmp, project) = drive_state_project();
        let src = "if (Control.Drive State.AsInteger() > 0)\n{\n}\n";

        // 1) `Control` → the top-level group.
        let on_control = hover_value_at(&project, src, "Control", 0);
        assert!(on_control.contains("group"), "Control hover: {on_control}");
        assert!(
            !on_control.contains("AsInteger"),
            "Control hover must not describe the whole path: {on_control}"
        );

        // 2) `Drive State` (the channel) → the custom enum type + its values.
        let on_enum = hover_value_at(&project, src, "Drive State", 0);
        assert!(
            on_enum.contains("Enum") && on_enum.contains("Drive State"),
            "Drive State hover should name the enum: {on_enum}"
        );
        assert!(
            !on_enum.contains("AsInteger"),
            "Drive State hover must not describe the method: {on_enum}"
        );

        // 3) `AsInteger` → the built-in enum accessor method, with its docs.
        let on_method = hover_value_at(&project, src, "AsInteger", 0);
        assert!(
            on_method.contains("AsInteger"),
            "AsInteger hover should name the method: {on_method}"
        );
        assert!(
            on_method.to_lowercase().contains("method"),
            "AsInteger hover should label it a method: {on_method}"
        );
        assert!(
            on_method.contains("Integer representation"),
            "AsInteger hover should show its doc: {on_method}"
        );
    }

    #[test]
    fn hover_on_enum_member_renders_enum_member_value() {
        // `Drive State.Off` — hovering the trailing member `Off` must describe it
        // as the enum member it is (enum name, member, integer value), not fall
        // back to "built-in symbol — type not modelled" (#127). The `Drive State`
        // enum here declares `Off` (ContainerOrder 0) and `Idle` (1).
        let (_tmp, project) = drive_state_project();
        let src = "Local State = Drive State.Off;\n";
        let on_member = hover_value_at(&project, src, "Off", 0);
        assert!(
            on_member.contains("Drive State") && on_member.contains("Off"),
            "enum-member hover should name the enum and member: {on_member}"
        );
        assert!(
            on_member.to_lowercase().contains("enum member"),
            "enum-member hover should label it an enum member: {on_member}"
        );
        assert!(
            on_member.contains("= 0"),
            "enum-member hover should show the member's value: {on_member}"
        );
        assert!(
            !on_member.contains("type not modelled"),
            "enum-member hover must not fall back to the not-modelled message: {on_member}"
        );
    }

    /// `Status = ASSI.Driving;` — the *head* of an `EnumName.Member` literal is
    /// the enum type itself. Hovering it must describe the enum (name + values),
    /// not fall back to "type not modelled". The enum name is not a channel, so it
    /// only ever resolves Opaque; this is the AV-M1 `ASSI.Driving` case. The member
    /// `Driving` and the enum-typed LHS already hover correctly — only the head was
    /// broken.
    #[test]
    fn hover_on_enum_type_head_names_the_enum() {
        use std::io::Write;
        let tmp = tempfile::tempdir().unwrap();
        let prj = tmp.path().join("Project.m1prj");
        std::fs::File::create(&prj)
            .unwrap()
            .write_all(
                br#"<?xml version="1.0"?>
<Project>
  <DataTypes>
    <Type Name="Gear State" Storage="enum" Default="Neutral">
      <Enum Name="Neutral" ContainerOrder="0"/>
      <Enum Name="Driving" ContainerOrder="1"/>
    </Type>
  </DataTypes>
  <Component Classname="BuiltIn.GroupCompound" Name="Root"/>
  <Component Classname="BuiltIn.GroupCompound" Name="Root.Control"/>
  <Component Classname="BuiltIn.Channel" Name="Root.Control.Status"><Props Type="::This.Gear State"/></Component>
</Project>"#,
            )
            .unwrap();
        let project = m1_typecheck::Project::load(&prj).unwrap();
        let src = "Status = Gear State.Driving;\n";
        // Hover the enum-type head `Gear State`, not the `Driving` member.
        let on_head = hover_value_at(&project, src, "Gear State", 0);
        assert!(
            on_head.contains("Gear State") && on_head.to_lowercase().contains("enum"),
            "enum-type head hover should name the enum type: {on_head}"
        );
        assert!(
            on_head.contains("Driving") && on_head.contains("Neutral"),
            "enum-type head hover should list the members: {on_head}"
        );
        assert!(
            !on_head.contains("type not modelled"),
            "enum-type head must not fall back to the not-modelled message: {on_head}"
        );
    }

    /// The real AV-M1 case: the enum type `ASSI` shares its name with its
    /// enclosing group `Root.Control.AV.ASSI`. Hovering `ASSI` in `ASSI.Driving`
    /// must describe the *enum* (because the next segment `Driving` is one of its
    /// members), not the shadowing group — which group-relative resolution would
    /// otherwise return as `group / type: Unknown`.
    #[test]
    fn hover_on_enum_head_that_shadows_a_group_names_the_enum() {
        use std::io::Write;
        let tmp = tempfile::tempdir().unwrap();
        let prj = tmp.path().join("Project.m1prj");
        std::fs::File::create(&prj)
            .unwrap()
            .write_all(
                br#"<?xml version="1.0"?>
<Project>
  <DataTypes>
    <Type Name="ASSI" Storage="enum" Default="Off">
      <Enum Name="Off" ContainerOrder="0"/>
      <Enum Name="Driving" ContainerOrder="1"/>
    </Type>
  </DataTypes>
  <Component Classname="BuiltIn.GroupCompound" Name="Root"/>
  <Component Classname="BuiltIn.GroupCompound" Name="Root.Control"/>
  <Component Classname="BuiltIn.GroupCompound" Name="Root.Control.ASSI"/>
  <Component Classname="BuiltIn.Channel" Name="Root.Control.ASSI.Status"><Props Type="::This.ASSI"/></Component>
  <Component Classname="BuiltIn.MethodUser" Name="Root.Control.ASSI.Update"/>
</Project>"#,
            )
            .unwrap();
        let project = m1_typecheck::Project::load(&prj).unwrap();
        let src = "Status = ASSI.Driving;\n";
        let cst = m1_core::parse(src);
        let li = LineIndex::new(src);
        let byte = src.find("ASSI").unwrap();
        // The script lives in the `Root.Control.ASSI` group — so a bare `ASSI`
        // resolves group-relatively to that group unless we recognise the literal.
        let h = hover(
            cst.root(),
            byte,
            Some(&project),
            Some("Control.ASSI.Update.m1scr"),
            &li,
            PositionEncoding::Utf16,
        )
        .unwrap();
        let HoverContents::Markup(m) = h.contents else {
            panic!("expected markup")
        };
        assert!(
            m.value.to_lowercase().contains("enum") && m.value.contains("Driving"),
            "ASSI head should describe the enum, not the shadowing group: {}",
            m.value
        );
        assert!(
            !m.value.contains("group"),
            "ASSI head must not resolve to the shadowing group: {}",
            m.value
        );
    }

    /// #163: with the enum/group name collision, a *misspelled* member must still
    /// produce the enum hover (with the valid member list) rather than falling
    /// back to the shadowing group — that is the most useful moment to show it.
    #[test]
    fn hover_on_enum_head_with_typoed_member_still_names_the_enum() {
        use std::io::Write;
        let tmp = tempfile::tempdir().unwrap();
        let prj = tmp.path().join("Project.m1prj");
        std::fs::File::create(&prj)
            .unwrap()
            .write_all(
                br#"<?xml version="1.0"?>
<Project>
  <DataTypes>
    <Type Name="ASSI" Storage="enum" Default="Off">
      <Enum Name="Off" ContainerOrder="0"/>
      <Enum Name="Driving" ContainerOrder="1"/>
    </Type>
  </DataTypes>
  <Component Classname="BuiltIn.GroupCompound" Name="Root"/>
  <Component Classname="BuiltIn.GroupCompound" Name="Root.Control"/>
  <Component Classname="BuiltIn.GroupCompound" Name="Root.Control.ASSI"/>
  <Component Classname="BuiltIn.Channel" Name="Root.Control.ASSI.Status"><Props Type="::This.ASSI"/></Component>
  <Component Classname="BuiltIn.MethodUser" Name="Root.Control.ASSI.Update"/>
</Project>"#,
            )
            .unwrap();
        let project = m1_typecheck::Project::load(&prj).unwrap();
        // `Drivng` is a typo of the member `Driving` — not a member, and
        // `ASSI.Drivng` does not resolve to any symbol.
        let src = "Status = ASSI.Drivng;\n";
        let cst = m1_core::parse(src);
        let li = LineIndex::new(src);
        let byte = src.find("ASSI").unwrap();
        let h = hover(
            cst.root(),
            byte,
            Some(&project),
            Some("Control.ASSI.Update.m1scr"),
            &li,
            PositionEncoding::Utf16,
        )
        .unwrap();
        let HoverContents::Markup(m) = h.contents else {
            panic!("expected markup")
        };
        assert!(
            m.value.to_lowercase().contains("enum") && m.value.contains("Driving"),
            "typoed-member head should still describe the enum + members: {}",
            m.value
        );
        assert!(
            m.value.contains("not a member"),
            "should flag the bad member: {}",
            m.value
        );
        assert!(
            !m.value.contains("`group`"),
            "must not fall back to the shadowing group: {}",
            m.value
        );
    }

    /// A genuine group-relative path that shares an enum name (`ASSI.Status`, where
    /// `Status` is a real child channel, not an enum member) must NOT be hijacked
    /// by the enum hover — it should resolve to the channel.
    #[test]
    fn hover_on_group_name_with_real_child_is_not_hijacked_by_enum() {
        use std::io::Write;
        let tmp = tempfile::tempdir().unwrap();
        let prj = tmp.path().join("Project.m1prj");
        std::fs::File::create(&prj)
            .unwrap()
            .write_all(
                br#"<?xml version="1.0"?>
<Project>
  <DataTypes>
    <Type Name="ASSI" Storage="enum" Default="Off">
      <Enum Name="Off" ContainerOrder="0"/>
      <Enum Name="Driving" ContainerOrder="1"/>
    </Type>
  </DataTypes>
  <Component Classname="BuiltIn.GroupCompound" Name="Root"/>
  <Component Classname="BuiltIn.GroupCompound" Name="Root.Control"/>
  <Component Classname="BuiltIn.GroupCompound" Name="Root.Control.ASSI"/>
  <Component Classname="BuiltIn.Channel" Name="Root.Control.ASSI.Status"><Props Type="u8"/></Component>
  <Component Classname="BuiltIn.MethodUser" Name="Root.Control.ASSI.Update"/>
</Project>"#,
            )
            .unwrap();
        let project = m1_typecheck::Project::load(&prj).unwrap();
        // `ASSI.Status` is a real path (the channel) — `Status` is not an enum
        // member, so the enum hover must not hijack it.
        let src = "x = Control.ASSI.Status;\n";
        let cst = m1_core::parse(src);
        let li = LineIndex::new(src);
        let byte = src.find("Status").unwrap();
        let h = hover(
            cst.root(),
            byte,
            Some(&project),
            Some("Other.Update.m1scr"),
            &li,
            PositionEncoding::Utf16,
        )
        .unwrap();
        let HoverContents::Markup(m) = h.contents else {
            panic!("expected markup")
        };
        assert!(
            !m.value.contains("not a member"),
            "a real group-relative path must not get the enum note: {}",
            m.value
        );
    }

    #[test]
    fn calibration_only_function_hover_is_labelled() {
        // Math.* are calibration-method-only; hover should resolve them but flag
        // that they're not valid in ECU scripts.
        let src = "x = Math.Sqrt(a);\n";
        let cst = m1_core::parse(src);
        let li = LineIndex::new(src);
        let byte = src.find("Sqrt").unwrap();
        let h = hover(cst.root(), byte, None, None, &li, PositionEncoding::Utf16).unwrap();
        if let HoverContents::Markup(m) = h.contents {
            assert!(m.value.contains("Math.Sqrt"), "{}", m.value);
            assert!(
                m.value.to_lowercase().contains("calibration"),
                "should label calibration-only: {}",
                m.value
            );
        } else {
            panic!("expected markup");
        }
    }
}
