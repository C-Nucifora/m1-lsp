//! textDocument/completion: library methods after `.`, else in-scope locals +
//! project symbols + library objects + keywords.
use crate::features::locate::collect_locals;
use crate::project_store::LoadedProject;
use m1_core::Node;
use m1_typecheck::project::Project;
use std::collections::HashSet;
use tower_lsp::lsp_types::{
    CompletionItem, CompletionItemKind, CompletionTextEdit, Documentation, InsertTextFormat, Range,
    TextEdit,
};

/// The construct skeleton inserted when an M1 control-flow / declaration keyword
/// is accepted from completion — Allman braces + tabs, with `${n}` tab stops and
/// a final `$0` cursor (#173). `None` for keywords that are not construct heads.
fn construct_snippet(kw: &str) -> Option<&'static str> {
    Some(match kw {
        "if" => "if (${1:condition})\n{\n\t$0\n}",
        "when" => "when (${1:subject})\n{\n\tis (${2:Value})\n\t{\n\t\t$0\n\t}\n}",
        "expand" => "expand (${1:i} = ${2:0} to ${3:count})\n{\n\t$0\n}",
        "local" => "local ${1:name} = ${2:0};$0",
        "static" => "static local ${1:name} = ${2:0};$0",
        _ => return None,
    })
}

/// A `${N:param}` snippet body for a function call, e.g.
/// `Max(${1:a}, ${2:b})`, so the client tabs through the argument
/// placeholders (#28). No-arg functions insert `Name()`.
fn call_snippet(name: &str, params: &[m1_typecheck::intrinsics::Param]) -> String {
    if params.is_empty() {
        return format!("{name}()");
    }
    let args: Vec<String> = params
        .iter()
        .enumerate()
        .map(|(i, p)| format!("${{{}:{}}}", i + 1, p.name))
        .collect();
    format!("{name}({})", args.join(", "))
}

/// A human-readable signature for the completion `detail`, e.g. `(a, b) -> Float`.
fn signature_detail(params: &[m1_typecheck::intrinsics::Param], returns: &str) -> String {
    let names: Vec<&str> = params.iter().map(|p| p.name.as_str()).collect();
    format!("({}) -> {returns}", names.join(", "))
}

/// The value type (and unit) of a project symbol, for the completion `detail`,
/// e.g. `Unsigned · ratio` or `Enum (Drive State)`. Most of this comes from the
/// project's `parameters.m1cfg`; returns `None` when the type is unknown (groups,
/// untyped names) so we don't show noise like `Unknown`.
fn type_unit_detail(sym: &m1_typecheck::symbols::Symbol, project: &Project) -> Option<String> {
    use m1_typecheck::types::ValueType;
    let ty = match sym.value_type {
        ValueType::Enum(id) => format!("Enum ({})", project.symbols().enum_type(id).name),
        other if other.is_known() => super::hover::value_type_str(other).to_string(),
        _ => return None,
    };
    let mut detail = match &sym.unit {
        Some(u) => format!("{ty} · {u}"),
        None => ty,
    };
    // Security / access level from the `.m1prj` `<Props Security>` (#77).
    if let Some(sec) = &sym.security {
        detail.push_str(&format!(" · {sec}"));
    }
    Some(detail)
}

/// If `byte` sits on the right-hand side of an `lhs = …` on the current line,
/// the trimmed `lhs` text; else `None`. Text-based (consistent with the after-`.`
/// member detection) — comparison operators (`==`, `<=`, `>=`, `!=`) are excluded.
fn assignment_lhs(text: &str, byte: usize) -> Option<String> {
    let line_start = text[..byte].rfind('\n').map_or(0, |i| i + 1);
    let before = &text[line_start..byte];
    let eq = before.rfind('=')?;
    // Reject `==`, `<=`, `>=`, `!=` (a comparison, not an assignment).
    if before[eq + 1..].starts_with('=')
        || matches!(
            before[..eq].trim_end().chars().last(),
            Some('=' | '<' | '>' | '!')
        )
    {
        return None;
    }
    let lhs = before[..eq].trim();
    (!lhs.is_empty()).then(|| lhs.to_string())
}

/// Enum-member completions for an enum-typed LHS symbol, e.g. `GearState.Neutral`
/// with detail `= 0`. `None` when `lhs` doesn't resolve to an enum-typed symbol.
///
/// Resolution goes through `m1_typecheck::resolve`, which tries the LHS absolute,
/// `Root.`-prefixed, *and* group-relatively (walking the enclosing `group` up to
/// Root). This is what makes the universal real pattern `<bare channel> = …` work
/// — `Value` in the `Root.Control` group resolves to `Root.Control.Value` (#126);
/// a fully-qualified LHS still resolves via the absolute step.
fn enum_member_completions(
    lhs: &str,
    group: Option<String>,
    lp: &LoadedProject,
) -> Option<Vec<CompletionItem>> {
    use m1_typecheck::resolve::{Resolution, Scope, resolve};
    use m1_typecheck::types::ValueType;
    let table = lp.project.symbols();
    let scope = Scope {
        locals: std::collections::HashMap::new(),
        group,
        project: Some(&lp.project),
        fn_symbol: None,
    };
    let Resolution::Symbol(sym) = resolve(lhs, &scope) else {
        return None;
    };
    let ValueType::Enum(id) = sym.value_type else {
        return None;
    };
    let et = table.enum_type(id);
    Some(
        et.members
            .iter()
            .map(|(name, value)| CompletionItem {
                label: format!("{}.{}", et.name, name),
                kind: Some(CompletionItemKind::ENUM_MEMBER),
                detail: Some(format!("= {value}")),
                ..Default::default()
            })
            .collect(),
    )
}

