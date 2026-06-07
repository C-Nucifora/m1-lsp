//! workspace/symbol: project-wide symbol search over the loaded SymbolTable.
//!
//! The model already holds the whole project's symbols (channels, parameters,
//! groups, functions, …) with their definition sites, so a query is just a
//! case-insensitive filter over `project.symbols()` mapped to `SymbolInformation`.
use crate::project_store::LoadedProject;
use m1_typecheck::symbols::{Symbol, SymbolKind};
#[allow(deprecated)]
use tower_lsp::lsp_types::{Location, SymbolInformation, SymbolKind as LspSymbolKind};

/// All project symbols whose dotted path contains `query` (case-insensitive),
/// as LSP `SymbolInformation`. An empty query returns every symbol.
#[allow(deprecated)]
pub fn workspace_symbols(loaded: &LoadedProject, query: &str) -> Vec<SymbolInformation> {
    let table = loaded.project.symbols();
    // A `tag:<name>` query selects symbols carrying that tag (own or inherited)
    // via the tag index, turning the editor's symbol search into a tag-based
    // channel browser (#170). Otherwise it is a case-insensitive substring match
    // over the dotted path.
    let candidates: Vec<&Symbol> = if let Some(tag) = query.strip_prefix("tag:") {
        table.symbols_with_tag(tag.trim())
    } else {
        let q = query.to_lowercase();
        table
            .iter()
            .filter(|s| q.is_empty() || s.path.to_lowercase().contains(&q))
            .collect()
    };
    candidates
        .into_iter()
        .filter_map(|s| {
            let location: Location = loaded.symbol_location(s)?;
            Some(SymbolInformation {
                name: s.path.clone(),
                kind: lsp_kind(s.kind),
                tags: None,
                deprecated: None,
                location,
                container_name: container_of(&s.path),
            })
        })
        .collect()
}

/// The enclosing group path (everything before the last `.`), if any.
fn container_of(path: &str) -> Option<String> {
    path.rsplit_once('.').map(|(head, _)| head.to_string())
}

fn lsp_kind(kind: SymbolKind) -> LspSymbolKind {
    match kind {
        SymbolKind::Channel => LspSymbolKind::VARIABLE,
        SymbolKind::Parameter => LspSymbolKind::PROPERTY,
        SymbolKind::Constant => LspSymbolKind::CONSTANT,
        SymbolKind::Function => LspSymbolKind::FUNCTION,
        SymbolKind::Method => LspSymbolKind::METHOD,
        SymbolKind::Table => LspSymbolKind::ARRAY,
        SymbolKind::Group => LspSymbolKind::NAMESPACE,
        SymbolKind::Reference => LspSymbolKind::VARIABLE,
        SymbolKind::Object => LspSymbolKind::OBJECT,
        SymbolKind::Other => LspSymbolKind::VARIABLE,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::project_store::ProjectStore;
    use std::io::Write;

    const M1PRJ: &str = r#"<?xml version="1.0"?>
<Project>
  <Component Classname="BuiltIn.GroupCompound" Name="Root"/>
  <Component Classname="BuiltIn.GroupCompound" Name="Root.Engine"/>
  <Component Classname="BuiltIn.Channel" Name="Root.Engine.Speed"><Props Type="f32" SelectedTags="Engine Normal"/></Component>
  <Component Classname="BuiltIn.Channel" Name="Root.Engine.Temperature"><Props Type="f32" SelectedTags="Vehicle"/></Component>
  <Component Classname="BuiltIn.FuncUser" Name="Root.Engine.Update" Filename="Engine Update.m1scr"/>
</Project>"#;

    fn loaded() -> ProjectStore {
        let tmp = Box::leak(Box::new(tempfile::tempdir().unwrap()));
        let prj = tmp.path().join("Project.m1prj");
        std::fs::File::create(&prj)
            .unwrap()
            .write_all(M1PRJ.as_bytes())
            .unwrap();
        let store = ProjectStore::new();
        store.discover_and_load(tmp.path()).unwrap();
        store
    }

    #[test]
    fn filters_by_query_case_insensitively() {
        let store = loaded();
        store.with_project(|p| {
            let p = p.unwrap();
            let hits = workspace_symbols(p, "speed");
            let names: Vec<_> = hits.iter().map(|s| s.name.as_str()).collect();
            assert!(names.contains(&"Root.Engine.Speed"), "got {names:?}");
            assert!(!names.contains(&"Root.Engine.Temperature"));
        });
    }

    // #170: a `tag:<name>` query selects symbols carrying that tag (own or
    // inherited) instead of doing a path substring match.
    #[test]
    fn tag_prefix_filters_by_tag() {
        let store = loaded();
        store.with_project(|p| {
            let hits = workspace_symbols(p.unwrap(), "tag:Engine");
            let names: Vec<_> = hits.iter().map(|s| s.name.as_str()).collect();
            assert!(names.contains(&"Root.Engine.Speed"), "got {names:?}");
            // Temperature is tagged Vehicle, not Engine.
            assert!(!names.contains(&"Root.Engine.Temperature"), "got {names:?}");
        });
    }

    #[test]
    fn empty_query_returns_all_with_locations() {
        let store = loaded();
        store.with_project(|p| {
            let hits = workspace_symbols(p.unwrap(), "");
            // Every returned symbol has a resolvable definition location.
            assert!(
                hits.len() >= 4,
                "expected the whole table, got {}",
                hits.len()
            );
            // A channel resolves into the .m1prj; a function into its script.
            let speed = hits.iter().find(|s| s.name == "Root.Engine.Speed").unwrap();
            assert!(speed.location.uri.path().ends_with("Project.m1prj"));
            assert_eq!(speed.container_name.as_deref(), Some("Root.Engine"));
            let update = hits
                .iter()
                .find(|s| s.name == "Root.Engine.Update")
                .unwrap();
            assert!(
                update
                    .location
                    .uri
                    .path()
                    .ends_with("Engine%20Update.m1scr")
            );
        });
    }
}
