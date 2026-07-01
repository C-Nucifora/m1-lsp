//! textDocument/codeLens: the execution rate of a `.m1scr` script (#86, view half).
//!
//! A script runs on the clock named by its `.m1prj` `<Props SelectedTrigger=…>`
//! (see #76). When that resolves to a definite rate, surface it as a code lens at
//! the top of the file — `⚡ 100 Hz` — so a reader sees how often the script
//! executes without opening the project file. The lens is informational
//! (title-only, no command).
//!
//! Only the *view* is implemented here. Changing the rate mutates `Project.m1prj`
//! and, by maintainer decision (#86), belongs in the editor/CLI layer, not the
//! LSP — so there is deliberately no write-back.
//!
//! Two sibling badges join the rate lens:
//! - `📊` logging (#171): the channels this script writes that carry a
//!   `DefaultLogRate` — what of this script's output actually reaches the log.
//! - `🔒` security (#172): the script's own access level and/or the distinct
//!   security levels of the channels it writes — the per-script row of the
//!   access matrix (the full matrix view is the editor's half, m1-vscode#78).
use crate::features::call_hierarchy::script_symbol;
use crate::features::locate::{build_scope, fmt_hz};
use crate::features::references::path_occurrences;
use crate::project_store::LoadedProject;
use m1_typecheck::resolve::{Resolution, resolve};
use m1_typecheck::symbols::{Symbol, SymbolKind};
use tower_lsp::lsp_types::{CodeLens, Command, Position, Range, Url};

/// The line-0 informational badges for a script: execution rate (#86),
/// logging (#171) and security (#172). `text` is the script's current buffer
/// (open document or disk), used to resolve the channels it writes; an empty
/// vec when the file is not a known script.
pub fn code_lens(loaded: &LoadedProject, uri: &Url, text: Option<&str>) -> Vec<CodeLens> {
    let Some(file_name) = uri
        .to_file_path()
        .ok()
        .and_then(|p| p.file_name().map(|s| s.to_string_lossy().into_owned()))
    else {
        return Vec::new();
    };
    if !file_name.ends_with(".m1scr") {
        return Vec::new();
    }
    let Some(sym) = script_symbol(loaded, &file_name) else {
        return Vec::new();
    };
    let mut lenses = Vec::new();
    if let Some(rate) = sym.call_rate_hz {
        lenses.push(rate_lens(loaded, sym, rate));
    }
    if let Some(text) = text {
        let written = written_symbols(loaded, &file_name, text);
        if let Some(l) = logging_lens(loaded, &written) {
            lenses.push(l);
        }
        if let Some(l) = security_lens(loaded, sym, &written) {
            lenses.push(l);
        }
    }
    lenses
}

fn rate_lens(loaded: &LoadedProject, sym: &Symbol, rate: f64) -> CodeLens {
    let title = format!("⚡ {} Hz", fmt_hz(rate));
    // Make the badge clickable (#175): navigate to the script's own `<Component>`
    // line in Project.m1prj, where its `<Props SelectedTrigger=…>` (the clock that
    // set this rate) is declared. The command id `m1.revealLocation` is registered
    // CLIENT-SIDE in m1-vscode and nvim-m1 — deliberately NOT via the server's
    // executeCommandProvider, which the first attempt used and had to revert:
    // vscode-languageclient registers an executeCommand handler per LanguageClient,
    // and m1-vscode runs one client per project root, so the second registration
    // collided and the client never reached the running state. A client-registered
    // command has no such per-client collision. The target is computed eagerly here
    // (it's free — we already have the symbol), so no codeLens/resolve round-trip
    // is needed. When the trigger line is unknown, fall back to a title-only badge.
    let command = sym
        .def_line
        .and_then(|line| {
            let uri = Url::from_file_path(&loaded.m1prj_path).ok()?;
            Some(Command {
                title: title.clone(),
                command: "m1.revealLocation".to_string(),
                arguments: Some(vec![
                    serde_json::Value::String(uri.to_string()),
                    serde_json::Value::from(line),
                ]),
            })
        })
        .unwrap_or(Command {
            title,
            command: String::new(),
            arguments: None,
        });
    lens_at_top(command)
}

fn lens_at_top(command: Command) -> CodeLens {
    CodeLens {
        range: Range::new(Position::new(0, 0), Position::new(0, 0)),
        command: Some(command),
        data: None,
    }
}

/// The distinct project channel/parameter symbols this script writes, resolved
/// through the typechecker so group-relative spellings canonicalise.
fn written_symbols<'a>(loaded: &'a LoadedProject, file_name: &str, text: &str) -> Vec<&'a Symbol> {
    let cst = m1_core::parse(text);
    if !cst.syntax_diagnostics().is_empty() {
        return Vec::new();
    }
    let scope = build_scope(cst.root(), Some(&loaded.project), Some(file_name));
    let mut seen = std::collections::BTreeSet::new();
    let mut out = Vec::new();
    for (path, _, is_write) in path_occurrences(cst.root()) {
        if !is_write {
            continue;
        }
        if let Resolution::Symbol(s) = resolve(&path, &scope)
            && matches!(s.kind, SymbolKind::Channel | SymbolKind::Parameter)
            && seen.insert(s.path.clone())
        {
            out.push(s);
        }
    }
    out
}

