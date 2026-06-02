//! textDocument/hover: describe the symbol/local/opaque under the cursor.
use crate::convert::range;
use crate::features::locate::{build_scope, path_at_byte};
use crate::line_index::{LineIndex, PositionEncoding};
use m1_typecheck::project::Project;
use m1_typecheck::resolve::{Resolution, resolve};
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

fn symbol_markdown(sym: &Symbol, project: Option<&Project>) -> String {
    let mut s = format!("**{}** `{}`\n\n", sym.path, kind_str(sym.kind));
    // For objects, show the package class instead of a (meaningless) value type.
    if sym.kind == SymbolKind::Object {
        match &sym.class {
            Some(class) => s.push_str(&format!("class: `{class}`")),
            None => s.push_str("object"),
        }
        // A CAN message object carries the frame's id + payload size (#80).
        if let Some(can) = &sym.can
            && let (Some(id), Some(dlc)) = (can.can_id, can.dlc)
        {
            s.push_str(&format!("\n\nCAN id: `0x{id:X}`  ·  `{dlc}` bytes"));
        }
        return s;
    }
    // A table's own hover shows its shape (from the `.m1cfg`), not a value type —
    // the table object isn't value-bearing; its interpolated result is the
    // separate `.Value` channel (#25).
    if sym.kind == SymbolKind::Table
        && let Some(meta) = &sym.table_meta
    {
        return format!("{s}{}", table_markdown(meta));
    }
    // Name the concrete enum type when known (e.g. `Enum (Drive State)`), and
    // collect its valid values to list below.
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
    s.push_str(&format!("type: `{type_str}`"));
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
    }
    s
}

/// Render hover for the path at `byte`. `project`/`file_name` drive resolution.
pub fn hover(
    root: m1_core::Node,
    byte: usize,
    project: Option<&Project>,
    file_name: Option<&str>,
    li: &LineIndex,
    enc: PositionEncoding,
) -> Option<Hover> {
    let (node, path) = path_at_byte(root, byte)?;
    let scope = build_scope(root, project, file_name);
    let md = match resolve(&path, &scope) {
        Resolution::Local(t) => format!("**{path}** `local`\n\ntype: `{}`", value_type_str(t)),
        Resolution::Symbol(sym) => symbol_markdown(sym, project),
        Resolution::BuiltinObject(name) => builtin_object_markdown(name),
        Resolution::BuiltinFn(overloads) => builtin_fn_markdown(&path, &overloads),
        Resolution::Opaque => format!("**{path}**\n\nbuilt-in symbol — type not modelled"),
        Resolution::Unresolved => return None,
    };
    Some(Hover {
        contents: HoverContents::Markup(MarkupContent {
            kind: MarkupKind::Markdown,
            value: md,
        }),
        range: Some(range(&node.byte_range(), li, enc)),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn hover_shows_security_level() {
        let sym = Symbol {
            path: "Root.Engine.Throttle".into(),
            kind: SymbolKind::Channel,
            value_type: ValueType::Float,
            declared_type: None,
            unit: Some("%".into()),
            security: Some("Protected".into()),
            filename: None,
            enum_assoc: None,
            class: None,
            def_line: None,
            dbc_range: None,
            can: None,
            call_rate_hz: None,
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
            security: None,
            filename: None,
            enum_assoc: None,
            class: None,
            def_line: None,
            dbc_range: None,
            can: None,
            call_rate_hz: Some(100.0),
            table_meta: None,
        };
        let md = symbol_markdown(&sym, None);
        assert!(md.contains("call rate: `100 Hz`"), "got: {md}");
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
            security: None,
            filename: None,
            enum_assoc: None,
            class: None,
            def_line: None,
            dbc_range: None,
            can: None,
            call_rate_hz: None,
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
            security: None,
            filename: Some("dbc/SBG DBC.m1dbc".into()),
            enum_assoc: None,
            class: None,
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
}
