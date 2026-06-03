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
use crate::features::call_hierarchy::script_symbol;
use crate::project_store::LoadedProject;
use tower_lsp::lsp_types::{CodeLens, Command, Position, Range, Url};

/// A single code lens at line 0 of the script naming its execution rate, or an
/// empty vec when the file is not a known script or its rate is not statically
/// known (no trigger, or a `$(…)`-templated trigger the model can't resolve).
pub fn code_lens(loaded: &LoadedProject, uri: &Url) -> Vec<CodeLens> {
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
    let Some(rate) = script_symbol(loaded, &file_name).and_then(|s| s.call_rate_hz) else {
        return Vec::new();
    };
    let title = format!("⚡ {} Hz", fmt_hz(rate));
    vec![CodeLens {
        range: Range::new(Position::new(0, 0), Position::new(0, 0)),
        command: Some(Command {
            title,
            command: String::new(), // informational; not clickable
            arguments: None,
        }),
        data: None,
    }]
}

fn fmt_hz(hz: f64) -> String {
    if hz.fract() == 0.0 {
        format!("{}", hz as i64)
    } else {
        format!("{hz}")
    }
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
            let lenses = code_lens(p.unwrap(), &uri);
            assert_eq!(lenses.len(), 1);
            assert_eq!(lenses[0].command.as_ref().unwrap().title, "⚡ 500 Hz");
            assert_eq!(lenses[0].range.start.line, 0);
        });
    }

    #[test]
    fn no_lens_for_startup_only_script() {
        // Boot has no SelectedTrigger → no statically-known rate → no lens.
        let (t, store) = load(M1PRJ, "Engine.Boot.m1scr", "x = 1;\n");
        let uri = Url::from_file_path(t.path().join("Scripts/Engine.Boot.m1scr")).unwrap();
        store.with_project(|p| assert!(code_lens(p.unwrap(), &uri).is_empty()));
    }

    #[test]
    fn no_lens_for_non_script_uri() {
        let (t, store) = load(M1PRJ, "Engine.Update.m1scr", "x = 1;\n");
        let uri = Url::from_file_path(t.path().join("Project.m1prj")).unwrap();
        store.with_project(|p| assert!(code_lens(p.unwrap(), &uri).is_empty()));
    }
}
