//! Name resolution: locals, absolute, group-relative, opaque-root passthrough.
use crate::project::Project;
use crate::types::ValueType;
use std::collections::HashMap;

pub struct Scope<'p> {
    pub locals: HashMap<String, ValueType>,
    pub group: Option<String>,
    pub project: Option<&'p Project>,
}

#[derive(Debug)]
pub enum Resolution<'p> {
    Local(ValueType),
    Symbol(&'p crate::symbols::Symbol),
    Opaque,     // resolves to a built-in/unknown root; type Unknown, never flagged
    Unresolved, // project-rooted path with no matching symbol
}

fn root_segment(path: &str) -> &str {
    match path.find('.') {
        Some(i) => &path[..i],
        None => path,
    }
}

pub fn resolve<'p>(path: &str, scope: &Scope<'p>) -> Resolution<'p> {
    // 1. Local (single segment).
    if !path.contains('.') {
        if let Some(&t) = scope.locals.get(path) {
            return Resolution::Local(t);
        }
    }
    let Some(project) = scope.project else {
        // project-less mode: locals only, everything else opaque
        return Resolution::Opaque;
    };
    let table = project.symbols();

    // 2. Absolute (as-is, then Root.-prefixed).
    if let Some(sym) = table.get(path) {
        return Resolution::Symbol(sym);
    }
    let rooted = format!("Root.{path}");
    if let Some(sym) = table.get(&rooted) {
        return Resolution::Symbol(sym);
    }

    // 3. Group-relative: walk the enclosing group up to Root.
    if let Some(group) = &scope.group {
        let mut prefix = Some(group.clone());
        while let Some(g) = prefix {
            let candidate = format!("{g}.{path}");
            if let Some(sym) = table.get(&candidate) {
                return Resolution::Symbol(sym);
            }
            prefix = g.rfind('.').map(|i| g[..i].to_string());
        }
    }

    // 4. Accessor/method on a resolved symbol: if any *proper prefix* of the
    //    path resolves to a symbol, the trailing segments are a built-in
    //    accessor/method (e.g. `Channel.AsInteger`, `Param.Set`) — opaque, not a
    //    miss. This is the key guard against flagging accessor calls on existing
    //    channels/parameters (the symbol table stores stored symbols only, not
    //    the built-in operations they expose).
    if prefix_resolves(path, scope, table) {
        return Resolution::Opaque;
    }

    // 5. Opaque vs unresolved: only flag when the root is a known PROJECT group.
    let root = root_segment(path);
    let root_is_project_group = table
        .get(&format!("Root.{root}"))
        .map(|s| matches!(s.kind, crate::symbols::SymbolKind::Group))
        .unwrap_or(false)
        || root == "Root";
    if root_is_project_group {
        Resolution::Unresolved
    } else {
        Resolution::Opaque
    }
}

/// True if some proper prefix of `path` (dropping one or more trailing segments)
/// resolves to a symbol via absolute, `Root.`-prefixed, or group-relative lookup.
fn prefix_resolves(path: &str, scope: &Scope, table: &crate::symbols::SymbolTable) -> bool {
    let mut current = path;
    // Strip trailing segments one at a time, testing each shorter prefix.
    while let Some(i) = current.rfind('.') {
        current = &current[..i];
        if symbol_exists(current, scope, table) {
            return true;
        }
    }
    false
}

/// Does `path` resolve to a symbol that can expose accessors/members? That is
/// any non-group symbol (channel/parameter/constant/…), OR a value-bearing
/// compound group (one with a `.Value` child — the M1 enum/channel-compound
/// idiom). Pure namespace groups are excluded: a path under one with an absent
/// leaf is a genuine miss, whereas a path under a value compound is an accessor.
fn symbol_exists(path: &str, scope: &Scope, table: &crate::symbols::SymbolTable) -> bool {
    let is_accessor_base = |full: &str, s: &crate::symbols::Symbol| {
        !matches!(s.kind, crate::symbols::SymbolKind::Group) || table.has_child(full, "Value")
    };
    for full in [path.to_string(), format!("Root.{path}")] {
        if table
            .get(&full)
            .map(|s| is_accessor_base(&full, s))
            .unwrap_or(false)
        {
            return true;
        }
    }
    if let Some(group) = &scope.group {
        let mut prefix = Some(group.clone());
        while let Some(g) = prefix {
            let full = format!("{g}.{path}");
            if table
                .get(&full)
                .map(|s| is_accessor_base(&full, s))
                .unwrap_or(false)
            {
                return true;
            }
            prefix = g.rfind('.').map(|i| g[..i].to_string());
        }
    }
    false
}
