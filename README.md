# m1-lsp

A [Language Server Protocol](https://microsoft.github.io/language-server-protocol/)
server for the MoTeC M1 script language (`.m1scr`). It is the integration layer
at the top of the M1 toolchain: it parses files through `m1-core`, surfaces
syntax and lint diagnostics live, and formats documents through `m1-fmt`.

The primary target editor is **Neovim**, but the server speaks plain LSP over
stdio and works with any LSP client (VS Code, Helix, Emacs `eglot`, etc.).

## Workspace layout

`m1-lsp` sits at the **top** of the M1 toolchain, which lives in **six separate
repositories** coupled through Cargo **path** dependencies. They are not published
to crates.io, so this crate does **not** build from a standalone clone — check out
the whole set as siblings under one parent directory:

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
`m1-core`, `m1-lint`, `m1-fmt`, and `m1-typecheck` (each `{ path = "../<crate>" }`),
plus `tree-sitter-m1` transitively — so the entire set must be checked out
alongside it. A clean build of `m1-lsp` against its real siblings is the
toolchain's end-to-end integration check.

Because the repos are independent on GitHub, this coupling is **not visible
there**: each repo's CI and PRs see only itself, and there is no cross-repo PR
link. Build/merge ordering across the stack is a manual, local-workspace concern.
The `m1-example` example project (used by the corpus smoke test) is an optional further
sibling.

## Features (v1)

- **Diagnostics** (`textDocument/publishDiagnostics`): the union of
  - `m1-core` syntax diagnostics (source `m1-core`), and
  - `m1-lint` rule findings `L001`–`L009` (source `m1-lint`),

  recomputed on open / change / save.
- **Formatting** (`textDocument/formatting`): whole-document reformat via
  `m1-fmt`, returned as a single full-document `TextEdit`. No edits are returned
  when the document is already formatted or has syntax errors (pass-through
  safety).
- **Position encoding**: byte offsets from `m1-core` are converted to LSP
  positions in UTF-16 code units by default, or UTF-8 when the client negotiates
  it.
- **Document lifecycle**: full-document sync (`didOpen` / `didChange` /
  `didSave` / `didClose`).

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

v1. Hover, completion, goto-definition, semantic tokens, and type diagnostics
are deferred to v2 (they need the `m1-typecheck` symbol model).
