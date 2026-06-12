# AGENTS.md — m1-lsp

Guidance for coding agents working in this repository.

## Purpose

The language server at the top of the M1 toolchain — the integration layer
that turns the libraries (`m1-core`, `m1-lint`, `m1-fmt`, `m1-typecheck`)
into an editor experience. Two editor integrations sit on top and bundle the
released binary: VS Code (`m1-vscode`) and Neovim (`nvim-m1`). A behaviour
change here lands in both editors at their next pin bump — think about both
clients, not just the one you're testing in.

## Things that are deliberate (don't "fix" them)

- **No direct tree-sitter / no semantic logic of its own.** CST access goes
  through `m1-core`; lint rules live in `m1-lint`; type/semantic analysis in
  `m1-typecheck`; formatting in `m1-fmt`. This crate converts, integrates,
  and serves — if you're writing analysis logic here, it belongs upstream.
- **Push XOR pull diagnostics.** Publishing diagnostics *and* advertising
  pull-diagnostic capability makes clients show everything twice. Push is
  gated on the client's pull capability — keep it that way.
- **Position encoding is negotiated.** Byte offsets from `m1-core` are
  converted to UTF-16 code units (LSP default) or UTF-8 when the client asks.
  Never index source text with raw LSP positions; a mid-codepoint slice is a
  remotely-triggerable panic.
- **Project-scope findings publish against the `.m1prj` URI**, not the open
  script — the editor must match what the CLIs report.
- **Pass-through safety on format.** No edits are returned for an already
  formatted or syntactically broken document.

## Testing honestly

Headless LSP tests and Neovim miss client-specific behaviour (the
double-diagnostics bug above was only visible in a real VS Code Extension
Host). For protocol-level changes, verify in a real editor against a real
corpus, not just `cargo test`. The corpus smoke test needs `$M1_CORPUS_PATH`
(or a sibling `m1-example/`) and skips when absent. Also note: a live server
keeps running across an editor's toolchain update — "stale" behaviour after
an upgrade usually just needs a server restart, not a bug hunt.

## Build / test gate

```sh
cargo test
cargo clippy --all-targets -- -D warnings
cargo fmt --all -- --check
```

CI also runs rustdoc with `-D warnings`, a security audit, and an MSRV job.
The MSRV pin in CI (`dtolnay/rust-toolchain@<version>`) must stay in sync with
`rust-version` in `Cargo.toml` — never bump one without the other.

## Dependencies and releases

All upstream crates are pinned by **versioned git tag** — never
`branch`/`path`/`[patch]`; the repo must build exactly like a public clone.
Everything in the lockfile must resolve to the *same* `m1-core` tag, which
means an `m1-core` bump here usually waits for same-tag `m1-fmt`/`m1-lint`/
`m1-typecheck` releases first.

This is a binary repo: a version bump on `main` makes `release.yml` tag it
and upload prebuilt binaries (Linux x64, Windows x64, macOS arm64) — the
editor integrations consume those. After releasing, open the downstream pin
bumps (`m1-vscode` server pin, `nvim-m1` bundled version, the `m1-ci`
tool-version pin) immediately rather than waiting for automation.

The CLI surface must keep accepting `--stdio`: vscode-languageclient appends
it, and the VS Code extension fails to start the server without it.
