# m1-lsp

A [Language Server Protocol](https://microsoft.github.io/language-server-protocol/)
server for the MoTeC M1 script language (`.m1scr`). It is the integration
layer at the top of the M1 toolchain: parsing through
[m1-core](https://github.com/C-Nucifora/m1-core), live lint and type
diagnostics through [m1-lint](https://github.com/C-Nucifora/m1-lint) and
[m1-typecheck](https://github.com/C-Nucifora/m1-typecheck), and formatting
through [m1-fmt](https://github.com/C-Nucifora/m1-fmt).

It speaks plain LSP over stdio and works with any LSP client. Ready-made
integrations exist for VS Code
([m1-vscode](https://github.com/nedlane/m1-vscode)) and Neovim
([nvim-m1](https://github.com/C-Nucifora/nvim-m1)) — both bundle the server,
so most users never install it directly.

## Features

- **Diagnostics** — syntax errors, lint findings, and project-aware type
  diagnostics, live on open/change/save. Project-scope audit findings are
  published against the `Project.m1prj` itself, so the editor matches what the
  CLIs report.
- **Project awareness** — the nearest `Project.m1prj`, `parameters.m1cfg`,
  and `.m1dbc` files are discovered and loaded automatically, giving symbols
  their real types and units; the model reloads when those files change.
- **Navigation and insight** — hover (types, units, enum members, library
  signatures), go-to-definition, references and document highlights with
  read/write classification, document symbols, call hierarchy over the
  cross-script channel data-flow graph, folding, and semantic tokens.
- **Editing support** — completion (project symbols, locals, library
  methods), signature help, inlay type hints, local rename, code-action
  quick-fixes (including a whole-file "fix all auto-fixable lint issues"),
  whole-document and range formatting via `m1-fmt`, and a call-rate code lens
  per script.

Channel references are resolved through each script's group scope, so the
same channel written group-relative in one script and read full-path in
another collapses onto one entity — navigation follows the project's real
data flow, not textual spellings.

## Install

Prebuilt binaries for Linux x64, Windows x64, and Apple-Silicon macOS are
attached to each [release](https://github.com/C-Nucifora/m1-lsp/releases)
(the editor integrations consume these automatically). Or build from source:

```sh
cargo install --git https://github.com/C-Nucifora/m1-lsp.git --tag <latest>
```

Intel-Mac binaries are not published (GitHub no longer reliably provides
Intel-Mac CI runners) — build from source there and point your editor at the
binary.

Run with no arguments to start the server over stdio (how editors launch it).
`m1-lsp --scaffold-config` prints a starter `m1-tools.toml`; see the
[m1-tools configuration docs](https://github.com/C-Nucifora/m1-tools#configuration)
for how that file is shared across the toolchain. A minimal hand-rolled
Neovim setup (no plugin) lives in [`editors/nvim/`](editors/nvim/).

## Development

`m1-lsp` depends on every other crate in the toolchain (via versioned git-tag
dependencies, so it builds from a standalone clone) — a clean build here is
the toolchain's end-to-end integration check. It never imports tree-sitter
directly; all CST access goes through `m1-core`.

The CI gate is `cargo test`, `cargo clippy --all-targets -- -D warnings`, and
`cargo fmt --all -- --check`. A corpus smoke test analyses every `.m1scr`
under `$M1_CORPUS_PATH` (falling back to a sibling `m1-example/` checkout)
and asserts the server never panics; it skips if no corpus is present.

## License

GPL-3.0-or-later — see [LICENSE](LICENSE).

## Trademark

Independent, community-built open-source tooling for the MoTeC® M1 script
language. Not affiliated with, authorised, or endorsed by MoTeC Pty Ltd.
"MoTeC" and "M1" are trademarks of MoTeC Pty Ltd.
