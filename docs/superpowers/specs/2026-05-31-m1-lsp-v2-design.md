# m1-lsp v2 — Design Specification

**Date:** 2026-05-31
**Status:** Approved for implementation
**Scope:** v2 — richer language features powered by `m1-typecheck`'s symbol
model: type diagnostics, hover, goto-definition, document symbols, completion,
and project (`.m1prj`) discovery/loading/reload, building on the shipped v1
server.

> **Note:** Example identifiers in this document are synthetic placeholders, not
> drawn from any real project. The example-corpus path is resolved via the
> `M1_CORPUS_PATH` env var (falling back to the sibling EV-M1 example project).

---

## 1. Purpose

v1 of `m1-lsp` shipped a `tower-lsp` server that publishes `m1-core` syntax +
`m1-lint` rule diagnostics and formats documents via `m1-fmt`, with Neovim
integration. It deliberately had **no symbol model**: no hover, no
goto-definition, no type diagnostics, no project awareness (see the v1 spec
§2.2 "Out-of-scope (v1) — YAGNI" and the v1 plan "Deferred to v2" table).

v2 adds the symbol-model-powered features. The new dependency is the
already-built **`m1-typecheck`** crate, which loads a MoTeC M1 project
(`Project.m1prj`, optionally augmented by a `.m1cfg`) into a `SymbolTable` of
channels/parameters/constants/functions/methods/tables/groups, resolves names
(locals → absolute → group-relative → opaque), and produces type diagnostics
(`T001`–`T011`). v2 wires that model into the editor:

- **Type diagnostics** — `m1-typecheck` becomes a third diagnostic source
  alongside `m1-core` syntax and `m1-lint`.
- **Hover** — show a symbol's kind / value type / unit (and a local's inferred
  Hungarian type) at the cursor.
- **Goto-definition** — for a project symbol, jump to the defining `.m1scr`
  file when the symbol is a `FuncUser`/`MethodUser` (the only symbols that carry
  a `Filename`); for other symbols, surface the project file.
- **Document symbols** — outline the locals and top-level assignments in the
  current file.
- **Completion** — project symbols (group-relative + absolute) and in-scope
  locals.
- **Project discovery / loading / reload** — find `Project.m1prj` from the
  server `root_dir`, load it once, cache it, and reload it when it (or a
  `.m1cfg`) changes on disk.

The toolchain position is unchanged from v1; `m1-typecheck` slots in as a peer
of `m1-lint`/`m1-fmt`:

```
tree-sitter-m1  (grammar)
      ↓
m1-core         (parse, CST, Node, byte-ranged positions)
      ↓
m1-lint  ──┐
m1-fmt   ──┤
m1-typecheck ──┤   (Project, SymbolTable, resolve, check_script)
               ↓
m1-lsp          (this crate — LSP server + editor glue)
```

`m1-lsp` continues to depend only on the **library crates** and must NOT import
`tree-sitter` directly; all CST access is via `m1-core`'s `Node`/`Cst`.

---

## 2. Scope

### 2.1 In-scope (v2)

Building directly on the existing `src/` modules (`backend.rs`, `analysis.rs`,
`convert.rs`, `line_index.rs`, `document.rs`, `lint_backend.rs`,
`fmt_backend.rs`) and the v1 traits (`analysis::LintProvider`,
`format::Formatter`, `backend::Backend::with_backends`):

- **Project discovery & loading** (`src/project_store.rs`): from the
  `InitializeParams.root_uri` (or `root_dir`), find `Project.m1prj` upward,
  `m1_typecheck::Project::load(path)`, optionally `.with_config(m1cfg)`, and
  cache the loaded `Project` on the backend behind an `RwLock<Option<...>>`.
  Reload when `didChangeWatchedFiles` reports a change to the `.m1prj`/`.m1cfg`
  (registered dynamically), and re-publish diagnostics for all open docs.
- **Type diagnostics** (`src/type_backend.rs` + a new `TypeProvider` trait in
  `analysis.rs`): a third diagnostic source. When a project is loaded,
  `m1_typecheck::rules::check_script(&project, script_path, src)`; with no
  project, `check_script_no_project(src)` (T001 disabled, the other rules still
  fire). Map each `TypeDiagnostic` to an LSP diagnostic with `source =
  "m1-typecheck"` and `code = TypeCode::as_str()`.
- **L006 ↔ T002 de-duplication**: when a project is loaded, suppress
  `m1-lint`'s `L006` (the float-equality *heuristic*) in favour of
  `m1-typecheck`'s `T002` (the type-aware float-equality rule). With no project
  loaded, keep `L006`. (The v1 typecheck design notes T002 supersedes the L006
  heuristic.)
