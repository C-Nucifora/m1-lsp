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
    Some(match &sym.unit {
        Some(u) => format!("{ty} · {u}"),
        None => ty,
    })
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

    let mut items: Vec<CompletionItem> = Vec::new();

    // Library objects (Calculate, CanComms, …) and keywords are always offered.
    for name in intr.library.keys() {
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