/// The dotted parent path immediately before the cursor's `.`, e.g. with the
/// cursor after `Calculate.` -> `Some("Calculate")`. Library object names have
/// no spaces, so we scan back over an identifier/dot run.
fn member_parent(text: &str, byte: usize) -> Option<String> {
    let chain = &text[chain_start(text, byte)..byte.min(text.len())];
    let dot = chain.rfind('.')?;
    Some(chain[..dot].to_string())
}

/// The dotted parent path before the cursor's `.`, allowing spaces *within*
/// segments — DBC message/signal names commonly contain spaces (`Demo Frame`),
/// unlike library objects. Scans back over path characters (alphanumerics, `_`,
/// `.`, space), stopping at the first token that can't be part of an M1 path
/// (`=`, `(`, an operator, line start), then drops the trailing leaf. `None` when
/// there is no `.` in the run. Used only to detect a DBC-message parent (#169).
fn member_parent_with_spaces(text: &str, byte: usize) -> Option<String> {
    let before = &text[..byte.min(text.len())];
    let start = before
        .rfind(|c: char| !(c.is_alphanumeric() || c == '_' || c == '.' || c == ' '))
        .map(|i| i + 1)
        .unwrap_or(0);
    let chain = before[start..].trim_start();
    let dot = chain.rfind('.')?;
    let parent = chain[..dot].trim();
    (!parent.is_empty()).then(|| parent.to_string())
}

/// Byte offset where the identifier/dot run ending at `byte` begins — i.e. the
/// start of the dotted path the user is currently typing (`Control.AV.` →
/// the `C`). Project-symbol completions replace from here so accepting a path
/// can't append after the prefix and duplicate it (#…). Same scan as
/// `member_parent`: stops at the first char that can't be part of a path token.
fn chain_start(text: &str, byte: usize) -> usize {
    let before = &text[..byte.min(text.len())];
    before
        .rfind(|c: char| !(c.is_alphanumeric() || c == '_' || c == '.'))
        .map(|i| i + 1)
        .unwrap_or(0)
}

/// Byte offset where the identifier/dot run *containing or starting at* `byte`
/// ends — the forward twin of [`chain_start`]. Scans FORWARD from `byte` over
/// path characters (alphanumerics, `_`, `.`) to the end of the dotted run.
/// Completing in the *middle* of a chain (`Driveline.A|ccumulator.Voltage`)
/// must replace the whole token, not just `chain_start..byte`, or accepting an
/// item leaves the orphaned suffix `ccumulator.Voltage` after the inserted text
/// (#…). At end-of-chain `chain_end == byte`, so the common case is unchanged.
fn chain_end(text: &str, byte: usize) -> usize {
    let byte = byte.min(text.len());
    text[byte..]
        .find(|c: char| !(c.is_alphanumeric() || c == '_' || c == '.'))
        .map(|i| byte + i)
        .unwrap_or(text.len())
}

/// `true` when `byte` sits inside a comment or string-literal node, where
/// code completion is meaningless and would otherwise dump the whole library /
/// keyword / symbol set into prose (#…). Walks `root`'s descendants for the
/// smallest node whose byte range contains `byte` and checks its kind.
///
/// The boundary right at the *end* of a token is treated as outside the token
/// (`start <= byte < end`), so completion still fires when the cursor sits just
/// past a closing `"` or at the line break after a `//` comment.
fn in_comment_or_string(root: Node, byte: usize) -> bool {
    use m1_core::Kind;
    let mut best: Option<Node> = None;
    for n in root.descendants() {
        let r = n.byte_range();
        if r.start <= byte && byte < r.end {
            // Prefer the smallest (innermost) containing node.
            match &best {
                Some(b) if (b.byte_range().end - b.byte_range().start) <= (r.end - r.start) => {}
                _ => best = Some(n),
            }
        }
    }
    matches!(
        best.map(|n| n.kind()),
        Some(Kind::LineComment | Kind::BlockComment | Kind::String | Kind::Interpolation)
    )
}

/// A project-symbol completion item that *replaces* the typed dotted chain
/// (`range`) with `text` — both the visible label and the inserted text. Setting
/// an explicit edit + filter is what stops the client appending the path after
/// the already-typed prefix.
fn path_item(text: &str, range: Range, detail: Option<String>) -> CompletionItem {
    CompletionItem {
        label: text.to_string(),
        kind: Some(CompletionItemKind::FIELD),
        detail,
        // Rank project symbols (1) below in-scope locals (0) but above library
        // objects (2) and keywords (3) — see `completions` (#146).
        sort_text: Some(format!("1{text}")),
        filter_text: Some(text.to_string()),
        text_edit: Some(CompletionTextEdit::Edit(TextEdit {
            range,
            new_text: text.to_string(),
        })),
        ..Default::default()
    }
}

