//! textDocument/definition: jump to a symbol's definition site — a script/DBC
//! file for file-backed symbols (FuncUser/MethodUser, DBC signals), or the
//! `.m1prj` at the declaring `<Component>` line for project objects (channels,
//! parameters, groups, tables, references, package objects).
use crate::features::locate::{build_scope, path_at_byte};
use crate::project_store::LoadedProject;
use m1_typecheck::resolve::{Resolution, resolve};
use tower_lsp::lsp_types::{Location, Position, Range, Url};

/// Resolve the path at `byte` to a symbol and return its definition Location.
/// File-backed symbols open their backing file at its start; other project
/// symbols open the `.m1prj` at their declaration line. `None` if the path does
/// not resolve to a symbol or no definition site is known.
pub fn goto(
    root: m1_core::Node,
    byte: usize,
    loaded: &LoadedProject,
    file_name: Option<&str>,
) -> Option<Location> {
    let (_, path) = path_at_byte(root, byte)?;
    let scope = build_scope(root, Some(&loaded.project), file_name);
    let Resolution::Symbol(sym) = resolve(&path, &scope) else {
        return None;
    };
    let (target, line) = match &sym.filename {
        // Script/DBC-backed: open the body file at its start.
        Some(f) => (loaded.root.join(f), 0),
        // Project object: jump to its declaration line in the .m1prj (#31).
        None => (loaded.m1prj_path.clone(), sym.def_line?),
    };
    let uri = Url::from_file_path(&target).ok()?;
    Some(Location {
        uri,
        range: Range::new(Position::new(line, 0), Position::new(line, 0)),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::project_store::ProjectStore;
    use std::io::Write;

    const M1PRJ: &str = r#"<?xml version="1.0"?>
<Project>
  <Component Classname="BuiltIn.GroupCompound" Name="Root"/>
  <Component Classname="BuiltIn.FuncUser" Name="Root.Do Thing" Filename="Do Thing.m1scr"/>
</Project>"#;

    #[test]
    fn goto_dbc_object_returns_its_m1dbc_file() {
        let tmp = tempfile::tempdir().unwrap();
        let prj = tmp.path().join("Project.m1prj");
        std::fs::File::create(&prj)
            .unwrap()
            .write_all(br#"<?xml version="1.0"?><Project><Component Classname="BuiltIn.GroupCompound" Name="Root"/></Project>"#)
            .unwrap();
        let dbcdir = tmp.path().join("dbc");
        std::fs::create_dir_all(&dbcdir).unwrap();
        std::fs::File::create(dbcdir.join("Balls3EV25.m1dbc"))
            .unwrap()
            .write_all(
                br#"<?xml version="1.0"?>
<DBC><ComponentStream><List>
  <Component Classname="BuiltIn.CAN.DBC" Name="Balls3EV25"/>
  <Component Classname="BuiltIn.CAN.Message" Name="Balls3EV25.DashVals"/>
  <Component Classname="BuiltIn.CAN.Signal" Name="Balls3EV25.DashVals.Inverter Error"><Props Type="u32"/></Component>
</List></ComponentStream></DBC>"#,
            )
            .unwrap();
        let store = ProjectStore::new();
        store.discover_and_load(tmp.path()).unwrap();

        // A direct reference to a DBC signal resolves to its symbol, so goto
        // opens the defining .m1dbc. (`.Init()`-style accessor calls stay opaque
        // by design — path_at_byte resolves the whole member expression.)
        let src = "Balls3EV25.DashVals.Inverter Error = 1;\n";
        let cst = m1_core::parse(src);
        store.with_project(|p| {
            let loc = goto(cst.root(), 0, p.unwrap(), Some("CAN.DBC Init.m1scr"))
                .expect("DBC signal should resolve to its .m1dbc file");
            let fs = loc.uri.to_file_path().unwrap();
            assert!(fs.ends_with("Balls3EV25.m1dbc"), "got {fs:?}");
        });
    }

    #[test]
    fn goto_func_returns_its_file() {
        let tmp = tempfile::tempdir().unwrap();
        let prj = tmp.path().join("Project.m1prj");
        std::fs::File::create(&prj)
            .unwrap()
            .write_all(M1PRJ.as_bytes())
            .unwrap();
        let store = ProjectStore::new();
        store.discover_and_load(tmp.path()).unwrap();

        let src = "Do Thing();\n";
        let cst = m1_core::parse(src);
        let byte = 0;
        store.with_project(|p| {
            let loc = goto(cst.root(), byte, p.unwrap(), Some("Caller.m1scr"));
            let loc = loc.expect("function should resolve to its file");
            // `Url::path()` percent-encodes spaces; compare the decoded fs path.
            let fs = loc.uri.to_file_path().unwrap();
            assert!(fs.ends_with("Do Thing.m1scr"), "got {fs:?}");
        });
    }

    #[test]
    fn goto_channel_returns_m1prj_at_definition_line() {
        let tmp = tempfile::tempdir().unwrap();
        let prj = tmp.path().join("Project.m1prj");
        // 0-based: line 0 = <?xml>, 1 = <Project>, 2 = Root, 3 = Root.Speed.
        let xml = "<?xml version=\"1.0\"?>\n<Project>\n  <Component Classname=\"BuiltIn.GroupCompound\" Name=\"Root\"/>\n  <Component Classname=\"BuiltIn.Channel\" Name=\"Root.Speed\"><Props Type=\"f32\"/></Component>\n</Project>";
        std::fs::File::create(&prj)
            .unwrap()
            .write_all(xml.as_bytes())
            .unwrap();
        let store = ProjectStore::new();
        store.discover_and_load(tmp.path()).unwrap();

        let src = "Root.Speed = 1.0;\n";
        let cst = m1_core::parse(src);
        store.with_project(|p| {
            let loc = goto(cst.root(), 0, p.unwrap(), Some("X.m1scr"))
                .expect("channel should resolve to the .m1prj");
            let fs = loc.uri.to_file_path().unwrap();
            assert!(fs.ends_with("Project.m1prj"), "got {fs:?}");
            assert_eq!(loc.range.start.line, 3, "should point at the declaration line");
        });
    }
}
