//! textDocument/definition: jump to a symbol's definition site — a script/DBC
//! file for file-backed symbols (FuncUser/MethodUser, DBC signals), or the
//! `.m1prj` at the declaring `<Component>` line for project objects (channels,
//! parameters, groups, tables, references, package objects).
use crate::convert::range;
use crate::features::locate::{build_scope, path_at_byte};
use crate::line_index::{LineIndex, PositionEncoding};
use crate::project_store::{LoadedProject, contained_join};
use m1_core::Kind;
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
        // Script/DBC-backed: open the body file at its start. The Filename comes
        // from the (untrusted) .m1prj, so reject any value that escapes the
        // project root rather than open an out-of-tree file (#134).
        Some(f) => (contained_join(&loaded.root, f)?, 0),
        // Project object: jump to its declaration line in the .m1prj (#31).
        None => (loaded.m1prj_path.clone(), sym.def_line?),
    };
    let uri = Url::from_file_path(&target).ok()?;
    Some(Location {
        uri,
        range: Range::new(Position::new(line, 0), Position::new(line, 0)),
    })
}

/// textDocument/typeDefinition: from an enum-typed channel/parameter, jump to the
/// `<Type … Name="…">` block that declares its enum in the `.m1prj` (#168). The
/// enum *type* isn't a project component (it lives in `<DataTypes>`, off the
/// channel table), so its line is located by scanning the project XML for the
/// `<Type>` element with the matching `Name`. `None` for non-enum symbols.
pub fn goto_type_definition(
    root: m1_core::Node,
    byte: usize,
    loaded: &LoadedProject,
    file_name: Option<&str>,
) -> Option<Location> {
    use m1_typecheck::types::ValueType;
    let (_, path) = path_at_byte(root, byte)?;
    let scope = build_scope(root, Some(&loaded.project), file_name);
    let Resolution::Symbol(sym) = resolve(&path, &scope) else {
        return None;
    };
    let ValueType::Enum(id) = sym.value_type else {
        return None;
    };
    let enum_name = loaded.project.symbols().enum_type(id).name.clone();
    let xml = crate::disk_read::read_disk(&loaded.m1prj_path)?;
    let line = enum_type_decl_line(&xml, &enum_name)?;
    let uri = Url::from_file_path(&loaded.m1prj_path).ok()?;
    Some(Location {
        uri,
        range: Range::new(Position::new(line, 0), Position::new(line, 0)),
    })
}

/// The 0-based line of the `<Type … Name="<enum>">` declaration in a `.m1prj`,
/// or `None` if absent. Requires `<Type` on the line so a same-named
/// `<Component>` channel can't match.
fn enum_type_decl_line(xml: &str, enum_name: &str) -> Option<u32> {
    let needle = format!("Name=\"{enum_name}\"");
    xml.lines()
        .position(|line| line.contains("<Type") && line.contains(&needle))
        .map(|i| i as u32)
}