- **Hover** (`textDocument/hover`, `src/features/hover.rs`): find the
  identifier/member-expression node at the cursor, take its dotted path text,
  `m1_typecheck::resolve::resolve(path, &scope)`, and render a markdown hover
  describing the resolution (symbol kind + value type + unit, or a local's
  inferred type, or "opaque/built-in").
- **Goto-definition** (`textDocument/definition`,
  `src/features/goto.rs`): resolve the path; if it is a `Symbol` whose
  `filename` is `Some`, return a `Location` pointing at that `.m1scr` file
  (resolved relative to the project root) at `0:0`; otherwise return `None`.
- **Document symbols** (`textDocument/documentSymbol`,
  `src/features/document_symbols.rs`): walk the CST for `LocalDeclaration` and
  top-level `AssignmentStatement` targets, emitting a flat `DocumentSymbol`
  list with correct ranges.
- **Completion** (`textDocument/completion`,
  `src/features/completion.rs`): offer in-scope locals plus project symbols
  (both their full `path` and the group-relative tail for the current file's
  group). No fuzzy ranking beyond what the client does.
- **Capability advertisement** extended to: `hover_provider`,
  `definition_provider`, `document_symbol_provider`, `completion_provider`
  (with no trigger characters in v2 — invoked completion only).
- **Neovim integration updates** (`editors/nvim/m1.lua`, `README.md`):
  document the new keymaps (`K` hover, `gd` goto, `<C-x><C-o>`/omnifunc
  completion, outline), the `.m1cfg` watch, and that `root_dir` must be the
  directory containing `Project.m1prj` for the project model to load.

### 2.2 Out-of-scope (v2) — YAGNI / deferred to v3

| Feature | Why deferred |
|---------|--------------|
| Code actions / quick fixes that **apply lint fixes** | Needs m1-lint v2 `--fix`/edit support, which does not exist yet (no `fix` field on `LintDiagnostic`, no fix module). v3. |
| Semantic tokens | `nvim-treesitter` already highlights via the grammar; the symbol model adds little over that. v3. |
| Find references / rename | `m1-typecheck` has no reverse index (symbol → use-sites) and symbols carry no source spans. Needs a project-wide index. v3. |
| Signature help | Needs a function-signature/parameter model `m1-typecheck` does not expose. v3. |
| Incremental (range) sync + CST/symbol caching | Files are small and re-parse is sub-ms; v1's full-sync is still correct and simple. Only if profiling demands. v3. |
| Goto-definition to a **precise location inside `.m1prj`** | `m1_typecheck::Symbol` stores `path`/`kind`/`value_type`/`unit`/`filename` but **no byte offset** within the `.m1prj`. v2 jumps to the defining `.m1scr` (functions/methods) only. v3 could add `.m1prj` offsets if `m1-typecheck` exposes them. |
| `.m1lint.toml` / per-project lint config | m1-lint's own v2 concern. |
| Workspace symbol (`workspace/symbol`) | Single-project, single-file editing is the v2 target; defer the workspace-wide query. v3. |

---

## 3. Key Decisions

### 3.1 `m1-typecheck` as a third diagnostic source — a new `TypeProvider` trait

**Decision:** Mirror the v1 `LintProvider`/`Formatter` pattern with a third
trait in `analysis.rs`:

```rust
pub trait TypeProvider: Send + Sync {
    /// Type diagnostics for `src`. `uri` lets the provider derive the script
    /// file name (for group-relative resolution) and consult the loaded project.
    fn types(&self, uri: &Url, src: &str, li: &LineIndex, enc: PositionEncoding) -> Vec<LspDiag>;
}
```

`analyze()` gains a `types: &dyn TypeProvider` parameter and concatenates its
output after the syntax and lint diagnostics. A `NoTypes` no-op (mirroring
`NoLint`) keeps the function and the server unit-testable without
`m1-typecheck`, and keeps the pure-function character of `analyze` (no I/O, no
`Client`).

**Justification:** This is the smallest change that fits the existing
architecture — the v1 plan already anticipated "wire `m1-typecheck` as a third
`DiagnosticSource`." The real provider (`type_backend::M1Type`) holds a shared
handle to the project cache (`Arc<ProjectStore>`) so it sees reloads.