/// Completion items for the document. `text`/`byte` give the cursor context so a
/// `.` after a library object completes that object's methods.
pub fn completions(
    root: Node,
    loaded: Option<&LoadedProject>,
    file_name: Option<&str>,
    text: &str,
    byte: usize,
    li: &crate::line_index::LineIndex,
    enc: crate::line_index::PositionEncoding,
) -> Vec<CompletionItem> {
    // Inside a comment or string literal, code completion is noise — bail out
    // with an empty list rather than dumping objects/keywords/locals (#…).
    if in_comment_or_string(root, byte) {
        return Vec::new();
    }

    let intr = m1_typecheck::intrinsics::get();

    // After `Object.` where Object is a library object: just its methods.
    if let Some(parent) = member_parent(text, byte)
        && let Some(obj) = intr.library_object(&parent)
    {
        let mut seen = HashSet::new();
        return obj
            .functions
            .iter()
            // Calibration-only functions (Math.*, UI.*, System.StrCat) are valid
            // only in M1 Tune calibration methods, never in ECU .m1scr scripts.
            .filter(|f| !f.calibration_only)
            .filter(|f| seen.insert(f.name.clone()))
            .map(|f| CompletionItem {
                label: f.name.clone(),
                kind: Some(CompletionItemKind::METHOD),
                detail: Some(signature_detail(&f.params, &f.returns)),
                documentation: (!f.doc.is_empty()).then(|| Documentation::String(f.doc.clone())),
                insert_text: Some(call_snippet(&f.name, &f.params)),
                insert_text_format: Some(InsertTextFormat::SNIPPET),
                ..Default::default()
            })
            .collect();
    }

    // After `Bus.Frame.` where `Bus.Frame` is a DBC CAN message object: offer the
    // frame's signals (its immediate children) as members (#169). DBC paths carry
    // spaces, so the parent scan must allow them. The message symbol is looked up
    // directly (DBC symbols are not `Root.`-prefixed), with a Root-prefixed
    // fallback for safety.
    if let Some(lp) = loaded
        && let Some(parent) = member_parent_with_spaces(text, byte)
    {
        let table = lp.project.symbols();
        if let Some(msg) = table
            .get(&parent)
            .or_else(|| table.get(&format!("Root.{parent}")))
            && msg
                .classname
                .as_deref()
                .is_some_and(|c| c.starts_with("BuiltIn.CAN.Message"))
        {
            return table
                .immediate_children(&msg.path)
                .into_iter()
                .filter_map(|sig| {
                    let leaf = sig.path.rsplit_once('.').map(|(_, l)| l)?;
                    Some(CompletionItem {
                        label: leaf.to_string(),
                        kind: Some(CompletionItemKind::FIELD),
                        detail: type_unit_detail(sig, &lp.project),
                        ..Default::default()
                    })
                })
                .collect();
        }
    }

    // On the RHS of `enumChannel = <cursor>`: offer that channel's enum members
    // (e.g. `GearState.Neutral` … with their integer values) instead of generic
    // symbols (#79).
    if let Some(lp) = loaded
        && let Some(lhs) = assignment_lhs(text, byte)
        && let Some(items) = enum_member_completions(
            &lhs,
            file_name.and_then(|f| lp.project.group_for_script(f)),
            lp,
        )
    {
        return items;
    }

    let mut items: Vec<CompletionItem> = Vec::new();

    // Library objects (Calculate, CanComms, …) and keywords are always offered.
    // Skip calibration-only objects (Math, UI) whose every function is valid only
    // in M1 Tune calibration methods, not in ECU .m1scr scripts.
    for (name, obj) in intr.library.iter() {
        if !obj.functions.is_empty() && obj.functions.iter().all(|f| f.calibration_only) {
            continue;
        }
        items.push(CompletionItem {
            label: name.clone(),
            kind: Some(CompletionItemKind::MODULE),
            detail: Some("library object".into()),
            sort_text: Some(format!("2{name}")),
            ..Default::default()
        });
    }
    for words in intr.language.keywords.values() {
        for kw in words {
            // Control-flow / declaration keywords expand to a full construct
            // skeleton with tab stops; plain keywords insert their text (#173).
            let snippet = construct_snippet(kw);
            items.push(CompletionItem {
                label: kw.clone(),
                kind: Some(CompletionItemKind::KEYWORD),
                sort_text: Some(format!("3{kw}")),
                insert_text: snippet.map(str::to_string),
                insert_text_format: snippet.map(|_| InsertTextFormat::SNIPPET),
                ..Default::default()
            });
        }
    }

    // 1. In-scope locals — ranked first (sort_text `0…`) so they aren't buried
    //    among the project's many symbols (#146).
    for (name, _ty) in collect_locals(root) {
        items.push(CompletionItem {
            sort_text: Some(format!("0{name}")),
            label: name,
            kind: Some(CompletionItemKind::VARIABLE),
            ..Default::default()
        });
    }

    // 2. Project symbols: the `Root.`-stripped path (the idiomatic bare reference
    //    — the resolver Root-prefixes on lookup — and exactly what the user types),
    //    plus the group-relative tail for this file. Each item carries a text_edit
    //    that replaces the dotted chain under the cursor, so accepting `Control.AV.`
    //    yields `Control.AV.DFMM.Checkup`, never the duplicated
    //    `Control.AV.Root.Control.AV.DFMM.Checkup`.
    if let Some(lp) = loaded {
        let group = file_name.and_then(|f| lp.project.group_for_script(f));
        // Replace the WHOLE dotted chain under the cursor — head to tail — so
        // completing in the middle (`Driveline.A|ccumulator.Voltage`) doesn't
        // leave the orphaned suffix after the inserted path (#…).
        let edit_range =
            crate::convert::range(&(chain_start(text, byte)..chain_end(text, byte)), li, enc);
        for sym in lp.project.symbols().iter() {
            let ty = type_unit_detail(sym, &lp.project);
            let rel = sym.path.strip_prefix("Root.").unwrap_or(&sym.path);
            if rel.is_empty() {
                continue; // the bare `Root` group — nothing to reference
            }
            items.push(path_item(rel, edit_range, ty.clone()));
            if let Some(g) = &group
                && let Some(tail) = sym.path.strip_prefix(&format!("{g}."))
            {
                // The short tail hides the full path, so put the path in the
                // detail, plus the type/unit when known.
                let detail = match &ty {
                    Some(t) => format!("{rel}  ·  {t}"),
                    None => rel.to_string(),
                };
                items.push(path_item(tail, edit_range, Some(detail)));
            }
        }
    }
    items
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::project_store::ProjectStore;
    use std::io::Write;

    const M1PRJ: &str = r#"<?xml version="1.0"?>
<Project>
  <Component Classname="BuiltIn.GroupCompound" Name="Root"/>
  <Component Classname="BuiltIn.ChannelMeasure" Name="Root.Speed Glonk"/>
</Project>"#;

    #[test]
    fn includes_locals_and_project_symbols() {
        let tmp = tempfile::tempdir().unwrap();
        let prj = tmp.path().join("Project.m1prj");
        std::fs::File::create(&prj)
            .unwrap()
            .write_all(M1PRJ.as_bytes())
            .unwrap();
        let store = ProjectStore::new();
        store.discover_and_load(tmp.path()).unwrap();

        let src = "local fGain = 1.0;\n";
        let cst = m1_core::parse(src);
        store.with_project(|p| {
            let items = completions(
                cst.root(),
                p,
                Some("X.m1scr"),
                src,
                src.len(),
                &crate::line_index::LineIndex::new(src),
                crate::line_index::PositionEncoding::Utf16,
            );
            let labels: Vec<_> = items.iter().map(|i| i.label.as_str()).collect();
            assert!(labels.contains(&"fGain"));
            assert!(labels.iter().any(|l| l.contains("Speed Glonk")));
        });
    }

    #[test]
    fn in_scope_locals_rank_before_project_symbols_and_keywords() {
        // #146: sortText must float in-scope locals to the top of the otherwise
        // huge flat list (locals < project symbols < library objects < keywords).
        let tmp = tempfile::tempdir().unwrap();
        std::fs::File::create(tmp.path().join("Project.m1prj"))
            .unwrap()
            .write_all(M1PRJ.as_bytes())
            .unwrap();
        let store = ProjectStore::new();
        store.discover_and_load(tmp.path()).unwrap();
        let src = "local fGain = 1.0;\n";
        let cst = m1_core::parse(src);
        store.with_project(|p| {
            let items = completions(
                cst.root(),
                p,
                Some("X.m1scr"),
                src,
                src.len(),
                &crate::line_index::LineIndex::new(src),
                crate::line_index::PositionEncoding::Utf16,
            );
            let st = |label_pred: &dyn Fn(&CompletionItem) -> bool| -> String {
                items
                    .iter()
                    .find(|i| label_pred(i))
                    .and_then(|i| i.sort_text.clone())
                    .expect("item with sort_text")
            };
            let local = st(&|i| i.label == "fGain");
            let sym = st(&|i| i.label.contains("Speed Glonk"));
            let kw = st(&|i| i.kind == Some(CompletionItemKind::KEYWORD));
            assert!(
                local < sym,
                "local {local:?} should rank before symbol {sym:?}"
            );
            assert!(sym < kw, "symbol {sym:?} should rank before keyword {kw:?}");
        });
    }

    // #169 gap 3: after a DBC message object's dot, completion offers the
    // message's signals (its immediate children) as members.
    #[test]
    fn dbc_message_dot_completion_offers_its_signals() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::File::create(tmp.path().join("Project.m1prj"))
            .unwrap()
            .write_all(
                br#"<?xml version="1.0"?>
<Project>
  <Component Classname="BuiltIn.GroupCompound" Name="Root"/>
</Project>"#,
            )
            .unwrap();
        let dbc_dir = tmp.path().join("dbc");
        std::fs::create_dir_all(&dbc_dir).unwrap();
        std::fs::write(
            dbc_dir.join("Sensors.m1dbc"),
            r#"<?xml version="1.0"?>
<DBC>
 <ComponentStream><List>
  <Component Classname="BuiltIn.CAN.DBC" Name="Sensors"/>
  <Component Classname="BuiltIn.CAN.Message" Name="Sensors.Demo Frame"><Props CANId="100" DLC="4"/></Component>
  <Component Classname="BuiltIn.CAN.Signal" Name="Sensors.Demo Frame.Widget Count"><Props Type="u16" Qty="count" StartBit="0" Length="16"/></Component>
  <Component Classname="BuiltIn.CAN.Signal" Name="Sensors.Demo Frame.Mode Raw"><Props Type="u16" StartBit="16" Length="16"/></Component>
 </List></ComponentStream>
</DBC>"#,
        )
        .unwrap();
        let store = ProjectStore::new();
        store.discover_and_load(tmp.path()).unwrap();

        let src = "local v = Sensors.Demo Frame.\n";
        let byte = src.find("Frame.").unwrap() + "Frame.".len();
        let cst = m1_core::parse(src);
        store.with_project(|p| {
            let items = completions(
                cst.root(),
                p,
                Some("Read.m1scr"),
                src,
                byte,
                &crate::line_index::LineIndex::new(src),
                crate::line_index::PositionEncoding::Utf16,
            );
            let labels: Vec<_> = items.iter().map(|i| i.label.as_str()).collect();
            assert!(
                labels.contains(&"Widget Count") && labels.contains(&"Mode Raw"),
                "DBC message dot-completion should offer its signals; got {labels:?}"
            );
            // Only the frame's own signals — not the global symbol soup.
            assert!(
                !labels.iter().any(|l| l.contains("Sensors.Demo Frame")),
                "members should be bare leaf names, not full paths; got {labels:?}"
            );
        });
    }

    const M1PRJ_PARAM: &str = r#"<?xml version="1.0"?>
