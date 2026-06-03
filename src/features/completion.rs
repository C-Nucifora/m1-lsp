//! textDocument/completion: library methods after `.`, else in-scope locals +
//! project symbols + library objects + keywords.
use crate::features::locate::collect_locals;
use crate::project_store::LoadedProject;
use m1_core::Node;
use m1_typecheck::project::Project;
use std::collections::HashSet;
use tower_lsp::lsp_types::{CompletionItem, CompletionItemKind, Documentation, InsertTextFormat};

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
fn enum_member_completions(lhs: &str, lp: &LoadedProject) -> Option<Vec<CompletionItem>> {
    use m1_typecheck::types::ValueType;
    let table = lp.project.symbols();
    let sym = table
        .get(lhs)
        .or_else(|| table.get(m1_workspace::qualify_root(lhs).as_ref()))?;
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
    let before = &text[..byte.min(text.len())];
    let start = before
        .rfind(|c: char| !(c.is_alphanumeric() || c == '_' || c == '.'))
        .map(|i| i + 1)
        .unwrap_or(0);
    let chain = &before[start..];
    let dot = chain.rfind('.')?;
    Some(chain[..dot].to_string())
}

/// Completion items for the document. `text`/`byte` give the cursor context so a
/// `.` after a library object completes that object's methods.
pub fn completions(
    root: Node,
    loaded: Option<&LoadedProject>,
    file_name: Option<&str>,
    text: &str,
    byte: usize,
) -> Vec<CompletionItem> {
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

    // On the RHS of `enumChannel = <cursor>`: offer that channel's enum members
    // (e.g. `GearState.Neutral` … with their integer values) instead of generic
    // symbols (#79).
    if let Some(lp) = loaded
        && let Some(lhs) = assignment_lhs(text, byte)
        && let Some(items) = enum_member_completions(&lhs, lp)
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
            ..Default::default()
        });
    }
    for words in intr.language.keywords.values() {
        for kw in words {
            items.push(CompletionItem {
                label: kw.clone(),
                kind: Some(CompletionItemKind::KEYWORD),
                ..Default::default()
            });
        }
    }

    // 1. In-scope locals.
    for (name, _ty) in collect_locals(root) {
        items.push(CompletionItem {
            label: name,
            kind: Some(CompletionItemKind::VARIABLE),
            ..Default::default()
        });
    }

    // 2. Project symbols: full path, and the group-relative tail for this file.
    if let Some(lp) = loaded {
        let group = file_name.and_then(|f| lp.project.group_for_script(f));
        for sym in lp.project.symbols().iter() {
            let ty = type_unit_detail(sym, &lp.project);
            items.push(CompletionItem {
                label: sym.path.clone(),
                kind: Some(CompletionItemKind::FIELD),
                detail: ty.clone(),
                ..Default::default()
            });
            if let Some(g) = &group
                && let Some(tail) = sym.path.strip_prefix(&format!("{g}."))
            {
                // The short tail hides the full path, so put the path in the
                // detail, plus the type/unit when known.
                let detail = match &ty {
                    Some(t) => format!("{}  ·  {t}", sym.path),
                    None => sym.path.clone(),
                };
                items.push(CompletionItem {
                    label: tail.to_string(),
                    kind: Some(CompletionItemKind::FIELD),
                    detail: Some(detail),
                    ..Default::default()
                });
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
            let items = completions(cst.root(), p, Some("X.m1scr"), src, src.len());
            let labels: Vec<_> = items.iter().map(|i| i.label.as_str()).collect();
            assert!(labels.contains(&"fGain"));
            assert!(labels.iter().any(|l| l.contains("Speed Glonk")));
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
            let items = completions(cst.root(), p, Some("X.m1scr"), src, byte);
            let labels: Vec<_> = items.iter().map(|i| i.label.as_str()).collect();
            assert!(labels.contains(&"Gear State.Neutral"), "got {labels:?}");
            assert!(labels.contains(&"Gear State.First"));
            // Enum members replace the generic completion list here.
            assert!(!labels.contains(&"if"), "should not offer keywords");
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
            let items = completions(cst.root(), p, Some("X.m1scr"), src, src.len());
            let item = items
                .iter()
                .find(|i| i.label == "Root.Foo.Gain.Value")
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
        // Math / UI are calibration-method-only objects; they must not be offered
        // in ECU .m1scr completion. ECU objects (Calculate, System) still are.
        let src = "\n";
        let cst = m1_core::parse(src);
        let items = completions(cst.root(), None, Some("X.m1scr"), src, 0);
        let labels: Vec<_> = items.iter().map(|i| i.label.as_str()).collect();
        assert!(
            labels.contains(&"Calculate"),
            "ECU object offered: {labels:?}"
        );
        assert!(
            labels.contains(&"System"),
            "System offered (has ECU functions)"
        );
        assert!(!labels.contains(&"Math"), "Math is calibration-only");
        assert!(!labels.contains(&"UI"), "UI is calibration-only");
    }

    #[test]
    fn calibration_only_methods_filtered_after_dot() {
        // System carries ECU functions plus the calibration-only StrCat; only the
        // ECU functions are offered after `System.`. (Debug is an ECU function.)
        let src = "x = System.\n";
        let cst = m1_core::parse(src);
        let byte = src.find("System.").unwrap() + "System.".len();
        let items = completions(cst.root(), None, None, src, byte);
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
        let items = completions(cst.root(), None, None, src, byte);
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
        let items = completions(cst.root(), None, None, src, src.len());
        let labels: Vec<_> = items.iter().map(|i| i.label.as_str()).collect();
        assert!(labels.contains(&"Calculate"), "library object offered");
        assert!(labels.contains(&"if"), "keyword offered");
    }
}
