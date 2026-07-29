//! Rendering for a resolved project symbol: the header/kind badge, value-type
//! and unit/security/rate/tag badges, object class, table shape, and CAN/DBC
//! signal layout. The seam consumed by [`super::hover_with_eval`] (project-symbol
//! branch) and by `completion`/`inlay` (via the re-exported `symbol_markdown` /
//! `value_type_str`).
use m1_typecheck::project::Project;
use m1_typecheck::symbols::{Symbol, SymbolKind, TableMeta};
use m1_typecheck::types::ValueType;

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
        Some(class) => {
            let mut s = format!("class: `{class}`");
            // Help summary from the M1 Build help-capture catalogue, matched
            // on the full class name or its leaf ("MoTeC Input.Sensor" →
            // "Sensor"). Internal spellings (`_IOMethod.*`) have no capture
            // and stay summary-less.
            let intr = m1_typecheck::intrinsics::get();
            if let Some(doc) = intr.class_doc(class).or_else(|| {
                class
                    .split_once('.')
                    .and_then(|(_, leaf)| intr.class_doc(leaf))
            }) {
                s.push_str(&format!("\n\n{doc}"));
            }
            s
        }
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

pub(crate) fn symbol_markdown(sym: &Symbol, project: Option<&Project>) -> String {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[allow(clippy::field_reassign_with_default)]
    fn channel(value_type: ValueType, declared_type: Option<&str>) -> Symbol {
        Symbol {
            path: "Root.Demo.X".into(),
            static_value: None,
            reference_target: None,
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
            scheduled: false,
            log_rate_hz: None,
            tags: Vec::new(),
            return_type: None,
            in_params: None,
            table_meta: None,
            default_value: None,
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
            static_value: None,
            reference_target: None,
            kind: SymbolKind::Channel,
            value_type: ValueType::Float,
            declared_type: None,
            unit: Some("%".into()),
            qty: Some("ratio".into()),
            display_unit: Some("%".into()),
            security: Some("Protected".into()),
            filename: None,
            enum_assoc: None,
            class: None,
            classname: None,
            def_line: None,
            dbc_range: None,
            can: None,
            call_rate_hz: None,
            scheduled: false,
            log_rate_hz: None,
            tags: Vec::new(),
            return_type: None,
            in_params: None,
            table_meta: None,
            default_value: None,
        };
        let md = symbol_markdown(&sym, None);
        assert!(md.contains("security: `Protected`"), "got: {md}");
    }

    #[test]
    fn hover_shows_script_call_rate() {
        let sym = Symbol {
            path: "Root.Engine.Control".into(),
            static_value: None,
            reference_target: None,
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
            scheduled: false,
            log_rate_hz: None,
            tags: Vec::new(),
            return_type: None,
            in_params: None,
            table_meta: None,
            default_value: None,
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
    fn object_hover_includes_class_help_summary() {
        // Capture docs match on the class leaf: "MoTeC Input.Sensor" → "Sensor".
        let mut sym = channel(ValueType::Unknown, None);
        sym.kind = SymbolKind::Object;
        sym.class = Some("MoTeC Input.Sensor".into());
        let md = object_markdown(&sym).unwrap();
        assert!(md.contains("class: `MoTeC Input.Sensor`"), "got: {md}");
        assert!(md.contains("require calibration"), "got: {md}");
    }

    #[test]
    fn object_hover_without_capture_doc_shows_class_only() {
        // Internal spellings (`_IOMethod.*`) have no capture entry.
        let mut sym = channel(ValueType::Unknown, None);
        sym.kind = SymbolKind::Object;
        sym.class = Some("_IOMethod.av_switch".into());
        let md = object_markdown(&sym).unwrap();
        assert!(md.contains("class: `_IOMethod.av_switch`"), "got: {md}");
        assert!(!md.contains("\n\n"), "no summary expected: {md}");
    }

    #[test]
    fn hover_shows_table_shape() {
        use m1_typecheck::symbols::TableAxis;
        let sym = Symbol {
            path: "Root.Control.Limiting.Torque".into(),
            static_value: None,
            reference_target: None,
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
            scheduled: false,
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
            default_value: None,
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
            static_value: None,
            reference_target: None,
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
                transmit: None,
                start_bit: Some(10),
                length: Some(10),
                multiplier: Some(0.1),
                offset: Some(0.0),
            }),
            call_rate_hz: None,
            scheduled: false,
            log_rate_hz: None,
            tags: Vec::new(),
            return_type: None,
            in_params: None,
            table_meta: None,
            default_value: None,
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
}