/// If `byte` sits on a bare reference to a `local` variable, the [`Location`] of
/// that local's `local <name> = …` declaration in the same file. M1 locals are
/// file-scoped (and their names may contain spaces), so this is a pure in-file
/// lookup that needs no project — covering the project-less case too (#141).
///
/// When a name is declared more than once (shadowing / re-declaration), the
/// nearest declaration at or before the cursor wins; otherwise the first.
pub fn goto_local(
    root: m1_core::Node,
    byte: usize,
    uri: &Url,
    li: &LineIndex,
    enc: PositionEncoding,
) -> Option<Location> {
    let (_, path) = path_at_byte(root, byte)?;
    // A dotted path is a channel/member access, never a local.
    if path.contains('.') {
        return None;
    }
    // The identifier node of every `local <name> = …` whose name matches.
    let mut decls: Vec<m1_core::Node> = root
        .descendants()
        .filter(|n| n.kind() == Kind::LocalDeclaration)
        .filter_map(|n| {
            n.named_children()
                .into_iter()
                .find(|c| c.kind() == Kind::Identifier)
        })
        .filter(|id| id.text() == path)
        .collect();
    if decls.is_empty() {
        return None;
    }
    decls.sort_by_key(|id| id.byte_range().start);
    // Nearest declaration at or before the cursor, else the first one.
    let id = decls
        .iter()
        .rev()
        .find(|id| id.byte_range().start <= byte)
        .copied()
        .unwrap_or(decls[0]);
    Some(Location {
        uri: uri.clone(),
        range: range(&id.byte_range(), li, enc),
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
    fn goto_local_returns_its_declaration_in_file() {
        use crate::line_index::{LineIndex, PositionEncoding};
        // A local used after its declaration; goto from the use-site should land
        // on the declaration line in the same file (#141). No project needed.
        let src = "local myValue = 0;\nmyValue = myValue + 1;\n";
        let cst = m1_core::parse(src);
        let li = LineIndex::new(src);
        let uri = Url::parse("file:///x.m1scr").unwrap();

        // use-site on line 1 (the assignment target `myValue`).
        let use_byte = src.find("myValue = myValue").unwrap();
        let loc = goto_local(cst.root(), use_byte, &uri, &li, PositionEncoding::Utf16)
            .expect("a local use should resolve to its declaration");
        assert_eq!(loc.uri, uri);
        assert_eq!(loc.range.start.line, 0, "declaration is on line 0");

        // from the declaration itself it still resolves (idempotent).
        let decl_byte = src.find("myValue").unwrap();
        assert!(
            goto_local(cst.root(), decl_byte, &uri, &li, PositionEncoding::Utf16).is_some(),
            "goto on the declaration should also resolve"
        );
    }

    #[test]
    fn goto_local_ignores_dotted_paths() {
        use crate::line_index::{LineIndex, PositionEncoding};
        let src = "Root.Speed = 1.0;\n";
        let cst = m1_core::parse(src);
        let li = LineIndex::new(src);
        let uri = Url::parse("file:///x.m1scr").unwrap();
        // `Root.Speed` is a dotted channel path, never a local.
        assert!(
            goto_local(cst.root(), 0, &uri, &li, PositionEncoding::Utf16).is_none(),
            "a dotted path is not a local"
        );
    }

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
            assert_eq!(
                loc.range.start.line, 3,
                "should point at the declaration line"
            );
        });
    }

    #[test]
    fn type_definition_jumps_to_enum_type_block() {
        // #168: Go to Type Definition from an enum-typed channel lands on the
        // `<Type … Name="…">` declaration in the .m1prj.
        let tmp = tempfile::tempdir().unwrap();
        let prj = tmp.path().join("Project.m1prj");
        // line 0 <?xml>, 1 <Project>, 2 <DataTypes>, 3 <Type Name="Color">.
        let xml = "<?xml version=\"1.0\"?>\n<Project>\n  <DataTypes>\n    <Type Name=\"Color\" Storage=\"enum\"><Enum Name=\"Red\" ContainerOrder=\"1\"/></Type>\n  </DataTypes>\n  <Component Classname=\"BuiltIn.GroupCompound\" Name=\"Root\"/>\n  <Component Classname=\"BuiltIn.Channel\" Name=\"Root.Mode\"><Props Type=\"::This.Color\"/></Component>\n</Project>";
        std::fs::File::create(&prj)
            .unwrap()
            .write_all(xml.as_bytes())
            .unwrap();
        let store = ProjectStore::new();
        store.discover_and_load(tmp.path()).unwrap();

        let src = "Root.Mode = Color.Red;\n";
        let cst = m1_core::parse(src);
        store.with_project(|p| {
            let loc = goto_type_definition(cst.root(), 0, p.unwrap(), Some("X.m1scr"))
                .expect("enum-typed channel should resolve to its <Type> block");
            assert!(loc.uri.to_file_path().unwrap().ends_with("Project.m1prj"));
            assert_eq!(loc.range.start.line, 3, "the <Type Name=\"Color\"> line");
        });
    }

    #[test]
    fn contained_join_rejects_absolute_and_parent_escapes() {
        use std::path::Path;
        let root = Path::new("/home/user/project");
        assert!(contained_join(root, "/etc/passwd").is_none());
        assert!(contained_join(root, "../../../../etc/passwd").is_none());
        assert!(contained_join(root, "sub/../../etc").is_none());
        assert_eq!(
            contained_join(root, "dbc/Foo.m1dbc"),
            Some(root.join("dbc/Foo.m1dbc"))
        );
        assert_eq!(
            contained_join(root, "Do Thing.m1scr"),
            Some(root.join("Do Thing.m1scr"))
        );
    }

    #[test]
    fn goto_rejects_out_of_tree_filename() {
        // #134: a `.m1prj` Filename pointing outside the project (absolute here)
        // must not yield a goto Location the editor would open.
        let tmp = tempfile::tempdir().unwrap();
        let prj = tmp.path().join("Project.m1prj");
        std::fs::File::create(&prj)
            .unwrap()
            .write_all(
                br#"<?xml version="1.0"?>
<Project>
  <Component Classname="BuiltIn.GroupCompound" Name="Root"/>
  <Component Classname="BuiltIn.FuncUser" Name="Root.Evil" Filename="/etc/passwd"/>
</Project>"#,
            )
            .unwrap();
        let store = ProjectStore::new();
        store.discover_and_load(tmp.path()).unwrap();
        let cst = m1_core::parse("Evil();\n");
        store.with_project(|p| {
            assert!(
                goto(cst.root(), 0, p.unwrap(), Some("Caller.m1scr")).is_none(),
                "an absolute .m1prj Filename must not produce a goto Location"
            );
        });
    }
}
