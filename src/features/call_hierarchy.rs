//! Call hierarchy over the channel data-flow graph (#84).
//!
//! M1 has no function-call graph in the C sense; the meaningful cross-script
//! dependency is *data flow through channels*. This module models that as an LSP
//! call hierarchy so a developer can navigate it with the editor's built-in
//! "Peek Call Hierarchy" UI:
//!
//! | LSP concept | M1 equivalent |
//! |---|---|
//! | item (a "function") | a `.m1scr` script, or a channel |
//! | outgoing call from a script | a channel the script **writes** |
//! | incoming call to a script | a script that **reads** a channel this one writes |
//! | incoming call to a channel | a script that **reads** the channel |
//! | outgoing call from a channel | a script that **writes** (produces) the channel |
//!
//! Items carry a small JSON tag in [`CallHierarchyItem::data`] so the incoming/
//! outgoing handlers know whether they are looking at a script or a channel.
//!
//! The index is built on demand (not cached at project load) so it always
//! reflects unsaved edits in open buffers, the same freshness contract as
//! [`super::references::project_references`].
use crate::convert::range;
use crate::features::locate::build_scope;
use crate::features::references::path_occurrences;
use crate::line_index::{LineIndex, PositionEncoding};
use crate::project_store::LoadedProject;
use m1_typecheck::resolve::{Resolution, Scope, resolve};
use m1_typecheck::symbols::{Symbol, SymbolKind};
use serde_json::json;
use std::collections::BTreeMap;
use tower_lsp::lsp_types::{
    CallHierarchyIncomingCall, CallHierarchyItem, CallHierarchyOutgoingCall, Range,
    SymbolKind as LspKind, Url,
};

/// One script in the data-flow graph: where it lives, its rate, and the channels
/// it reads and writes (with the in-file ranges of each occurrence).
pub struct ScriptNode {
    pub uri: Url,
    pub file_name: String,
    /// Human label for the item — the script's group path when known, else the
    /// file stem.
    pub label: String,
    pub rate_hz: Option<f64>,
    /// A zero-length range at the start of the file; the item's selection range.
    pub anchor: Range,
    /// channel path → read occurrence ranges in this script.
    pub reads: BTreeMap<String, Vec<Range>>,
    /// channel path → write occurrence ranges in this script.
    pub writes: BTreeMap<String, Vec<Range>>,
}

/// The channel↔script read/write index for one loaded project.
pub struct CallGraph {
    pub scripts: Vec<ScriptNode>,
    /// channel path → indices into `scripts` that write it.
    writers: BTreeMap<String, Vec<usize>>,
    /// channel path → indices into `scripts` that read it.
    readers: BTreeMap<String, Vec<usize>>,
}

/// The conventional backing-file basename of a script symbol: its explicit
/// `Filename`, else the path-encoding convention (`Root.Engine.Update` →
/// `Engine.Update.m1scr`). Mirrors `project_store`/`m1prj` script matching.
pub(crate) fn backing_file(sym: &Symbol) -> String {
    sym.filename.clone().unwrap_or_else(|| {
        format!(
            "{}.m1scr",
            sym.path.strip_prefix("Root.").unwrap_or(&sym.path)
        )
    })
}

/// The Function/Method symbol backing a given script file, if any — the source of
/// the script's call rate and group label.
pub(crate) fn script_symbol<'a>(loaded: &'a LoadedProject, file_name: &str) -> Option<&'a Symbol> {
    loaded.project.symbols().iter().find(|s| {
        matches!(s.kind, SymbolKind::Function | SymbolKind::Method) && backing_file(s) == file_name
    })
}

/// Resolve a path reference (as written in a script — possibly group-relative)
/// to the *canonical* full path of the project Channel it names, or `None` when
/// it isn't a channel (a local, library member, parameter, …). Using the
/// type-checker's `resolve` is what makes the graph correct: two scripts in
/// different groups that both write `Speed.Number` relative to their own group
/// collapse onto the same canonical channel. `scope` carries the script's group
/// anchor (see [`build_scope`]).
fn channel_path(scope: &Scope, path: &str) -> Option<String> {
    match resolve(path, scope) {
        Resolution::Symbol(s) if s.kind == SymbolKind::Channel => Some(s.path.clone()),
        _ => None,
    }
}