`analyze`'s signature changes from
`analyze(src, li, enc, lint)` to `analyze(uri, src, li, enc, lint, types)`. The
`uri` is needed so the type provider can compute the script's `file_name` for
`check_script`. All v1 call-sites (`backend::Backend::publish`, the tests) are
updated accordingly.

### 3.2 Project discovery, caching, and reload — `ProjectStore`

**Decision:** A new `src/project_store.rs` owns project lifecycle:

```rust
pub struct ProjectStore {
    /// The loaded project + the paths it was loaded from (for reload + goto).
    inner: RwLock<Option<LoadedProject>>,
}

pub struct LoadedProject {
    pub project: m1_typecheck::Project,
    pub root: PathBuf,        // dir containing Project.m1prj
    pub m1prj_path: PathBuf,
    pub m1cfg_path: Option<PathBuf>,
}
```

- **Discovery:** on `initialize`, take `root_uri` (fall back to the first
  `workspace_folders` entry). Search that directory and its ancestors for
  `Project.m1prj`; if found, also look for a sibling `*.m1cfg`. This mirrors the
  v1 Neovim `root_dir` rule and `m1-typecheck`'s own CLI (`find_project`).
- **Load:** `Project::load(&m1prj_path)`, then `.with_config(&m1cfg)` if a
  `.m1cfg` was found. On `LoadError`, log via `client.log_message` and leave the
  store empty (project-less mode — the server still works, T001 just stays off).
- **Cache:** the `LoadedProject` lives behind an `RwLock<Option<...>>` on the
  backend, shared with the `TypeProvider` and the hover/goto/completion features
  via an `Arc`.
- **Reload:** register a `didChangeWatchedFiles` watcher for `**/*.m1prj` and
  `**/*.m1cfg` (dynamic registration in `initialized`, since v1 advertises a
  static capability set). On a watched-file change, re-run discovery/load and
  then re-`publish` diagnostics for every open document so T001 results
  refresh.

**Justification:** Keeping project state out of `Backend`'s per-document
`DashMap` and behind one `RwLock` keeps the document store and the project model
orthogonal — exactly as v1 kept lint/fmt behind trait objects. The `Arc` share
lets the pure-ish feature handlers read the project without reaching back into
`Backend`.

### 3.3 Cursor → node → path lookup (the new piece of real complexity)

Hover/goto/completion all need: given an LSP `Position`, find the
identifier-or-member node under it and its dotted path. `m1-core`'s `Node` has
`children()`/`named_children()`/`byte_range()`/`kind()`/`text()` but **no
"node at offset"** helper, so `m1-lsp` owns a small descend-to-offset walk in
`src/features/locate.rs`:

```rust
/// The deepest node whose byte range contains `byte`.
pub fn node_at_byte(root: Node, byte: usize) -> Option<Node>;

/// The enclosing identifier/member-expression and its dotted path text, e.g.
/// hovering inside `Engine Speed` within `Engine Speed.Value` yields the whole
/// `Engine Speed.Value` path (so resolution matches m1-typecheck).
pub fn path_at_byte(root: Node, byte: usize) -> Option<(Node, String)>;
```

`path_at_byte` walks up from `node_at_byte` while the parent is a
`Kind::MemberExpression`, then returns that member node's `text()` (which is the
full dotted path as it appears in source, including space-containing segments —
`m1-core` already treats `Vund Klee.Trilby Glonk` as one member expression).
This reuses the same path string `m1-typecheck`'s rules feed to `resolve`, so
hover/goto resolution is consistent with the published T001 diagnostics.

The inbound `Position` → byte conversion uses the **existing**
`LineIndex::offset(pos, text, enc)` from v1 (already encoding-aware). The
outbound node-range → LSP range uses the existing `convert::range(byte_range,
li, enc)`. No new position math is introduced.

### 3.4 Position encoding for the new features

All new features are pure consumers of the v1 `LineIndex` + `convert` layer:
inbound positions go through `LineIndex::offset` (honouring the negotiated
`PositionEncoding` stored on `Backend`), outbound ranges through
`convert::range`. `.m1scr` is ASCII in practice, but the encoding-aware code
path is exercised unchanged. Hover ranges, goto target ranges, document-symbol
ranges and completion edit ranges (none needed in v2 — completion items carry
plain `insert_text`) all flow through this one path.

### 3.5 L006 / T002 de-duplication

**Decision:** The de-dup lives in `analyze()`, not in either provider, because
only `analyze` sees both sources and knows whether a project is loaded:

- The `TypeProvider` reports whether the diagnostics it produced were
  project-backed (it knows — `M1Type` checks the `ProjectStore`).
