//! Enum-literal hover rendering: an enum member (`Drive State.Off`), an enum
//! *type* head (`Gear State` in `Gear State.Driving`), and the group-shadowing
//! head disambiguation (`ASSI` in `ASSI.Driving`). The seam consumed by
//! [`super::hover_with_eval`] to describe enum constructs the resolver returns
//! Opaque for.
use m1_typecheck::project::Project;
use m1_typecheck::resolve::{Resolution, Scope, resolve};

/// Hover for an enum-member token, e.g. the trailing `Off` in `Drive State.Off`
/// — which resolves to neither a project symbol nor a built-in method, but which
/// the project model fully defines as `EnumType.Member` (name + integer value).
/// Renders `**{Enum}.{Member}** \`enum member\`\n\n= {value}`.
///
/// The segment under the cursor (`seg`, index `i` in `segs`) is the member name.
/// The enum is identified by the segment to its left when that is the enum's name
/// (`Drive State.Off`); a bare member (`Off`) is resolved via `enums_with_member`
/// when it is unambiguous (declared by exactly one enum). Returns `None` when the
/// segment is not an enum member.
pub(super) fn enum_member_markdown(
    seg: m1_core::Node,
    i: usize,
    segs: &[m1_core::Node],
    project: Option<&Project>,
) -> Option<String> {
    let table = project?.symbols();
    let member = seg.text();
    // Prefer the explicit `Enum.Member` form: the immediately-preceding segment
    // names the enum type.
    let id = if i > 0
        && let Some(id) = table.enum_by_name(segs[i - 1].text())
        && table.enum_has_member(id, member)
    {
        id
    } else {
        // Bare member: accept only when exactly one enum declares it (no ambiguity).
        match table.enums_with_member(member) {
            [only] => *only,
            _ => return None,
        }
    };
    let et = table.enum_type(id);
    let value = et
        .members
        .iter()
        .find(|(m, _)| m == member)
        .map(|(_, v)| v)?;
    Some(format!(
        "**{}.{member}** `enum member`\n\n= {value}",
        et.name
    ))
}

/// Hover for the *head* of an enum literal, e.g. the `Gear State` in
/// `Gear State.Driving` (or AV-M1's `ASSI.Driving`). The enum *type* name is not
/// a project symbol — it lives in the type table, not the channel table — so it
/// resolves Opaque and would otherwise fall back to "type not modelled". When the
/// segment names a known enum, describe the enum: its name and the valid members
/// (ContainerOrder, default marked), matching the channel-side enum rendering.
/// Returns `None` when the segment is not an enum name.
pub(super) fn enum_type_markdown(name: &str, project: Option<&Project>) -> Option<String> {
    let table = project?.symbols();
    let id = table.enum_by_name(name)?;
    let et = table.enum_type(id);
    let mut members: Vec<&(String, i64)> = et.members.iter().collect();
    members.sort_by_key(|(_, order)| *order);
    let list = members
        .iter()
        .map(|(name, _)| {
            if et.default.as_deref() == Some(name.as_str()) {
                format!("`{name}` (default)")
            } else {
                format!("`{name}`")
            }
        })
        .collect::<Vec<_>>()
        .join(", ");
    let mut s = format!("**{}** `enum type`", et.name);
    if !list.is_empty() {
        s.push_str(&format!("\n\nvalues: {list}"));
    }
    Some(s)
}

/// `ASSI` in `ASSI.Driving`: the *head* of an `EnumName.Member` literal. An enum
/// type routinely shares its name with a group/channel (the AV-M1 `ASSI` enum
/// lives alongside the `Root.Control.AV.ASSI` group), so plain per-segment
/// resolution returns that shadowing group (`type: Unknown`). The disambiguator
/// is the *next* segment: when it names a member of the enum this segment names,
/// the pair is unambiguously an enum literal, so describe the enum type rather
/// than the group. Returns `None` when there is no following member of a matching
/// enum (e.g. `ASSI.Status`, a real group-relative path).
pub(super) fn enum_literal_head_markdown(
    i: usize,
    segs: &[m1_core::Node],
    project: Option<&Project>,
    scope: &Scope,
) -> Option<String> {
    let table = project?.symbols();
    let id = table.enum_by_name(segs[i].text())?;
    let next = segs.get(i + 1)?;
    if table.enum_has_member(id, next.text()) {
        return enum_type_markdown(segs[i].text(), project);
    }
    // The next segment is not a member. Decide whether this is still an enum
    // literal with a *misspelled* member (`Color.Grren`) or a genuine path:
    let head_path = segs[..=i]
        .iter()
        .map(|n| n.text())
        .collect::<Vec<_>>()
        .join(".");
    // If the head resolves to a *value* symbol (a channel/parameter), the
    // construct is `<value>.<accessor>` (e.g. `Drive State.AsInteger`), not an
    // enum literal — defer to that symbol's hover.
    if let Resolution::Symbol(s) = resolve(&head_path, scope)
        && !matches!(
            s.kind,
            m1_typecheck::symbols::SymbolKind::Group | m1_typecheck::symbols::SymbolKind::Object
        )
    {
        return None;
    }
    // If `head.next` resolves to a real symbol, it is a genuine group-relative
    // path that merely shares the enum's name (`ASSI.Status`) — let the symbol
    // hover handle it. Otherwise the member is misspelled and the head is an
    // enum (optionally colliding with a group): show the enum's valid members and
    // flag the bad member — the most useful thing on a broken line (#163).
    let path = format!("{head_path}.{}", next.text());
    if matches!(resolve(&path, scope), Resolution::Symbol(_)) {
        return None;
    }
    enum_type_markdown(segs[i].text(), project)
        .map(|md| format!("{md}\n\n⚠ `{}` is not a member of this enum", next.text()))
}