<Project>
  <Component Classname="BuiltIn.GroupCompound" Name="Root.Foo"/>
  <Component Classname="BuiltIn.Parameter" Name="Root.Foo.Gain.Value"><Props/></Component>
</Project>"#;
    const M1CFG_PARAM: &str = r#"<?xml version="1.0"?>
<Configuration><Group Name="">
  <Parameter Name="Root.Foo.Gain.Value"><Cell Type="u16" Unit="ratio"><![CDATA[1]]></Cell></Parameter>
</Group></Configuration>"#;

    const M1PRJ_ENUM: &str = r#"<?xml version="1.0"?>
<Project>
  <DataTypes>
    <Type Name="Gear State" Storage="enum" Default="Neutral">
      <Enum Name="Neutral" ContainerOrder="0"/>
      <Enum Name="First" ContainerOrder="1"/>
    </Type>
  </DataTypes>
  <Component Classname="BuiltIn.Channel" Name="Root.Transmission.Gear"><Props Type="::This.Gear State"/></Component>
</Project>"#;

    #[test]
    fn offers_enum_members_on_assignment_rhs() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("Project.m1prj"), M1PRJ_ENUM).unwrap();
        let store = ProjectStore::new();
        store.discover_and_load(tmp.path()).unwrap();

        let src = "Root.Transmission.Gear = \n";
        let byte = src.find('\n').unwrap(); // cursor just after `= `
        let cst = m1_core::parse(src);
        store.with_project(|p| {
            let items = completions(
                cst.root(),
                p,
                Some("X.m1scr"),
                src,
                byte,
                &crate::line_index::LineIndex::new(src),
                crate::line_index::PositionEncoding::Utf16,
            );
            let labels: Vec<_> = items.iter().map(|i| i.label.as_str()).collect();
            assert!(labels.contains(&"Gear State.Neutral"), "got {labels:?}");
            assert!(labels.contains(&"Gear State.First"));
            // Enum members replace the generic completion list here.
            assert!(!labels.contains(&"if"), "should not offer keywords");
        });
    }

    /// The universal real pattern: the LHS is a *bare in-group* channel name
    /// (`Drive State = …` inside the `Root.Control` group), not a fully-qualified
    /// path. Completion must still resolve it group-relatively and offer the enum's
    /// members — not fall through to the 2000-item global dump (#126).
    const M1PRJ_ENUM_GROUP: &str = r#"<?xml version="1.0"?>