- When project-backed, `analyze` filters out any `m1-lint` diagnostic whose
  `code == "L006"` before concatenating. T002 (from the type source) takes its
  place.
- With no project, nothing is filtered; L006 remains the only float-equality
  signal.

To make this testable without the real providers, `TypeProvider` exposes
`fn project_loaded(&self) -> bool`, and `analyze` branches on it. (Both v1 no-op
providers return `false`.)

**Justification:** Putting the policy in the orchestrator keeps both providers
ignorant of each other and keeps `convert`/the backends thin — consistent with
v1's "providers don't know about each other" design.

### 3.6 Goto-definition target model

**Decision:** `m1_typecheck::Symbol` carries `filename: Option<String>` — set
only for `FuncUser`/`MethodUser` components (the `.m1scr`-backed symbols). For
goto:

- Resolve the path at the cursor via `resolve`.
- If `Resolution::Symbol(sym)` and `sym.filename.is_some()`, build a target
  `Location` = `root.join(filename)` as a `file://` `Url`, range `0:0..0:0`
  (whole-file; no in-file offset is available because functions live in their
  own file).
- Otherwise (`Local`, `Opaque`, `Unresolved`, or a symbol with no `filename`)
  return `None`.

**Justification:** This is the honest extent of what the symbol model supports
today: channels/parameters/constants/tables/groups are defined declaratively in
`.m1prj` with no byte offset, so they have no jump target. Functions/methods do
have a backing `.m1scr`. v3 can add `.m1prj` offsets if `m1-typecheck` grows
them (noted in §2.2).

### 3.7 Buildability / sequencing

`m1-typecheck` is **already built** (its `Project`, `SymbolTable`, `resolve`,
`check_script`, `check_script_no_project`, `TypeDiagnostic`, `TypeCode`,
`ValueType`, `SymbolKind` are the v2 integration surface). So unlike v1's
lint/fmt sequencing, the v2 type features are *not* blocked on a sibling crate.
The one genuinely-blocked item — lint-fix code actions — depends on m1-lint v2
`--fix`, which does **not** exist, and is therefore deferred to v3 (§2.2), not
sequenced into v2.

The new `TypeProvider`/`ProjectStore`/feature modules still follow the v1
pattern of trait-isolation so the pure parts are unit-testable without a live
server and without a real `.m1prj` on disk (tests construct a tiny in-memory
project fixture, or use the `NoTypes`/`project_loaded()==false` path).

---

## 4. Architecture

### 4.1 Crate layout (additions to v1)

```
m1-lsp/
  Cargo.toml          (+ m1-typecheck = { path = "../m1-typecheck" })
  src/
    main.rs           (inject M1Type + ProjectStore via with_backends_v2)
    backend.rs        (extend: project store, watched-file reload, new handlers)
    analysis.rs       (+ TypeProvider trait, NoTypes, analyze() gains uri+types, L006/T002 de-dup)
    convert.rs        (+ type_diagnostic() mapping)
    type_backend.rs   (NEW: M1Type — real TypeProvider over ProjectStore)
    project_store.rs  (NEW: discovery, load, cache, reload of Project)
    features/
      mod.rs          (NEW)
      locate.rs       (NEW: node_at_byte, path_at_byte)
      hover.rs        (NEW)
      goto.rs         (NEW)
      document_symbols.rs (NEW)
      completion.rs   (NEW)
    (unchanged: line_index.rs, document.rs, format.rs, lint_backend.rs, fmt_backend.rs)
  editors/nvim/
    m1.lua            (extend: keymaps, .m1cfg watch note)
    README.md         (extend: new capabilities)
  tests/
    integration.rs    (extend: capabilities now include hover/definition/...)
    features.rs       (NEW: hover/goto/document-symbol/completion direct-call tests)
    project.rs        (NEW: discovery + type-diagnostic + L006/T002 de-dup tests)
    corpus.rs         (extend: also run the type provider over the corpus)
```

### 4.2 Module responsibilities (new/changed)

**`project_store.rs`** — `ProjectStore { inner: RwLock<Option<LoadedProject>> }`
with `discover_and_load(root: &Path, client_log)`, `loaded() ->
RwLockReadGuard`, `project_loaded() -> bool`, and `script_path_in_project(uri)`
helpers. Owns all `.m1prj`/`.m1cfg` filesystem I/O — the one place in the crate
that touches disk for the project model.