impl CallGraph {
    /// Build the index by scanning every project script once. `open_text` supplies
    /// the in-memory buffer for any open file (preferred over disk).
    pub fn build(
        loaded: &LoadedProject,
        enc: PositionEncoding,
        open_text: &dyn Fn(&Url) -> Option<String>,
    ) -> CallGraph {
        let mut scripts = Vec::new();
        for p in &loaded.script_files {
            let Ok(uri) = Url::from_file_path(p) else {
                continue;
            };
            let Some(text) = open_text(&uri).or_else(|| crate::disk_read::read_disk(p)) else {
                continue;
            };
            let file_name = p
                .file_name()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default();
            let li = LineIndex::new(&text);
            let cst = m1_core::parse(&text);
            // The script's scope anchors group-relative channel references.
            let scope = build_scope(cst.root(), Some(&loaded.project), Some(&file_name));
            let mut reads: BTreeMap<String, Vec<Range>> = BTreeMap::new();
            let mut writes: BTreeMap<String, Vec<Range>> = BTreeMap::new();
            for (path, br, is_write) in path_occurrences(cst.root()) {
                let Some(canon) = channel_path(&scope, &path) else {
                    continue;
                };
                let r = range(&br, &li, enc);
                if is_write {
                    writes.entry(canon).or_default().push(r);
                } else {
                    reads.entry(canon).or_default().push(r);
                }
            }
            let sym = script_symbol(loaded, &file_name);
            let label = sym
                .map(|s| s.path.clone())
                .or_else(|| loaded.project.group_for_script(&file_name))
                .unwrap_or_else(|| file_name.trim_end_matches(".m1scr").to_string());
            let rate_hz = sym.and_then(|s| s.call_rate_hz);
            let anchor = Range::default();
            scripts.push(ScriptNode {
                uri,
                file_name,
                label,
                rate_hz,
                anchor,
                reads,
                writes,
            });
        }

        let mut writers: BTreeMap<String, Vec<usize>> = BTreeMap::new();
        let mut readers: BTreeMap<String, Vec<usize>> = BTreeMap::new();
        for (i, s) in scripts.iter().enumerate() {
            for ch in s.writes.keys() {
                writers.entry(ch.clone()).or_default().push(i);
            }
            for ch in s.reads.keys() {
                readers.entry(ch.clone()).or_default().push(i);
            }
        }
        CallGraph {
            scripts,
            writers,
            readers,
        }
    }

    fn script_by_file(&self, file_name: &str) -> Option<&ScriptNode> {
        self.scripts.iter().find(|s| s.file_name == file_name)
    }

    /// Build the LSP item for one script node.
    fn script_item(&self, node: &ScriptNode) -> CallHierarchyItem {
        let detail = match node.rate_hz {
            Some(hz) => format!("{} @ {} Hz", node.label, fmt_hz(hz)),
            None => node.label.clone(),
        };
        CallHierarchyItem {
            name: node.file_name.clone(),
            kind: LspKind::FUNCTION,
            tags: None,
            detail: Some(detail),
            uri: node.uri.clone(),
            range: node.anchor,
            selection_range: node.anchor,
            data: Some(json!({ "k": "s", "file": node.file_name })),
        }
    }
}

fn fmt_hz(hz: f64) -> String {
    if hz.fract() == 0.0 {
        format!("{}", hz as i64)
    } else {
        format!("{hz}")
    }
}

/// Build the LSP item for a channel, anchored at its declaration site.
fn channel_item(loaded: &LoadedProject, path: &str) -> Option<CallHierarchyItem> {
    let sym = loaded.project.symbols().get(path)?;
    let loc = loaded.symbol_location(sym)?;
    let detail = sym.unit.clone().map(|u| format!("channel · {u}"));
    Some(CallHierarchyItem {
        name: path.to_string(),
        kind: LspKind::FIELD,
        tags: None,
        detail: detail.or_else(|| Some("channel".into())),
        uri: loc.uri,
        range: loc.range,
        selection_range: loc.range,
        data: Some(json!({ "k": "c", "path": path })),
    })
}

