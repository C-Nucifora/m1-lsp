# m1-lsp

A [Language Server Protocol](https://microsoft.github.io/language-server-protocol/)
server for the MoTeC M1 script language (`.m1scr`). It is the integration layer
at the top of the M1 toolchain: it parses files through `m1-core`, surfaces
syntax and lint diagnostics live, and formats documents through `m1-fmt`.

The primary target editor is **Neovim**, but the server speaks plain LSP over
stdio and works with any LSP client (VS Code, Helix, Emacs `eglot`, etc.).

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

## Status

v1. Hover, completion, goto-definition, semantic tokens, and type diagnostics
are deferred to v2 (they need the `m1-typecheck` symbol model).
