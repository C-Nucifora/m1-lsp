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
///
/// Faceted queries (#170, #236): whitespace-separated `key:value` tokens
/// filter, the remaining free text substring-matches the path. Facets compose
/// (`security:Tune torque`):
/// - `tag:<name>` — symbols carrying the tag (own or inherited)
/// - `security:<level>` — by `Props Security` (case-insensitive)
/// - `rate:<hz>` — functions/methods scheduled at that numeric rate, or the named
///   rate `rate:Startup` (case-insensitive) for startup-scheduled functions
/// - `type:<enum|float|integer|unsigned|boolean|string>` — by value type
#[allow(deprecated)]
pub fn workspace_symbols(loaded: &LoadedProject, query: &str) -> Vec<SymbolInformation> {
    use m1_typecheck::types::ValueType;
    let table = loaded.project.symbols();

    let mut tag: Option<String> = None;
    let mut security: Option<String> = None;
    let mut rate: Option<RateFacet> = None;
    let mut vtype: Option<String> = None;
    let mut free = String::new();
    for tok in query.split_whitespace() {
        match tok.split_once(':') {
            Some(("tag", v)) => tag = Some(v.to_string()),
            Some(("security", v)) => security = Some(v.to_lowercase()),
            Some(("rate", v)) => rate = RateFacet::parse(v),
            Some(("type", v)) => vtype = Some(v.to_lowercase()),
            _ => {
                if !free.is_empty() {
                    free.push(' ');
                }
                free.push_str(tok);
            }
        }
    }
    let free = free.to_lowercase();

    let base: Vec<&Symbol> = match &tag {
        Some(t) => table.symbols_with_tag(t.trim()),
        None => table.iter().collect(),
    };
    let type_matches = |s: &Symbol| match vtype.as_deref() {
        None => true,
        Some("enum") => matches!(s.value_type, ValueType::Enum(_)),
        Some("float") => s.value_type == ValueType::Float,
        Some("integer") => s.value_type == ValueType::Integer,
        Some("unsigned") => s.value_type == ValueType::Unsigned,
        Some("boolean") => s.value_type == ValueType::Boolean,
        Some("string") => s.value_type == ValueType::String,
        Some(_) => false,
    };
    let candidates: Vec<&Symbol> = base
        .into_iter()
        .filter(|s| {
            (free.is_empty() || s.path.to_lowercase().contains(&free))
                && security.as_deref().is_none_or(|sec| {
                    s.security
                        .as_deref()
                        .is_some_and(|have| have.eq_ignore_ascii_case(sec))
                })
                && rate.as_ref().is_none_or(|r| r.matches(s))
                && type_matches(s)
        })
        .collect();
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

/// The `rate:` facet value: a numeric execution rate in Hz, or the named
/// `Startup` schedule slice. A numeric rate matches functions triggered by a
/// resolvable clock (`On <N>Hz`); `Startup` matches startup-scheduled functions,
/// which the symbol model records as `scheduled` with no numeric `call_rate_hz`
/// (an `On Startup` trigger carries no Hz). telescope-m1's call_rates picker
/// dead-ended on its `Startup` slice because the facet parsed floats only (#).
enum RateFacet {
    Hz(f64),
    Startup,
}

impl RateFacet {
    /// Parse a `rate:` value: `Startup` (case-insensitive) or a float. `None` for
    /// anything else, so an unrecognised value filters nothing out silently rather
    /// than matching everything.
    fn parse(v: &str) -> Option<RateFacet> {
        if v.eq_ignore_ascii_case("startup") {
            return Some(RateFacet::Startup);
        }
        v.parse::<f64>().ok().map(RateFacet::Hz)
    }

    fn matches(&self, s: &Symbol) -> bool {
        match self {
            RateFacet::Hz(r) => s.call_rate_hz.is_some_and(|have| (have - r).abs() < 1e-9),
            // A startup function is bound to a schedule event (`scheduled`) but has
            // no statically-resolvable clock rate (`call_rate_hz == None`). This
            // also captures the rare `$(…)` parameter-reference trigger, which the
            // model likewise can't resolve to a rate — both belong to the "no fixed
            // Hz" slice the picker groups under Startup.
            RateFacet::Startup => s.scheduled && s.call_rate_hz.is_none(),
        }
    }
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
  <Component Classname="BuiltIn.EventKernel" Name="Root.Events.On 100Hz"/>
  <Component Classname="BuiltIn.EventKernel" Name="Root.Events.On Startup"/>
  <Component Classname="BuiltIn.Channel" Name="Root.Engine.Speed"><Props Type="f32" SelectedTags="Engine Normal" Security="Tune"/></Component>
  <Component Classname="BuiltIn.Channel" Name="Root.Engine.Temperature"><Props Type="f32" SelectedTags="Vehicle"/></Component>
  <Component Classname="BuiltIn.Channel" Name="Root.Engine.Count"><Props Type="s32"/></Component>
  <Component Classname="BuiltIn.FuncUser" Name="Root.Engine.Update" Filename="Engine Update.m1scr"><Props SelectedTrigger="Parent.Parent.Events.On 100Hz"/></Component>
  <Component Classname="BuiltIn.FuncUser" Name="Root.Engine.Init" Filename="Engine Init.m1scr"><Props SelectedTrigger="Parent.Parent.Events.On Startup"/></Component>
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

    // #236: faceted queries compose with free text.
    #[test]
    fn facets_filter_by_security_rate_and_type() {
        let store = loaded();
        store.with_project(|p| {
            let p = p.unwrap();
            let names = |q: &str| -> Vec<String> {
                workspace_symbols(p, q)
                    .iter()
                    .map(|s| s.name.clone())
                    .collect()
            };
            let sec = names("security:tune");
            assert!(sec.contains(&"Root.Engine.Speed".into()), "got {sec:?}");
            assert!(!sec.contains(&"Root.Engine.Temperature".into()));

            let rate = names("rate:100");
            assert!(rate.contains(&"Root.Engine.Update".into()), "got {rate:?}");
            assert!(!rate.contains(&"Root.Engine.Speed".into()));

            let ints = names("type:integer");
            assert!(ints.contains(&"Root.Engine.Count".into()), "got {ints:?}");
            assert!(!ints.contains(&"Root.Engine.Speed".into()));

            // Facet + free text compose.
            let combo = names("type:float speed");
            assert!(combo.contains(&"Root.Engine.Speed".into()), "got {combo:?}");
            assert!(!combo.contains(&"Root.Engine.Temperature".into()));
        });
    }

    // Finding 5: the `rate:` facet accepts the named `Startup` slice (not just
    // floats), so telescope-m1's call_rates picker no longer dead-ends on it.
    #[test]
    fn rate_startup_facet_filters_startup_functions() {
        let store = loaded();
        store.with_project(|p| {
            let p = p.unwrap();
            let names = |q: &str| -> Vec<String> {
                workspace_symbols(p, q)
                    .iter()
                    .map(|s| s.name.clone())
                    .collect()
            };
            // Case-insensitive named rate returns the startup-triggered function…
            let start = names("rate:Startup");
            assert!(start.contains(&"Root.Engine.Init".into()), "got {start:?}");
            // …and excludes the 100Hz one.
            assert!(
                !start.contains(&"Root.Engine.Update".into()),
                "a clocked fn is not startup: {start:?}"
            );
            assert_eq!(names("rate:startup"), start, "facet is case-insensitive");

            // The numeric facet still works and excludes the startup function.
            let hz = names("rate:100");
            assert!(hz.contains(&"Root.Engine.Update".into()), "got {hz:?}");
            assert!(!hz.contains(&"Root.Engine.Init".into()), "got {hz:?}");
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
