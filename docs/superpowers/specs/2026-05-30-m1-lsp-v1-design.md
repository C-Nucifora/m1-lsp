# m1-lsp v1 — Design Specification

**Date:** 2026-05-30
**Status:** Approved for implementation
**Scope:** v1 — language server tying together `m1-core`, `m1-lint`, and `m1-fmt`, with Neovim integration

> **Note:** Example identifiers in this document are synthetic placeholders, not
> drawn from any real project. The example-corpus path is resolved via the
> `M1_CORPUS_PATH` env var (falling back to the sibling m1-example example project).

---

## 1. Purpose

`m1-lsp` is a [Language Server Protocol](https://microsoft.github.io/language-server-protocol/)
server for the MoTeC M1 script language (`.m1scr`). It is the integration layer
that brings the rest of the toolchain into an editor: it parses files through
`m1-core`, surfaces syntax and lint diagnostics live as you type, and formats
documents through `m1-fmt`. The primary target editor is **Neovim**, but the
server speaks plain LSP over stdio and works with any LSP client (VS Code,
Helix, Emacs `eglot`, etc.).

`m1-lsp` occupies the top of the toolchain:

```
tree-sitter-m1  (grammar)
      ↓
m1-core         (parse, CST, diagnostics, byte-ranged positions)
      ↓
m1-lint  ──┐
m1-fmt   ──┤
m1-typecheck (v2) ──┐
                    ↓
m1-lsp          (this crate — LSP server + editor glue)
```

`m1-lsp` depends on `m1-core`, `m1-lint`, and `m1-fmt`. It must NOT import
`tree-sitter` directly; all CST access is via `m1-core`.

---

## 2. Scope

### 2.1 In-scope (v1)

- **Document lifecycle**: `initialize`, `initialized`, `shutdown`, `exit`;
  `textDocument/didOpen`, `didChange` (full sync), `didSave`, `didClose`.
- **Diagnostics** (`textDocument/publishDiagnostics`): the union of
  - `m1-core` syntax diagnostics (`SyntaxError`, `MissingToken`), and
  - `m1-lint` rule diagnostics (`L001`–`L009`),
  recomputed on open/change/save and published asynchronously.
- **Formatting** (`textDocument/formatting`): whole-document reformat via
  `m1_fmt::format_str`. Returns a single full-document `TextEdit`, or no edits
  when the document is unchanged or has syntax errors (pass-through safety).
- **Position encoding**: correct conversion from `m1-core`'s byte offsets/byte
  columns to LSP positions (UTF-16 code units by default; honour the client's
  declared `positionEncoding` when the negotiation is supported).
- **Server capabilities advertisement** matching exactly the above.
- **Neovim integration assets**: a documented `vim.lsp.start` configuration,
  `.m1scr` filetype registration, root-directory detection, and a pointer to the
  `tree-sitter-m1` queries for `nvim-treesitter` highlight/indent/fold.

### 2.2 Out-of-scope (v1) — YAGNI

| Feature | Why deferred |
|---------|--------------|
| Hover, completion, signature help | Need the symbol model / `m1-typecheck`; v2 |
| Goto-definition / references | Need cross-file symbol resolution; v2 |
| Semantic tokens | `nvim-treesitter` already highlights via the grammar; v2 |
| Type diagnostics | Produced by `m1-typecheck`; wired in once that ships |
| Incremental (range) sync | Files are small; full sync is simpler and correct |
| Range / on-type formatting | Whole-document formatting suffices for v1 |
| Code actions / quick fixes | Depends on `--fix` support in lint/fmt; v2 |
| Workspace symbol / project-wide indexing | Needs `.m1prj` model (`m1-typecheck`); v2 |
| Configuration UI | One sensible default profile; init options only (§6) |
| Published VS Code extension | Neovim is the v1 target; LSP is editor-agnostic |

---

## 3. Key Decisions

### 3.1 Framework: `tower-lsp`

**Decision:** Build on [`tower-lsp`](https://crates.io/crates/tower-lsp) (async,
`tokio`) rather than hand-rolling the JSON-RPC framing or using a synchronous
crate.

**Justification:** `tower-lsp` provides the `LanguageServer` trait, request
routing, JSON-RPC framing over stdio, and `lsp-types`. It is the de-facto
standard for Rust LSP servers (`rust-analyzer`-adjacent tooling, `taplo`,
`wgsl-analyzer`, etc.). The async model is irrelevant to correctness here (our
analysis is fast and synchronous) but costs nothing and keeps the door open for
background project indexing in v2.

**Transport:** stdio only in v1 (`tower_lsp::Server::new(stdin, stdout, socket)`).
This is what Neovim's `vim.lsp.start` launches. TCP/socket mode is not needed.

### 3.2 Document store: in-memory, full-text, full-sync

**Decision:** Keep an in-memory map `Uri -> Document`, where `Document` owns the
current full text and a cached `LineIndex` (see §3.4). On `didChange`, replace
the whole text (the server advertises `TextDocumentSyncKind::FULL`).

**Justification:** `.m1scr` files are small (the largest example-corpus script is
a few KB). Full-document sync removes an entire class of incremental-patch bugs.
A `DashMap<Url, Document>` (or `tokio::sync::Mutex<HashMap<...>>`) gives
concurrent access from async handlers without contention concerns at this scale.

The CST is **not** cached across edits in v1 — we re-`parse` on demand inside the
diagnostics pass. Parsing is sub-millisecond and re-parsing keeps state trivial.
(A cached/incrementally-reparsed tree is a v2 optimisation if profiling demands.)

### 3.3 Diagnostics: union of producers, one publish per document version

**Decision:** A single `analyze(document) -> Vec<lsp::Diagnostic>` function:
1. `let cst = m1_core::parse(text);`
2. Convert `cst.syntax_diagnostics()` → LSP diagnostics with
   `source = "m1-core"`, `code` = the `m1_core::Code` name.
3. Run `m1_lint::Runner::new(Registry::default_v1()).run_source(text)`;
   convert each `LintDiagnostic` → LSP diagnostic with `source = "m1-lint"`,
   `code` = the `LintCode` (e.g. `"L004"`).
4. Concatenate and publish via `client.publish_diagnostics(uri, diags, version)`.

`m1-lint`'s `RunResult` already re-parses internally and exposes
`syntax_errors`; to avoid double-reporting syntax errors, `m1-lsp` takes syntax
diagnostics from `m1-core` directly and uses only `RunResult::diagnostics`
(the lint findings) from the lint runner.

Severity mapping: `m1_core::Severity::{Error,Warning,Info,Hint}` →
`lsp::DiagnosticSeverity::{ERROR,WARNING,INFORMATION,HINT}`.

### 3.4 Position conversion: the one piece of real complexity

`m1-core` documents that `Position::column` is a **byte** offset within a line
and that "UTF-16/LSP position conversion is the responsibility of `m1-lsp`."
This crate owns that conversion.

**Decision:** Build a `LineIndex` from the document text that records the byte
offset of each line start. Provide:
- `byte_to_position(byte: usize) -> lsp::Position` — find the line via binary
  search over line starts, then count **UTF-16 code units** from the line start
  to `byte` (the default LSP encoding).
- `m1_core::Range -> lsp::Range` using `byte_range` (preferred — unambiguous)
  rather than the byte-column `Range`, so the conversion has a single code path.
- `lsp::Position -> byte: usize` for inbound positions (formatting range, future
  features).

**Encoding negotiation:** advertise support for `utf-16` (always) and `utf-8`
(if the client lists it in `general.positionEncodings`). Store the negotiated
encoding on the backend and branch the per-line counting on it. Neovim defaults
to `utf-16`; `.m1scr` is ASCII in practice, so the two encodings coincide for
real files, but the code is correct for multi-byte input regardless.

### 3.5 Formatting safety

`textDocument/formatting` calls `m1_fmt::format_str(text)`:
- `Ok(result)` with `result.changed == true` → return one `TextEdit` replacing
  the whole document range `[0,0]..[last_line, last_col]` with `result.output`.
- `result.changed == false` → return `None` (no edits).
- `Err(FormatError::SyntaxErrors(_))` (or the pass-through `changed == false`
  case, depending on the final `m1-fmt` API) → return `None`; do not surface an
  error to the client. The user already sees the syntax diagnostics.

The formatter never produces invalid output (its own invariant), so the server
never needs to validate the result before returning it.

### 3.6 Build-order prerequisite

`m1-lsp` depends on the **library crates** of `m1-lint` (`m1_lint::Runner`,
`Registry`, `LintDiagnostic`) and `m1-fmt` (`m1_fmt::format_str`,
`FormatResult`). Those crates have approved plans but are not yet implemented.

**Decision:** The diagnostics and formatting providers are isolated behind two
internal traits (`DiagnosticSource`, `Formatter`) so the server compiles and is
testable against `m1-core` alone, with `m1-lint`/`m1-fmt`-backed implementations
wired in once those crates exist. The implementation plan sequences this: a
core-only server first (syntax diagnostics + identity formatter stub), then the
lint and fmt integrations as they become available. This keeps `m1-lsp`
progressable without a hard block on the sibling crates.

---

## 4. Architecture

### 4.1 Crate layout

```
m1-lsp/
  Cargo.toml          (bin crate; deps: tower-lsp, tokio, m1-core, m1-lint, m1-fmt)
  src/
    main.rs           (tokio runtime, stdio transport, Server bootstrap)
    backend.rs        (Backend: impl LanguageServer — the request handlers)
    document.rs       (Document: text + LineIndex; the in-memory store)
    line_index.rs     (LineIndex + byte<->LSP position conversion, encoding-aware)
    analysis.rs       (analyze(): core syntax + lint -> Vec<lsp::Diagnostic>)
    format.rs         (formatting handler -> Option<Vec<TextEdit>>)
    convert.rs        (m1_core/m1_lint -> lsp-types mapping: severity, range, code)
  editors/
    nvim/
      m1.lua          (filetype + vim.lsp.start + root detection; copy-paste setup)
      README.md       (Neovim install/setup instructions, incl. nvim-treesitter)
  tests/
    lifecycle.rs      (initialize/shutdown handshake; capabilities advertised)
    diagnostics.rs    (didOpen a snippet -> expected diagnostics published)
    formatting.rs     (formatting request -> expected TextEdit)
    line_index.rs     (byte<->position conversion incl. multi-byte unit tests)
```

### 4.2 Module responsibilities

**`main.rs`** — build the `tokio` runtime, construct
`LspService::new(|client| Backend::new(client))`, and serve over
`tokio::io::{stdin, stdout}`. No logic beyond bootstrap.

**`backend.rs`** — `struct Backend { client: Client, docs: DashMap<Url, Document>,
encoding: PositionEncoding }`. Implements `#[tower_lsp::async_trait] LanguageServer`:
- `initialize`: negotiate `positionEncoding`; return `ServerCapabilities`
  (`text_document_sync = FULL`, `document_formatting_provider = Some(true)`).
- `initialized`: log readiness.
- `did_open` / `did_change` / `did_save`: update the doc store, then call
  `publish(uri)`.
- `did_close`: drop the doc and clear its diagnostics.
- `formatting`: delegate to `format.rs`.
- `shutdown`: ok.
- private `publish(uri)`: `analyze` + `client.publish_diagnostics(...)`.

**`document.rs`** — `struct Document { text: String, line_index: LineIndex,
version: i32 }`, rebuilt on each full-sync update.

**`line_index.rs`** — `struct LineIndex { line_starts: Vec<usize> }` plus the
conversion functions in §3.4. Encoding-aware via a `PositionEncoding` enum.
Pure, no LSP-runtime dependency beyond `lsp-types`.

**`analysis.rs`** — `pub fn analyze(text: &str, line_index: &LineIndex,
enc: PositionEncoding) -> Vec<Diagnostic>`. Orchestrates core + lint and maps
through `convert.rs`. Pure function of its inputs (no I/O, no `Client`), so it is
unit-testable without a running server.

**`format.rs`** — `pub fn format(doc: &Document) -> Option<Vec<TextEdit>>`.

**`convert.rs`** — the small, well-tested mapping layer (severity, range, code,
`source` strings).

### 4.3 Data flow

```
        Neovim (LSP client)
            │  JSON-RPC over stdio
            ▼
 ┌──────────────────────────────────────────────┐
 │ backend.rs  (tower-lsp LanguageServer)         │
 │  did_open/did_change/did_save                  │
 │     └─ docs.insert(uri, Document::new(text))   │
 │     └─ publish(uri):                           │
 │          analysis::analyze(text, line_index)   │
 │             ├─ m1_core::parse → syntax diags    │
 │             └─ m1_lint::Runner::run_source      │
 │          convert → Vec<lsp::Diagnostic>         │
 │          client.publish_diagnostics(...)        │
 │  formatting                                     │
 │     └─ format::format(doc):                     │
 │          m1_fmt::format_str → Option<TextEdit>  │
 └──────────────────────────────────────────────┘
```

---

## 5. Neovim integration

Two independent layers, both documented in `editors/nvim/README.md`:

1. **Syntax highlighting / indent / fold** — provided by `tree-sitter-m1`'s
   queries through `nvim-treesitter`. Setup: register the `m1` parser (path or
   git), map the `m1scr` filetype to it, install the queries. This needs no LSP.

2. **Diagnostics + formatting** — `m1-lsp`. Setup in `editors/nvim/m1.lua`:
   - `vim.filetype.add({ extension = { m1scr = "m1scr" } })` (and optionally
     `m1prj`/`m1cfg` for future use).
   - A `FileType m1scr` autocmd calling `vim.lsp.start({ name = "m1-lsp",
     cmd = { "m1-lsp" }, root_dir = ... })`.
   - `root_dir`: nearest ancestor containing `Project.m1prj`, else the nearest
     `.git`, else the file's directory.
   - Formatting bound via `vim.lsp.buf.format()` (and an optional format-on-save
     autocmd guarded by a user flag).

The Lua is copy-paste ready and dependency-free (no plugin manager assumptions),
with an optional `nvim-lspconfig` snippet shown as an alternative.

---

## 6. Configuration

v1 reads LSP **initialization options** (no config file):

```jsonc
{
  "format_on_save": false,   // advisory hint for the editor snippet, not server-enforced
  "lint": true               // toggle lint diagnostics (syntax always on)
}
```

Unknown options are ignored. Lint thresholds remain `m1-lint`'s compile-time
defaults in v1 (a `.m1lint.toml` is `m1-lint`'s own v2 concern). There is no
project (`.m1prj`) loading in v1; that arrives with `m1-typecheck`.

---

## 7. Error handling

| Condition | Behaviour |
|-----------|-----------|
| Document has syntax errors | Publish them as diagnostics; formatting returns no edits |
| `analyze` on malformed text | Never panics; returns whatever diagnostics were produced |
| Formatting request on unknown URI | Return `Ok(None)` (no edits) |
| `m1_fmt` returns `Err`/pass-through | Return `Ok(None)`; do not error the client |
| Panic inside a handler | `tower-lsp` isolates the request; log via `client.log_message` |
| Client requests an unsupported capability | Not advertised → client won't call it |

The server never writes files and never mutates documents itself — all changes
flow back to the editor as `TextEdit`s the client applies.

---

## 8. Testing strategy

- **`line_index.rs` unit tests** — the highest-risk code. Round-trip
  `byte ↔ position` on ASCII, on multi-byte UTF-8 (so UTF-16 unit counting is
  exercised), on `\n`-only and trailing-newline edge cases, and empty documents.
- **`analysis.rs` unit tests** — feed snippets (`x = a == b;`, a too-long line,
  a syntax-error fragment) and assert the exact set of `(code, line, severity)`
  tuples. No server needed.
- **`format.rs` unit tests** — unformatted snippet → expected single full-range
  `TextEdit`; already-formatted snippet → `None`; syntax-error snippet → `None`.
- **Integration tests** (`tests/`) — drive a `Backend` in-process: send
  `initialize` and assert advertised capabilities; `didOpen` a snippet and assert
  the published diagnostics (via a recording test `Client`); send a `formatting`
  request and assert the returned edit. Use `tower-lsp`'s testing harness /
  an in-memory duplex transport.
- **Corpus smoke test** — for each script under `$M1_CORPUS_PATH` (skipped if
  unset), `analyze` must not panic and must terminate. Mirrors the other crates'
  corpus gate.

---

## 9. Resolved decisions / assumptions

1. **Editor target:** Neovim is the v1 priority; the server is plain LSP so other
   clients work without server changes.
2. **Sync model:** full-document (`TextDocumentSyncKind::FULL`).
3. **CST caching:** none in v1 (re-parse per analysis).
4. **Position encoding:** UTF-16 default, UTF-8 if negotiated.
5. **Lint/fmt availability:** abstracted behind internal traits so the server is
   buildable and testable before those crates are implemented; real backends are
   wired in per the implementation plan's sequencing.
6. **No project model in v1:** `.m1prj`/`.m1cfg` loading and the features that
   need it (hover types, goto-def, type diagnostics) are deferred to the
   `m1-typecheck` integration (v2).

---

## 10. Non-goals

- `m1-lsp` does not parse `.m1scr` itself (delegates to `m1-core`), define lint
  rules (delegates to `m1-lint`), or implement formatting logic (delegates to
  `m1-fmt`).
- `m1-lsp` does not resolve channel/parameter types or load the project model in
  v1.
- `m1-lsp` does not ship an editor plugin/package; it ships a server binary plus
  copy-paste Neovim configuration.