**`type_backend.rs`** — `M1Type { store: Arc<ProjectStore> }` implementing
`analysis::TypeProvider`. `types()` derives the file name from `uri`, calls
`check_script` (project) or `check_script_no_project` (no project), and maps via
`convert::type_diagnostic`. `project_loaded()` delegates to the store.

**`analysis.rs`** — adds `TypeProvider`/`NoTypes`; `analyze(uri, src, li, enc,
lint, types)` now: parse → syntax diags → lint diags → type diags, then drops
`L006` lint diags iff `types.project_loaded()`.

**`convert.rs`** — adds `type_diagnostic(d: &TypeDiagnostic, li, enc) ->
LspDiag` (source `"m1-typecheck"`, code `d.code.as_str()`), reusing the existing
`range`/`severity` helpers.

**`features/locate.rs`** — pure CST cursor utilities (§3.3), unit-tested against
`m1_core::parse` directly.

**`features/{hover,goto,document_symbols,completion}.rs`** — each a pure
function taking the parsed/located inputs plus the loaded project, returning the
LSP payload (`Option<Hover>`, `Option<GotoDefinitionResponse>`,
`Vec<DocumentSymbol>`, `Vec<CompletionItem>`). `backend.rs` is the only place
that touches `self.client`/`self.docs`/the store and calls these.

**`backend.rs`** — extends `Backend` with `store: Arc<ProjectStore>` and a
`types: Box<dyn TypeProvider>`; adds `with_backends_v2(client, lint, formatter,
types, store)`; runs discovery in `initialize`; dynamically registers the
watched-file capability in `initialized`; implements `hover`, `goto_definition`,
`document_symbol`, `completion`, and `did_change_watched_files`. Existing
handlers (`did_open`/`did_change`/`did_save`/`did_close`/`formatting`/
`shutdown`) are unchanged except `publish` now passes `uri` + the type provider
to `analyze`.

### 4.3 Data flow (hover, representative of the read features)

```
        Neovim (LSP client)
            │  textDocument/hover {uri, position}
            ▼
 ┌──────────────────────────────────────────────────────────┐
 │ backend.rs: hover()                                        │
 │   doc = docs.get(uri)                                      │
 │   byte = doc.line_index.offset(position, &doc.text, enc)   │
 │   cst  = m1_core::parse(&doc.text)                         │
 │   (node, path) = locate::path_at_byte(cst.root(), byte)?   │
 │   guard = store.loaded()                                   │
 │   scope = build_scope(&doc.text, uri, project)             │
 │   res  = m1_typecheck::resolve::resolve(&path, &scope)     │
 │   features::hover::render(res, node, li, enc) -> Hover     │
 └──────────────────────────────────────────────────────────┘
```

`build_scope` mirrors `m1-typecheck`'s own `run()`: collect locals from the CST
(same Hungarian inference), set `group = project.group_for_script(file_name)`,
and `project = Some(&loaded.project)`. To avoid duplicating
`collect_locals`/scope assembly, v2 prefers a thin local re-implementation in
`features/locate.rs` (the logic is ~15 lines and `m1-typecheck` does not export
`collect_locals`); if `m1-typecheck` later exports a `Scope` builder, switch to
it.

---

## 5. Neovim integration

`editors/nvim/m1.lua` and `README.md` are extended (not rewritten):

1. **`root_dir` matters now.** The server only loads the project model when its
   `root_dir` is (an ancestor of) the directory containing `Project.m1prj`. The
   v1 `root_dir` rule (nearest `Project.m1prj`, else `.git`, else file dir)
   already does this; the README now calls out *why* it matters (T001/hover/goto
   need it).
2. **New keymaps** (documented, applied via an `LspAttach` autocmd filtered to
   the `m1-lsp` client):
   - `K` → `vim.lsp.buf.hover()`
   - `gd` → `vim.lsp.buf.definition()`
   - `gO` → `vim.lsp.buf.document_symbol()`
   - completion via `vim.lsp.completion.enable()` (built-in, Nvim 0.11+) or
     `omnifunc`/`<C-x><C-o>`.
3. **`.m1cfg` awareness.** Note that editing `Project.m1prj` or a `.m1cfg`
   triggers a server-side reload via watched files; no editor action needed.
4. The `nvim-treesitter` highlight/indent/fold layer is unchanged from v1.

The Lua stays copy-paste ready and plugin-manager-agnostic.

---

## 6. Configuration

v2 keeps v1's initialization-options model and adds two optional keys:

