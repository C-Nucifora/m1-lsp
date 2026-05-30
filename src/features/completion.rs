//! textDocument/completion: in-scope locals + project symbols.
use crate::features::locate::collect_locals;
use crate::project_store::LoadedProject;
use m1_core::Node;
use tower_lsp::lsp_types::{CompletionItem, CompletionItemKind};

pub fn completions(
    root: Node,
    loaded: Option<&LoadedProject>,
    file_name: Option<&str>,
) -> Vec<CompletionItem> {
    let mut items: Vec<CompletionItem> = Vec::new();

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
            items.push(CompletionItem {
                label: sym.path.clone(),
                kind: Some(CompletionItemKind::FIELD),
                ..Default::default()
            });
            if let Some(g) = &group {
                if let Some(tail) = sym.path.strip_prefix(&format!("{g}.")) {
                    items.push(CompletionItem {
                        label: tail.to_string(),
                        kind: Some(CompletionItemKind::FIELD),
                        detail: Some(sym.path.clone()),
                        ..Default::default()
                    });
                }
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
            let items = completions(cst.root(), p, Some("X.m1scr"));
            let labels: Vec<_> = items.iter().map(|i| i.label.as_str()).collect();
            assert!(labels.contains(&"fGain"));
            assert!(labels.iter().any(|l| l.contains("Speed Glonk")));
        });
    }
}