<Project>
  <DataTypes>
    <Type Name="Drive State" Storage="enum" Default="Idle">
      <Enum Name="Idle" ContainerOrder="0"/>
      <Enum Name="Latching Fault" ContainerOrder="1"/>
    </Type>
  </DataTypes>
  <Component Classname="BuiltIn.GroupCompound" Name="Root"/>
  <Component Classname="BuiltIn.GroupCompound" Name="Root.Control"/>
  <Component Classname="BuiltIn.Channel" Name="Root.Control.Value"><Props Type="::This.Drive State"/></Component>
  <Component Classname="BuiltIn.MethodUser" Name="Root.Control.Update"/>
</Project>"#;

    #[test]
    fn offers_enum_members_for_bare_in_group_lhs() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("Project.m1prj"), M1PRJ_ENUM_GROUP).unwrap();
        let store = ProjectStore::new();
        store.discover_and_load(tmp.path()).unwrap();

        // `Value` is `Root.Control.Value`; the script lives in the `Root.Control`
        // group, so the bare name must resolve group-relatively.
        let src = "Value = \n";
        let byte = src.find('\n').unwrap();
        let cst = m1_core::parse(src);
        store.with_project(|p| {
            let items = completions(
                cst.root(),
                p,
                Some("Control.Update.m1scr"),
                src,
                byte,
                &crate::line_index::LineIndex::new(src),
                crate::line_index::PositionEncoding::Utf16,
            );
            let labels: Vec<_> = items.iter().map(|i| i.label.as_str()).collect();
            assert!(
                labels.contains(&"Drive State.Idle"),
                "bare in-group LHS should offer enum members, got {labels:?}"
            );
            assert!(labels.contains(&"Drive State.Latching Fault"));
            // The enum members replace the generic dump — no keyword fallback.
            assert!(
                !labels.contains(&"if"),
                "should not fall back to the global dump"
            );
        });
    }

    const M1PRJ_DEEP: &str = r#"<?xml version="1.0"?>