/// `prepareCallHierarchy`: produce the item the cursor sits on. A channel path
/// under the cursor wins (matches right-clicking a channel); otherwise, when the
/// cursor is in a script file, the script itself becomes the item.
pub fn prepare(
    loaded: &LoadedProject,
    uri: &Url,
    text: &str,
    byte: usize,
    enc: PositionEncoding,
    open_text: &dyn Fn(&Url) -> Option<String>,
) -> Option<Vec<CallHierarchyItem>> {
    let file_name = uri
        .to_file_path()
        .ok()
        .and_then(|p| p.file_name().map(|s| s.to_string_lossy().into_owned()));
    // A channel under the cursor (works from a script or the .m1prj), resolved
    // through the cursor file's scope so a group-relative reference still maps to
    // the canonical channel.
    let cst = m1_core::parse(text);
    // Only a meaningful target under the cursor yields an item — a channel, or a
    // reference to a user function/method (its backing script). A local, keyword,
    // or whitespace yields nothing, instead of mis-attributing the enclosing
    // script's call rate to an unrelated token (#144).
    let (_, path) = crate::features::locate::path_at_byte(cst.root(), byte)?;
    let scope = build_scope(cst.root(), Some(&loaded.project), file_name.as_deref());
    if let Some(canon) = channel_path(&scope, &path)
        && let Some(item) = channel_item(loaded, &canon)
    {
        return Some(vec![item]);
    }
    // A call to a user function/method → that callable's backing script item.
    if let Resolution::Symbol(s) = resolve(&path, &scope)
        && matches!(s.kind, SymbolKind::Function | SymbolKind::Method)
    {
        let graph = CallGraph::build(loaded, enc, open_text);
        if let Some(node) = graph.script_by_file(&backing_file(s)) {
            return Some(vec![graph.script_item(node)]);
        }
    }
    None
}

/// `callHierarchy/incomingCalls`: who depends on this item.
///   - channel → the scripts that read it (from-ranges = their read sites);
///   - script  → the scripts that read a channel this script writes.
pub fn incoming(
    loaded: &LoadedProject,
    item: &CallHierarchyItem,
    enc: PositionEncoding,
    open_text: &dyn Fn(&Url) -> Option<String>,
) -> Option<Vec<CallHierarchyIncomingCall>> {
    let graph = CallGraph::build(loaded, enc, open_text);
    let mut out = Vec::new();
    match item_tag(item)? {
        Tag::Channel(path) => {
            for &i in graph.readers.get(&path)? {
                let node = &graph.scripts[i];
                out.push(CallHierarchyIncomingCall {
                    from: graph.script_item(node),
                    from_ranges: node.reads.get(&path).cloned().unwrap_or_default(),
                });
            }
        }
        Tag::Script(file) => {
            let me = graph.script_by_file(&file)?;
            let my_channels: Vec<&String> = me.writes.keys().collect();
            // caller index → the from-ranges (its read sites of my channels).
            let mut callers: BTreeMap<usize, Vec<Range>> = BTreeMap::new();
            for ch in &my_channels {
                for &i in graph.readers.get(*ch).into_iter().flatten() {
                    if graph.scripts[i].file_name == file {
                        continue; // a script reading its own channel isn't a caller
                    }
                    let ranges = graph.scripts[i].reads.get(*ch).cloned().unwrap_or_default();
                    callers.entry(i).or_default().extend(ranges);
                }
            }
            for (i, from_ranges) in callers {
                out.push(CallHierarchyIncomingCall {
                    from: graph.script_item(&graph.scripts[i]),
                    from_ranges,
                });
            }
        }
    }
    Some(out)
}

