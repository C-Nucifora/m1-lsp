# m1-lsp

A [Language Server Protocol](https://microsoft.github.io/language-server-protocol/)
server for the MoTeC M1 script language (`.m1scr`). It is the integration layer
at the top of the M1 toolchain: it parses files through `m1-core`, surfaces
syntax and lint diagnostics live, and formats documents through `m1-fmt`.

The primary target editor is **Neovim**, but the server speaks plain LSP over
stdio and works with any LSP client (VS Code, Helix, Emacs `eglot`, etc.).

## Workspace layout

`m1-lsp` sits at the **top** of the M1 toolchain, which lives in **six separate
repositories**. They are not published to crates.io; instead each crate pins its
upstreams as **versioned git-tag Cargo dependencies**, so this crate **does**
build from a standalone clone — Cargo fetches every upstream from its tagged
release. Checking the whole set out as siblings under one parent directory is
handy for cross-repo work, but is not required to build:

```
<parent>/
├── tree-sitter-m1/   # grammar (root)
├── m1-core/          # parse / CST / diagnostics
├── m1-lint/          # linter
├── m1-fmt/           # formatter
├── m1-typecheck/     # type checker
└── m1-lsp/           # this crate
```

**`m1-lsp` depends on all four upstream crates** —
`m1-core`, `m1-lint`, `m1-fmt`, and `m1-typecheck` (each a git-tag dep), plus
`tree-sitter-m1` and `m1-workspace` — so a clean build of `m1-lsp` against the
pinned upstream tags is the toolchain's end-to-end integration check.

Because every dependency is pinned by tag, the coupling **is** visible on
GitHub — each `Cargo.toml` names its upstreams and their versions, and Dependabot
opens bump PRs as new upstream tags ship. Cutting a new upstream release and
bumping `tag = "vX.Y.Z"` in each consumer is what propagates a change across the
stack. The `m1-example` example project (used by the corpus smoke test) is an
optional sibling checkout.

## Features

- **Diagnostics** (`textDocument/publishDiagnostics`): the union of
  - `m1-core` syntax diagnostics (source `m1-core`),
  - `m1-lint` rule findings `L001`–`L009` (source `m1-lint`),
  - `m1-typecheck` type diagnostics `T0xx` (source `m1-typecheck`, requires a
    loaded project), with deprecated-overload findings tagged
    `DiagnosticTag::Deprecated`, and
  - an `unsupported-c-token` check (source `m1-intrinsics`) that flags C
    operators M1 rejects (`==`/`!=`/`&&`/`||`/`!`/`while`/`for`/`do`).

  Recomputed on open / change / save, and re-published when the project model
  reloads.
- **Quick-fixes** (`textDocument/codeAction`): for the fixable
  `unsupported-c-token` operators, a quick-fix replaces them with the M1
  keyword (`==`→`eq`, `!=`→`neq`, `&&`→`and`, `||`→`or`, `!`→`not`).
- **Formatting** (`textDocument/formatting`): whole-document reformat via
  `m1-fmt`, returned as a single full-document `TextEdit`. No edits are returned
  when the document is already formatted or has syntax errors (pass-through
  safety).
- **Hover** (`textDocument/hover`): type / kind of the symbol under the cursor —
  locals (inferred type), project channels/parameters/constants (with units and
  named enum types), and library functions (signatures, `stateful` /
  `deprecated` flags).
- **Go-to-definition** (`textDocument/definition`): jumps to the backing
  `.m1scr` / `.m1dbc` file of a project function or DBC signal. (The target is
  opened at its start; the symbol model does not track a finer position.)
- **References & document highlights** (`textDocument/references`,
  `textDocument/documentHighlight`): all same-file occurrences of the local /
  channel / symbol under the cursor, with read/write classification for
  highlights.
- **Document symbols** (`textDocument/documentSymbol`): outline of locals and
  assignment targets.
- **Completion** (`textDocument/completion`): in-scope locals, project symbols,
  library objects and keywords; after a library object `.` (a trigger
  character), that object's methods. Project symbols carry their value type and
  unit in the completion `detail` (e.g. `Unsigned · ratio`, `Enum (Drive State)`).
- **Signature help** (`textDocument/signatureHelp`): library-function overloads
  with the active argument highlighted, triggered on `(` and `,`.
- **Inlay hints** (`textDocument/inlayHint`): an inline `: Type` after each
  un-annotated `local`.
- **Rename** (`textDocument/rename` + `prepareRename`): file-scoped rename of a
  `local` (project symbols are declared in `.m1prj` and are not renamed here).
- **Folding** (`textDocument/foldingRange`): `{ … }` blocks and multi-line block
  comments.
- **Code lens** (`textDocument/codeLens`): a `⚡ N Hz` lens at the top of each
  `.m1scr` naming the script's execution rate, derived from its `.m1prj`
  `SelectedTrigger` clock (#86, view half). Shown only when the rate is
  statically known; startup-only and `$(…)`-templated triggers carry no lens.
  Changing the rate writes to `Project.m1prj` and lives in the editor/CLI layer,
  not the server.
- **Semantic tokens** (`textDocument/semanticTokens/full`): full-document token
  classification (variables, functions, keywords, numbers, strings, comments,
  types, parameters, namespaces; `definition` / `readonly` modifiers).
- **Call hierarchy** (`textDocument/prepareCallHierarchy` +
  `callHierarchy/incomingCalls` / `outgoingCalls`): the cross-script channel
  *data-flow* graph (#84). An item is a channel or a `.m1scr` script. From a
  channel, **incoming** lists the scripts that read it and **outgoing** the
  scripts that write (produce) it; from a script, **outgoing** lists the channels
  it writes and **incoming** the scripts that read a channel it writes. Channel
  references are resolved through each script's group scope, so the same channel
  written/read under different group-relative spellings collapses onto one node.
  Script items show their call rate (e.g. `Engine.Control @ 100 Hz`).
- **Position encoding**: byte offsets from `m1-core` are converted to LSP
  positions in UTF-16 code units by default, or UTF-8 when the client negotiates
  it.
- **Document lifecycle**: full-document sync (`didOpen` / `didChange` /
  `didSave` / `didClose`), plus `workspace/didChangeWatchedFiles` for
  `.m1prj` / `.m1cfg` reloads.

The project model auto-discovers a `parameters.m1cfg` in the project directory
or an ancestor (nearest wins) and loads it via `m1-typecheck`; this is what gives parameters
their concrete **value types and units** (the `.m1prj` mostly just names them).
Those types flow into hover, completion detail, inlay hints, and assignment
type-checking, and the server reloads them when the `.m1cfg` changes.

## Build

```bash
cargo build --release      # binary at target/release/m1-lsp
# or:
cargo install --path .     # installs `m1-lsp` onto your $PATH
```

## Editor setup

See [`editors/nvim/README.md`](editors/nvim/README.md) for Neovim setup,
covering both the `m1-lsp` server (diagnostics + formatting) and
`tree-sitter-m1` highlighting / indent / fold via `nvim-treesitter`.

## Architecture

- `src/line_index.rs` — encoding-aware byte ↔ LSP position conversion.
- `src/convert.rs` — `m1-core` / `m1-lint` diagnostics → `lsp-types`.
- `src/analysis.rs` — `analyze()`: unions syntax diagnostics with a
  `LintProvider`.
- `src/format.rs` — `Formatter` trait + full-range `TextEdit` construction.
- `src/lint_backend.rs` / `src/fmt_backend.rs` — the real `m1-lint` / `m1-fmt`
  backends.
- `src/backend.rs` — the `tower-lsp` `LanguageServer` implementation.

`m1-lsp` never imports `tree-sitter` directly; all CST access is via `m1-core`.

## Testing

```bash
cargo test                 # unit + integration + corpus smoke test
M1_CORPUS_PATH=/path cargo test --test corpus   # point the smoke test elsewhere
```

The corpus smoke test runs `analyze()` over every `.m1scr` in
`$M1_CORPUS_PATH` (falling back to the sibling m1-example example project) and
asserts it never panics. It is skipped if the directory is absent.

## Releases

`.github/workflows/release.yml` publishes prebuilt server binaries to a GitHub
Release (`v<crate-version>`) for **Linux x64**, **Windows x64**, and
**Apple-Silicon macOS (arm64)**. These are consumed by the `m1-vscode` extension.

### Intel macOS

Intel macOS (`x86_64-apple-darwin`) binaries are **not** published — GitHub no
longer reliably provides Intel-Mac CI runners. On an Intel Mac, build the server
yourself:

```bash
git clone https://github.com/C-Nucifora/m1-lsp && cd m1-lsp
cargo build --release            # -> target/release/m1-lsp
```

Then point the editor at it (in VS Code: set `m1.server.path` to that binary, or
put `m1-lsp` on your `PATH`). See the `m1-vscode` README for the full Intel-Mac
setup.

## Status

v2. The symbol-model features (hover, completion, go-to-definition, signature
help, inlay hints, semantic tokens, and type diagnostics) are implemented on top
of the `m1-typecheck` project model, alongside references, document highlights,
folding, rename, and code-action quick-fixes.

## License

Licensed under the GNU General Public License v3.0 or later (GPL-3.0-or-later) — see [LICENSE](LICENSE).

Copyright (C) 2026 The M1 Tools authors.

## Trademark

Independent, community-built open-source tooling for the MoTeC® M1 script
language. Not affiliated with, authorised, or endorsed by MoTeC Pty Ltd.
"MoTeC" and "M1" are trademarks of MoTeC Pty Ltd.