<Project>
  <Component Classname="BuiltIn.GroupCompound" Name="Root"/>
  <Component Classname="BuiltIn.GroupCompound" Name="Root.Control"/>
  <Component Classname="BuiltIn.GroupCompound" Name="Root.Control.AV"/>
  <Component Classname="BuiltIn.GroupCompound" Name="Root.Control.AV.DFMM"/>
  <Component Classname="BuiltIn.Channel" Name="Root.Control.AV.DFMM.Checkup"/>
</Project>"#;

    /// Typing a dotted path prefix (`Control.AV.`) must produce a completion that
    /// REPLACES the typed chain, not one appended after it. Before the fix the item
    /// was a bare label `Root.Control.AV.DFMM.Checkup` with no text_edit, so the
    /// client inserted it after the cursor → `Control.AV.Root.Control.AV.DFMM.Checkup`
    /// (invalid). The item must (a) display/insert the `Root.`-stripped path
    /// `Control.AV.DFMM.Checkup` and (b) carry a text_edit whose range starts at the
    /// beginning of the typed chain so accepting it yields exactly that path.
    #[test]
    fn dotted_path_completion_replaces_typed_prefix() {
        use tower_lsp::lsp_types::CompletionTextEdit;
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("Project.m1prj"), M1PRJ_DEEP).unwrap();
        let store = ProjectStore::new();
        store.discover_and_load(tmp.path()).unwrap();

        let src = "Control.AV.";
        let byte = src.len(); // cursor right after the trailing `.`
        let cst = m1_core::parse(src);
        let li = crate::line_index::LineIndex::new(src);
        let enc = crate::line_index::PositionEncoding::Utf16;
        store.with_project(|p| {
            let items = completions(cst.root(), p, Some("X.m1scr"), src, byte, &li, enc);
            // The path is shown Root-stripped, exactly as the user is typing it.
            let item = items
                .iter()
                .find(|i| i.label == "Control.AV.DFMM.Checkup")
                .unwrap_or_else(|| {
                    let labels: Vec<_> = items.iter().map(|i| i.label.as_str()).collect();
                    panic!("expected Root-stripped path label, got {labels:?}")
                });
            // No stale full-Root label that would duplicate the typed prefix.
            assert!(
                !items
                    .iter()
                    .any(|i| i.label == "Root.Control.AV.DFMM.Checkup"),
                "the full-Root label must be replaced by the stripped form"
            );
            let Some(CompletionTextEdit::Edit(edit)) = &item.text_edit else {
                panic!(
                    "completion must carry a text_edit, got {:?}",
                    item.text_edit
                );
            };
            assert_eq!(
                edit.new_text, "Control.AV.DFMM.Checkup",
                "text_edit must insert the clean path"
            );
            // Range covers the whole typed chain `Control.AV.` (line 0, cols 0..11),
            // so accepting REPLACES it rather than appending.
            assert_eq!(edit.range.start.line, 0);
            assert_eq!(
                edit.range.start.character, 0,
                "edit must start at the chain head"
            );
            assert_eq!(edit.range.end.character, byte as u32);
            // Filter text lets the typed prefix match the item.
            assert!(
                item.filter_text.as_deref().unwrap_or(&item.label) == "Control.AV.DFMM.Checkup",
                "filter_text should be the stripped path"
            );
        });
    }

    /// BUG 4: completing in the MIDDLE of a dotted chain must replace the WHOLE
    /// chain (head..tail), not `chain_start..cursor`. With the cursor inside
    /// `Accumulator` (`Driveline.A|ccumulator.Voltage`), the old edit range ended
    /// at the cursor, so accepting `Control.AV.DFMM.Checkup` left the orphaned
    /// suffix `ccumulator.Voltage` behind. The fix scans forward to the end of the
    /// chain so the edit range covers the entire token.
    #[test]
    fn mid_chain_completion_replaces_whole_chain() {
        use tower_lsp::lsp_types::CompletionTextEdit;
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("Project.m1prj"), M1PRJ_DEEP).unwrap();
        let store = ProjectStore::new();
        store.discover_and_load(tmp.path()).unwrap();

        // Cursor sits right after `Driveline.A`, i.e. inside `Accumulator`.
        let src = "local v = Driveline.Accumulator.Voltage;\n";
        let byte = src.find("Driveline.A").unwrap() + "Driveline.A".len();
        let chain_start = src.find("Driveline").unwrap();
        let chain_end = src.find(';').unwrap(); // end of the dotted run
        let cst = m1_core::parse(src);
        let li = crate::line_index::LineIndex::new(src);
        let enc = crate::line_index::PositionEncoding::Utf16;
        store.with_project(|p| {
            let items = completions(cst.root(), p, Some("X.m1scr"), src, byte, &li, enc);
            let item = items
                .iter()
                .find(|i| i.label == "Control.AV.DFMM.Checkup")
                .expect("project symbol should be offered mid-chain");
            let Some(CompletionTextEdit::Edit(edit)) = &item.text_edit else {
                panic!("completion must carry a text_edit");
            };
            // Range covers the FULL chain `Driveline.Accumulator.Voltage`, so
            // accepting replaces it cleanly with no orphaned `ccumulator.Voltage`.
            assert_eq!(
                edit.range.start.character, chain_start as u32,
                "edit must start at the chain head"
            );
            assert_eq!(
                edit.range.end.character, chain_end as u32,
                "edit must end at the chain tail, not the cursor"
            );

            // Simulate the client applying the edit: replace [start..end) with
            // new_text. The result must be exactly the inserted path — no suffix.
            let mut applied = String::new();
            applied.push_str(&src[..chain_start]);
            applied.push_str(&edit.new_text);
            applied.push_str(&src[chain_end..]);
            assert_eq!(
                applied, "local v = Control.AV.DFMM.Checkup;\n",
                "accepting must replace the whole chain, got {applied:?}"
            );
            assert!(
                !applied.contains("ccumulator"),
                "no orphaned suffix may survive, got {applied:?}"
            );
        });
    }

    /// BUG 5: completion must NOT fire inside line comments, block comments, or
    /// string literals — it would otherwise dump the whole library/keyword set
    /// into prose. Normal code positions still return items.
    #[test]
    fn no_completion_inside_comments_and_strings() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("Project.m1prj"), M1PRJ).unwrap();
        let store = ProjectStore::new();
        store.discover_and_load(tmp.path()).unwrap();
        let li = |s: &str| crate::line_index::LineIndex::new(s);
        let enc = crate::line_index::PositionEncoding::Utf16;

        // Inside a line comment.
        let line_c = "// some Calc note\n";
        let lc_byte = line_c.find("Calc").unwrap() + 2; // inside the comment text
        // Inside a block comment.
        let block_c = "/* some Calc note */\n";
        let bc_byte = block_c.find("Calc").unwrap() + 2;
        // Inside a string literal.
        let string_l = "local s = \"some Calc text\";\n";
        let sl_byte = string_l.find("Calc").unwrap() + 2;
        // Normal code (control: must still return items).
        let code = "local x = \n";
        let code_byte = code.find('\n').unwrap();

        store.with_project(|p| {
            let comp = |src: &str, byte: usize| {
                completions(
                    m1_core::parse(src).root(),
                    p,
                    Some("X.m1scr"),
                    src,
                    byte,
                    &li(src),
                    enc,
                )
            };
            assert!(
                comp(line_c, lc_byte).is_empty(),
                "no completion inside a line comment"
            );
            assert!(
                comp(block_c, bc_byte).is_empty(),
                "no completion inside a block comment"
            );
            assert!(
                comp(string_l, sl_byte).is_empty(),
                "no completion inside a string literal"
            );
            assert!(
                !comp(code, code_byte).is_empty(),
                "completion still fires in normal code"
            );
        });
    }

    #[test]
    fn parameter_completion_shows_type_and_unit_from_cfg() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("Project.m1prj"), M1PRJ_PARAM).unwrap();
        std::fs::write(tmp.path().join("parameters.m1cfg"), M1CFG_PARAM).unwrap();
        let store = ProjectStore::new();
        store.discover_and_load(tmp.path()).unwrap();

        let src = "\n";
        let cst = m1_core::parse(src);
        store.with_project(|p| {
            let items = completions(
                cst.root(),
                p,
                Some("X.m1scr"),
                src,
                src.len(),
                &crate::line_index::LineIndex::new(src),
                crate::line_index::PositionEncoding::Utf16,
            );
            // Paths are offered `Root.`-stripped (the idiomatic bare reference).
            let item = items
                .iter()
                .find(|i| i.label == "Foo.Gain.Value")
                .expect("parameter should be offered");
            let detail = item.detail.as_deref().unwrap_or("");
            assert!(
                detail.contains("Unsigned") && detail.contains("ratio"),
                "completion detail should carry the cfg type + unit, got: {detail:?}"
            );
        });
    }

    #[test]
    fn calibration_only_objects_not_offered_in_completion() {
        // UI is calibration-method-only; it must not be offered in ECU .m1scr
        // completion. Math carries ECU functions (lowercase `fabs`, `sqrt`, ...
        // — the real corpus calls Math.fabs in scripts) alongside its
        // calibration-only PascalCase set, so it IS offered.
        let src = "\n";
        let cst = m1_core::parse(src);
        let items = completions(
            cst.root(),
            None,
            Some("X.m1scr"),
            src,
            0,
            &crate::line_index::LineIndex::new(src),
            crate::line_index::PositionEncoding::Utf16,
        );
        let labels: Vec<_> = items.iter().map(|i| i.label.as_str()).collect();
        assert!(
            labels.contains(&"Calculate"),
            "ECU object offered: {labels:?}"
        );
        assert!(
            labels.contains(&"System"),
            "System offered (has ECU functions)"
        );
        assert!(
            labels.contains(&"Math"),
            "Math offered (has ECU functions like fabs)"
        );
        assert!(!labels.contains(&"UI"), "UI is calibration-only");
    }

    #[test]
    fn calibration_only_methods_filtered_after_dot() {
        // System carries ECU functions plus the calibration-only StrCat; only the
        // ECU functions are offered after `System.`. (Debug is an ECU function.)
        let src = "x = System.\n";
        let cst = m1_core::parse(src);
        let byte = src.find("System.").unwrap() + "System.".len();
        let items = completions(
            cst.root(),
            None,
            None,
            src,
            byte,
            &crate::line_index::LineIndex::new(src),
            crate::line_index::PositionEncoding::Utf16,
        );
        let labels: Vec<_> = items.iter().map(|i| i.label.as_str()).collect();
        assert!(
            labels.contains(&"Preserve"),
            "ECU System.Preserve offered: {labels:?}"
        );
        assert!(labels.contains(&"Debug"), "ECU System.Debug offered");
        assert!(
            !labels.contains(&"StrCat"),
            "StrCat is calibration-only: {labels:?}"
        );
    }

    #[test]
    fn completes_library_methods_after_dot() {
        let src = "x = Calculate.\n";
        let cst = m1_core::parse(src);
        let byte = src.find("Calculate.").unwrap() + "Calculate.".len();
        let items = completions(
            cst.root(),
            None,
            None,
            src,
            byte,
            &crate::line_index::LineIndex::new(src),
            crate::line_index::PositionEncoding::Utf16,
        );
        let labels: Vec<_> = items.iter().map(|i| i.label.as_str()).collect();
        assert!(
            labels.contains(&"Max"),
            "Calculate.Max should be offered: {labels:?}"
        );
        assert!(labels.contains(&"Min"));
        // only methods after a library-object dot (no keywords/objects mixed in)
        assert!(!labels.contains(&"if"));

        // Methods carry a snippet insert-text with ${N:param} placeholders (#28).
        let max = items.iter().find(|i| i.label == "Max").unwrap();
        assert_eq!(
            max.insert_text_format,
            Some(InsertTextFormat::SNIPPET),
            "Max should be a snippet"
        );
        let snip = max.insert_text.as_deref().unwrap_or("");
        assert!(
            snip.starts_with("Max(") && snip.contains("${1:"),
            "expected placeholder snippet, got {snip:?}"
        );
    }

    #[test]
    fn offers_objects_and_keywords_at_top_level() {
        let src = "x = \n";
        let cst = m1_core::parse(src);
        let items = completions(
            cst.root(),
            None,
            None,
            src,
            src.len(),
            &crate::line_index::LineIndex::new(src),
            crate::line_index::PositionEncoding::Utf16,
        );
        let labels: Vec<_> = items.iter().map(|i| i.label.as_str()).collect();
        assert!(labels.contains(&"Calculate"), "library object offered");
        assert!(labels.contains(&"if"), "keyword offered");
    }

    #[test]
    fn construct_keywords_carry_a_snippet_body() {
        // #173: accepting `if`/`when`/`expand`/`local` inserts the full construct.
        let src = "x = \n";
        let cst = m1_core::parse(src);
        let items = completions(
            cst.root(),
            None,
            None,
            src,
            src.len(),
            &crate::line_index::LineIndex::new(src),
            crate::line_index::PositionEncoding::Utf16,
        );
        let if_item = items
            .iter()
            .find(|i| i.label == "if")
            .expect("`if` offered");
        assert_eq!(if_item.insert_text_format, Some(InsertTextFormat::SNIPPET));
        let body = if_item.insert_text.as_deref().unwrap_or("");
        assert!(
            body.starts_with("if (") && body.contains("${1:"),
            "got {body:?}"
        );
        let when_item = items.iter().find(|i| i.label == "when").unwrap();
        assert!(
            when_item
                .insert_text
                .as_deref()
                .unwrap_or("")
                .contains("is (${2:"),
            "when expands to a when…is skeleton"
        );
    }
}