/// `callHierarchy/outgoingCalls`: what this item produces/depends on.
///   - script  → the channels it writes (from-ranges = its write sites);
///   - channel → the scripts that write (produce) it.
pub fn outgoing(
    loaded: &LoadedProject,
    item: &CallHierarchyItem,
    enc: PositionEncoding,
    open_text: &dyn Fn(&Url) -> Option<String>,
) -> Option<Vec<CallHierarchyOutgoingCall>> {
    let graph = CallGraph::build(loaded, enc, open_text);
    let mut out = Vec::new();
    match item_tag(item)? {
        Tag::Script(file) => {
            let me = graph.script_by_file(&file)?;
            for (ch, ranges) in &me.writes {
                let Some(to) = channel_item(loaded, ch) else {
                    continue;
                };
                out.push(CallHierarchyOutgoingCall {
                    to,
                    from_ranges: ranges.clone(),
                });
            }
        }
        Tag::Channel(path) => {
            for &i in graph.writers.get(&path)? {
                let node = &graph.scripts[i];
                // The channel item has no body; anchor the call at its decl.
                out.push(CallHierarchyOutgoingCall {
                    to: graph.script_item(node),
                    from_ranges: vec![item.selection_range],
                });
            }
        }
    }
    Some(out)
}

enum Tag {
    Channel(String),
    Script(String),
}