/// `📊` badge (#171): the written channels that carry a `DefaultLogRate`.
fn logging_lens(loaded: &LoadedProject, written: &[&Symbol]) -> Option<CodeLens> {
    let logged: Vec<&&Symbol> = written.iter().filter(|s| s.log_rate_hz.is_some()).collect();
    let first = logged.first()?;
    let title = if logged.len() == 1 {
        format!(
            "📊 logs {} @ {} Hz",
            leaf(&first.path),
            fmt_hz(first.log_rate_hz.unwrap_or(0.0))
        )
    } else {
        format!("📊 logs {} channels", logged.len())
    };
    Some(lens_at_top(reveal_command(loaded, title, first.def_line)))
}

/// `🔒` badge (#172): the script's own access level, else the distinct levels
/// of the channels it writes.
fn security_lens(loaded: &LoadedProject, sym: &Symbol, written: &[&Symbol]) -> Option<CodeLens> {
    if let Some(own) = &sym.security {
        return Some(lens_at_top(reveal_command(
            loaded,
            format!("🔒 {own}"),
            sym.def_line,
        )));
    }
    let mut levels: Vec<&str> = written
        .iter()
        .filter_map(|s| s.security.as_deref())
        .collect();
    levels.sort_unstable();
    levels.dedup();
    if levels.is_empty() {
        return None;
    }
    let first = written.iter().find(|s| s.security.is_some())?;
    Some(lens_at_top(reveal_command(
        loaded,
        format!("🔒 writes {}", levels.join(", ")),
        first.def_line,
    )))
}

/// A `m1.revealLocation` command into Project.m1prj at `line`, falling back to
/// a title-only badge when the line is unknown.
fn reveal_command(loaded: &LoadedProject, title: String, line: Option<u32>) -> Command {
    line.and_then(|line| {
        let uri = Url::from_file_path(&loaded.m1prj_path).ok()?;
        Some(Command {
            title: title.clone(),
            command: "m1.revealLocation".to_string(),
            arguments: Some(vec![
                serde_json::Value::String(uri.to_string()),
                serde_json::Value::from(line),
            ]),
        })
    })
    .unwrap_or(Command {
        title,
        command: String::new(),
        arguments: None,
    })
}