```jsonc
{
  "format_on_save": false,  // (v1) advisory hint for the editor snippet
  "lint": true,             // (v1) toggle lint diagnostics
  "typecheck": true,        // (v2) toggle type diagnostics
  "project_file": null      // (v2) explicit Project.m1prj path; overrides discovery
}
```

Unknown options are ignored. `project_file`, when set, skips upward discovery
and loads exactly that path (useful for non-standard layouts / tests). When
`typecheck` is `false`, the server installs `NoTypes` and never loads a project.

---

## 7. Error handling

| Condition | Behaviour |
|-----------|-----------|
| No `Project.m1prj` found | Project-less mode: `check_script_no_project` (T001 off, T002/T003/T010/T011 on); hover/goto resolve locals + opaque only |
| `Project::load` / `.with_config` returns `LoadError` | Log via `client.log_message`; leave store empty (project-less mode); never error the client |
| `.m1prj` changes on disk | Reload; on failure keep the previously-loaded project and log |
| Hover/goto/completion on unknown URI or no node at cursor | Return `Ok(None)` / empty list |
| Type check on syntactically-broken source | `m1-typecheck` returns only `syntax_errors` (no type diags); `m1-lsp` ignores those (syntax already reported by `m1-core`) and publishes no type diagnostics |
| Goto a symbol with no `filename` | Return `Ok(None)` |
| Panic inside a handler | `tower-lsp` isolates the request; log via `client.log_message` |

The server still never writes files and never mutates documents.

---

## 8. Testing strategy

- **`features/locate.rs` unit tests** — `node_at_byte`/`path_at_byte` on:
  a bare identifier, a member expression (`A.B.C`), a space-containing
  member (`Engine Speed.Value`), a cursor in whitespace (→ `None`), and the
  empty document.
- **`convert::type_diagnostic` unit test** — a `TypeDiagnostic` maps to source
  `"m1-typecheck"`, code `"T002"`, correct severity/range.
- **`analysis::analyze` tests** — (a) type provider contributes diagnostics;
  (b) with `project_loaded()==true` a fake L006 lint diagnostic is dropped while
  a non-L006 lint diagnostic survives; (c) with `project_loaded()==false` L006
  survives.
- **`project_store.rs` tests** — write a minimal `Project.m1prj` to a `tempdir`,
  discover + load, assert a known symbol resolves; assert a missing file yields
  project-less mode without panicking.
- **Feature tests (`tests/features.rs`)** — build a `Backend` (or call the pure
  feature fns) against an in-memory project fixture: hover over a known channel
  shows its kind/type; goto a `FuncUser` returns its `.m1scr` `Location`;
  document symbols list the locals; completion includes a known symbol and a
  local.
- **Integration test (`tests/integration.rs`)** — extend the capabilities
  assertion: `hoverProvider`, `definitionProvider`, `documentSymbolProvider`,
  `completionProvider` are advertised.
- **Corpus smoke (`tests/corpus.rs`)** — for each script under
  `$M1_CORPUS_PATH`, the type provider (project-less) and `locate::path_at_byte`
  at every identifier start must not panic. If a sibling `Project.m1prj` exists,
  also load it and run `check_script`.

---

## 9. Resolved decisions / assumptions

1. **`m1-typecheck` is built** — it is the v2 integration target, not a blocker.
2. **Third diagnostic source** via a new `TypeProvider` trait mirroring v1's
   `LintProvider`; `analyze` orchestrates and owns the L006/T002 de-dup.
3. **Project lifecycle** lives in `ProjectStore` behind one `RwLock`, shared via
   `Arc`; reload on `didChangeWatchedFiles` for `.m1prj`/`.m1cfg`.
4. **Goto** targets the defining `.m1scr` for functions/methods only; other
   symbols have no source span in the model (v3 if `m1-typecheck` adds offsets).
5. **Cursor→path** is a small `m1-lsp`-owned CST walk reusing the v1
   `LineIndex`/`convert` position layer; no new position math.
6. **Lint-fix code actions are NOT in v2** — they require m1-lint v2 `--fix`,
   which is unimplemented; deferred to v3.

---

## 10. Non-goals

- `m1-lsp` does not implement type inference or name resolution itself
  (delegates entirely to `m1-typecheck`), nor parse `.m1prj`/`.m1cfg` itself
  (delegates to `m1_typecheck::Project::load`/`.with_config`).
- `m1-lsp` does not build a cross-file reference index, rename engine, or
  workspace symbol index in v2.
- `m1-lsp` does not apply lint fixes (no m1-lint `--fix` yet) and ships no
  semantic-token provider (nvim-treesitter covers highlighting).