/// Decode the `data` tag an item was built with.
fn item_tag(item: &CallHierarchyItem) -> Option<Tag> {
    let data = item.data.as_ref()?;
    match data.get("k")?.as_str()? {
        "c" => Some(Tag::Channel(data.get("path")?.as_str()?.to_string())),
        "s" => Some(Tag::Script(data.get("file")?.as_str()?.to_string())),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::project_store::ProjectStore;
    use std::io::Write;

    /// A project with three scripts: Control writes Speed; Dash and Safety read it.
    fn fixture() -> (tempfile::TempDir, ProjectStore) {
        let tmp = tempfile::tempdir().unwrap();
        let m1prj = r#"<?xml version="1.0"?>
<Project>
  <Component Classname="BuiltIn.GroupCompound" Name="Root"/>
  <Component Classname="BuiltIn.GroupCompound" Name="Root.Engine"/>
  <Component Classname="BuiltIn.Channel" Name="Root.Engine.Speed"><Props Type="f32"/></Component>
  <Component Classname="BuiltIn.MethodUser" Name="Root.Engine.Control"/>
  <Component Classname="BuiltIn.MethodUser" Name="Root.Engine.Dash"/>
  <Component Classname="BuiltIn.MethodUser" Name="Root.Engine.Safety"/>
</Project>"#;
        std::fs::File::create(tmp.path().join("Project.m1prj"))
            .unwrap()
            .write_all(m1prj.as_bytes())
            .unwrap();
        let scripts = tmp.path().join("Scripts");
        std::fs::create_dir_all(&scripts).unwrap();
        std::fs::write(
            scripts.join("Engine.Control.m1scr"),
            "Root.Engine.Speed = 1.0;\n",
        )
        .unwrap();
        std::fs::write(
            scripts.join("Engine.Dash.m1scr"),
            "local a = Root.Engine.Speed;\n",
        )
        .unwrap();
        std::fs::write(
            scripts.join("Engine.Safety.m1scr"),
            "local b = Root.Engine.Speed + 1.0;\n",
        )
        .unwrap();
        let store = ProjectStore::new();
        store.discover_and_load(tmp.path()).unwrap();
        (tmp, store)
    }

    fn no_open(_: &Url) -> Option<String> {
        None
    }

    #[test]
    fn index_classifies_reads_and_writes() {
        let (_t, store) = fixture();
        store.with_project(|p| {
            let g = CallGraph::build(p.unwrap(), PositionEncoding::Utf16, &no_open);
            assert_eq!(g.writers.get("Root.Engine.Speed").unwrap().len(), 1);
            assert_eq!(g.readers.get("Root.Engine.Speed").unwrap().len(), 2);
        });
    }

    #[test]
    fn channel_incoming_lists_readers() {
        let (_t, store) = fixture();
        store.with_project(|p| {
            let lp = p.unwrap();
            let item = channel_item(lp, "Root.Engine.Speed").unwrap();
            let calls = incoming(lp, &item, PositionEncoding::Utf16, &no_open).unwrap();
            let names: Vec<_> = calls.iter().map(|c| c.from.name.clone()).collect();
            assert!(names.contains(&"Engine.Dash.m1scr".to_string()));
            assert!(names.contains(&"Engine.Safety.m1scr".to_string()));
            assert_eq!(calls.len(), 2, "two readers");
            // Each reader contributes its read-site range.
            assert!(calls.iter().all(|c| !c.from_ranges.is_empty()));
        });
    }

    #[test]
    fn channel_outgoing_lists_writers() {
        let (_t, store) = fixture();
        store.with_project(|p| {
            let lp = p.unwrap();
            let item = channel_item(lp, "Root.Engine.Speed").unwrap();
            let calls = outgoing(lp, &item, PositionEncoding::Utf16, &no_open).unwrap();
            assert_eq!(calls.len(), 1, "one writer");
            assert_eq!(calls[0].to.name, "Engine.Control.m1scr");
        });
    }

    #[test]
    fn script_outgoing_lists_written_channels() {
        let (_t, store) = fixture();
        store.with_project(|p| {
            let lp = p.unwrap();
            let g = CallGraph::build(lp, PositionEncoding::Utf16, &no_open);
            let ctrl = g.script_by_file("Engine.Control.m1scr").unwrap();
            let item = g.script_item(ctrl);
            let calls = outgoing(lp, &item, PositionEncoding::Utf16, &no_open).unwrap();
            assert_eq!(calls.len(), 1);
            assert_eq!(calls[0].to.name, "Root.Engine.Speed");
            assert!(!calls[0].from_ranges.is_empty(), "write site range");
        });
    }

    #[test]
    fn script_incoming_lists_downstream_readers() {
        let (_t, store) = fixture();
        store.with_project(|p| {
            let lp = p.unwrap();
            let g = CallGraph::build(lp, PositionEncoding::Utf16, &no_open);
            let ctrl = g.script_by_file("Engine.Control.m1scr").unwrap();
            let item = g.script_item(ctrl);
            let calls = incoming(lp, &item, PositionEncoding::Utf16, &no_open).unwrap();
            let names: Vec<_> = calls.iter().map(|c| c.from.name.clone()).collect();
            assert!(names.contains(&"Engine.Dash.m1scr".to_string()));
            assert!(names.contains(&"Engine.Safety.m1scr".to_string()));
            assert_eq!(calls.len(), 2);
        });
    }

    #[test]
    fn prepare_on_channel_path_yields_channel_item() {
        let (_t, store) = fixture();
        let scripts_dir = _t.path().join("Scripts");
        let uri = Url::from_file_path(scripts_dir.join("Engine.Control.m1scr")).unwrap();
        let text = "Root.Engine.Speed = 1.0;\n";
        store.with_project(|p| {
            let items = prepare(
                p.unwrap(),
                &uri,
                text,
                0, // cursor on `Root.Engine.Speed`
                PositionEncoding::Utf16,
                &no_open,
            )
            .unwrap();
            assert_eq!(items.len(), 1);
            assert_eq!(items[0].name, "Root.Engine.Speed");
            assert_eq!(items[0].kind, LspKind::FIELD);
        });
    }

    #[test]
    fn prepare_on_a_local_yields_no_hierarchy() {
        // #144: the cursor on a local (or keyword/whitespace) must NOT fall back
        // to the enclosing script — that mis-attributes the script's call rate to
        // an unrelated token. Only channels and callable references get an item.
        let (_t, store) = fixture();
        let uri =
            Url::from_file_path(_t.path().join("Scripts").join("Engine.Control.m1scr")).unwrap();
        let text = "local myValue = 42;\n";
        store.with_project(|p| {
            // Cursor on `myValue` (byte 6).
            let got = prepare(p.unwrap(), &uri, text, 6, PositionEncoding::Utf16, &no_open);
            assert!(
                got.is_none(),
                "a local should yield no call hierarchy: {got:?}"
            );
        });
    }
}