fn leaf(path: &str) -> &str {
    path.rsplit_once('.').map(|(_, l)| l).unwrap_or(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::project_store::ProjectStore;
    use std::io::Write;

    fn load(m1prj: &str, script: &str, src: &str) -> (tempfile::TempDir, ProjectStore) {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::File::create(tmp.path().join("Project.m1prj"))
            .unwrap()
            .write_all(m1prj.as_bytes())
            .unwrap();
        let scripts = tmp.path().join("Scripts");
        std::fs::create_dir_all(&scripts).unwrap();
        std::fs::write(scripts.join(script), src).unwrap();
        let store = ProjectStore::new();
        store.discover_and_load(tmp.path()).unwrap();
        (tmp, store)
    }

    // A method whose SelectedTrigger points at a 500 Hz EventKernel clock.
    const M1PRJ: &str = r#"<?xml version="1.0"?>
<Project>
  <Component Classname="BuiltIn.GroupCompound" Name="Root"/>
  <Component Classname="BuiltIn.GroupCompound" Name="Root.Engine"/>
  <Component Classname="BuiltIn.EventKernel" Name="Root.Events.On 500Hz"/>
  <Component Classname="BuiltIn.MethodUser" Name="Root.Engine.Update">
    <Props SelectedTrigger="Parent.Parent.Events.On 500Hz"/>
  </Component>
  <Component Classname="BuiltIn.MethodUser" Name="Root.Engine.Boot"/>
</Project>"#;

    #[test]
    fn lens_shows_rate_for_triggered_script() {
        let (t, store) = load(M1PRJ, "Engine.Update.m1scr", "x = 1;\n");
        let uri = Url::from_file_path(t.path().join("Scripts/Engine.Update.m1scr")).unwrap();
        store.with_project(|p| {
            let lenses = code_lens(p.unwrap(), &uri, Some("x = 1;\n"));
            assert_eq!(lenses.len(), 1);
            assert_eq!(lenses[0].command.as_ref().unwrap().title, "⚡ 500 Hz");
            assert_eq!(lenses[0].range.start.line, 0);
        });
    }

    // #175: the rate lens is clickable — its command navigates to the script's
    // own `<Component>` line in Project.m1prj (where `SelectedTrigger=` lives).
    // The command id is client-registered (`m1.revealLocation`), NOT advertised
    // via the server's executeCommandProvider (that collided across the per-root
    // clients in multi-root VS Code, which is why the first attempt was reverted).
    #[test]
    fn lens_is_clickable_to_project_trigger_line() {
        let (t, store) = load(M1PRJ, "Engine.Update.m1scr", "x = 1;\n");
        let uri = Url::from_file_path(t.path().join("Scripts/Engine.Update.m1scr")).unwrap();
        store.with_project(|p| {
            let lenses = code_lens(p.unwrap(), &uri, Some("x = 1;\n"));
            let cmd = lenses[0].command.as_ref().unwrap();
            assert_eq!(cmd.command, "m1.revealLocation");
            let args = cmd.arguments.as_ref().expect("reveal args");
            // arg 0 = the Project.m1prj file URI; arg 1 = Root.Engine.Update's
            // 0-based declaration line (5 in M1PRJ above).
            let target = args[0].as_str().unwrap();
            assert!(target.ends_with("Project.m1prj"), "target uri: {target}");
            assert_eq!(args[1].as_u64().unwrap(), 5);
        });
    }

    #[test]
    fn no_lens_for_startup_only_script() {
        // Boot has no SelectedTrigger → no statically-known rate → no lens.
        let (t, store) = load(M1PRJ, "Engine.Boot.m1scr", "x = 1;\n");
        let uri = Url::from_file_path(t.path().join("Scripts/Engine.Boot.m1scr")).unwrap();
        store.with_project(|p| assert!(code_lens(p.unwrap(), &uri, Some("x = 1;\n")).is_empty()));
    }

    #[test]
    fn no_lens_for_non_script_uri() {
        let (t, store) = load(M1PRJ, "Engine.Update.m1scr", "x = 1;\n");
        let uri = Url::from_file_path(t.path().join("Project.m1prj")).unwrap();
        store.with_project(|p| assert!(code_lens(p.unwrap(), &uri, Some("x = 1;\n")).is_empty()));
    }
}

#[cfg(test)]
mod badge_tests {
    use super::*;
    use crate::project_store::ProjectStore;
    use std::io::Write;

    const M1PRJ: &str = r#"<?xml version="1.0"?>
<Project>
  <Component Classname="BuiltIn.GroupCompound" Name="Root"/>
  <Component Classname="BuiltIn.GroupCompound" Name="Root.Engine"/>
  <Component Classname="BuiltIn.EventKernel" Name="Root.Events.On 100Hz"/>
  <Component Classname="BuiltIn.Channel" Name="Root.Engine.Speed"><Props Type="f32" DefaultLogRate="5MS"/></Component>
  <Component Classname="BuiltIn.Channel" Name="Root.Engine.Temp"><Props Type="f32" Security="Calibration"/></Component>
  <Component Classname="BuiltIn.Channel" Name="Root.Engine.Plain"><Props Type="f32"/></Component>
  <Component Classname="BuiltIn.MethodUser" Name="Root.Engine.Update">
    <Props SelectedTrigger="Parent.Parent.Events.On 100Hz"/>
  </Component>
</Project>"#;

    fn load(src: &str) -> (tempfile::TempDir, ProjectStore) {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::File::create(tmp.path().join("Project.m1prj"))
            .unwrap()
            .write_all(M1PRJ.as_bytes())
            .unwrap();
        let scripts = tmp.path().join("Scripts");
        std::fs::create_dir_all(&scripts).unwrap();
        std::fs::write(scripts.join("Engine.Update.m1scr"), src).unwrap();
        let store = ProjectStore::new();
        store.discover_and_load(tmp.path()).unwrap();
        (tmp, store)
    }

    fn titles(store: &ProjectStore, dir: &std::path::Path, src: &str) -> Vec<String> {
        let uri = Url::from_file_path(dir.join("Scripts/Engine.Update.m1scr")).unwrap();
        store.with_project(|p| {
            code_lens(p.unwrap(), &uri, Some(src))
                .iter()
                .filter_map(|l| l.command.as_ref().map(|c| c.title.clone()))
                .collect()
        })
    }

    // #171: a script writing a logged channel gets the 📊 badge.
    #[test]
    fn logging_badge_for_logged_channel_write() {
        let src = "Speed = 1.0;\n";
        let (t, store) = load(src);
        let got = titles(&store, t.path(), src);
        assert!(
            got.iter().any(|t| t.starts_with("📊 logs Speed @ ")),
            "got {got:?}"
        );
    }

    // #172: writing a security-tagged channel gets the 🔒 badge.
    #[test]
    fn security_badge_for_secured_channel_write() {
        let src = "Temp = 1.0;\n";
        let (t, store) = load(src);
        let got = titles(&store, t.path(), src);
        assert!(
            got.contains(&"🔒 writes Calibration".to_string()),
            "got {got:?}"
        );
    }

    #[test]
    fn no_badges_for_plain_writes() {
        let src = "Plain = 1.0;\n";
        let (t, store) = load(src);
        let got = titles(&store, t.path(), src);
        assert!(
            got.iter()
                .all(|t| !t.starts_with("📊") && !t.starts_with("🔒")),
            "got {got:?}"
        );
        // The rate badge is still there.
        assert!(
            got.iter().any(|t| t.starts_with("⚡ 100 Hz")),
            "got {got:?}"
        );
    }
}
