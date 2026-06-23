# m1-eval integration into m1-lsp — hover-to-evaluate + inline value hints

Plan date: 2026-06-23
Status: planning only (no code, no commits)
Phase: Phase 4 of the m1-eval design (`m1-eval/docs/specs/2026-06-23-m1-eval-design.md`,
"LSP integration — hover-to-evaluate + inline value hints via m1-lsp, reusing the
library API").

## Goal

When the user hovers a channel / parameter / expression in a `.m1scr`, the LSP shows
its **evaluated value** in addition to the existing type/symbol info. Optionally, render
inlay hints showing each assigned channel's computed value inline. Both are thin views
over a single cached `m1_eval::Trace` — the LSP runs no second engine and writes no
evaluation logic of its own (matches AGENTS.md: "this crate converts, integrates, and
serves").

All work is **additive and gated behind an opt-in capability/setting**: with eval
disabled or unavailable, hover and inlay behave exactly as today.

---

## Key design question: where do evaluated values come from?

An evaluation needs a scenario or a log. The decision (see `scenario_design` at the
bottom and Milestone E2):

1. **Configured scenario file** (highest fidelity) — a path in the LSP settings, e.g.
   `m1.eval.scenario = "scenarios/idle.toml"`, parsed by
   `Scenario::from_toml_str` / `Scenario::from_json_str`.
2. **Configured log** (counterfactual ground truth) — `m1.eval.log = "logs/run.csv"`,
   attached via `Engine::load_log`; values come from
   `Engine::run_counterfactual_diff().trace`.
3. **Offline default whole-project run** (fallback, no config) — a synthesised
   `RunMode::WholeProject` scenario with no inputs. Honest: most channels then read the
   evaluator's **offline default world** (calibration defaults, zero-seeded inputs,
   Tier-3 IO stubs). The hover must say so.

Honesty requirement: when a value comes from the offline default (no scenario/log) or
from a Tier-3 externally-driven stub (`Trace::is_external`), the hover labels it clearly
(`(offline default — no scenario)`, `(externally driven)`), never presenting a guessed
number as a measured one.

Caching/debounce: the `Trace` is computed **once per (project version + scenario/log
config)** and cached; hover/inlay read the cache. It is never recomputed on a hover. See
Milestone E3.

---

## Constraints discovered in the codebase

- **Dependency pinning (AGENTS.md "Dependencies and releases"):** upstream crates are
  pinned by **versioned git tag** only — never `branch`/`path`/`[patch]`. Add
  `m1-eval = { git = "https://github.com/C-Nucifora/m1-eval.git", tag = "v0.1.0" }`
  (the `v0.1.0` tag exists). m1-eval pins the **same** `m1-core` v0.12.0 and
  `m1-typecheck` v0.36.0 tags the LSP already uses, so the lockfile stays single-tag —
  no version skew (AGENTS.md: "Everything in the lockfile must resolve to the same
  m1-core tag").
- **`.ld` is feature-gated in m1-eval.** The default (CSV-only) build carries neither
  the `ld` feature nor the `motec-i2` dependency. The LSP depends on m1-eval **without**
  `features = ["ld"]`; a configured `.ld` log then fails loud through `Engine::load_log`
  ("rebuild with --features ld") and the LSP surfaces that as a one-line notice rather
  than crashing. (Enabling `ld` is a later, optional decision.)
- **No `[eval]` section exists in `M1ToolsConfig`.** That struct lives in
  `m1-workspace` (`m1_workspace::config::M1ToolsConfig`, pinned by tag) with only
  `lint` / `format` / `diagnostics` sections. Adding `[eval]` to `m1-tools.toml` would
  require an upstream m1-workspace release first. To stay additive and tag-pure, the
  scenario config is sourced from **LSP editor settings** (`m1.eval.*`, deserialised in
  the LSP's own `config.rs`/backend) plus optional **standalone scenario-file
  discovery** — not from `M1ToolsConfig`. A future `[eval]` block in `m1-tools.toml` can
  be added once m1-workspace ships it, without changing the hover/inlay surface.
- **Push XOR pull diagnostics and position-encoding negotiation are load-bearing** — do
  not touch them. Eval adds no diagnostics; it only enriches hover/inlay.

---

## Public API facts (verified against m1-eval @ v0.1.0 — not invented)

From `m1-eval/src/lib.rs`, `engine.rs`, `scenario.rs`, `trace.rs`, `value.rs`,
`diff.rs`:

- `m1_eval::Engine::load(project: &Path, cfg: Option<&Path>) -> Result<Engine, EvalError>`
- `Engine::run(&self, scenario: &Scenario) -> Result<Trace, EvalError>`
- `Engine::load_log(&mut self, path: &Path) -> Result<(), EvalError>` (`.csv` always;
  `.ld` only under the `ld` feature, else fail-loud)
- `Engine::override_channel(&mut self, spec: &str) -> Result<(), EvalError>` (`"CH=expr"`)
- `Engine::run_counterfactual(&self) -> Result<Trace, EvalError>` (needs a log)
- `Engine::run_counterfactual_diff(&self) -> Result<Counterfactual, EvalError>`
- `Engine::coverage(&self) -> CoverageReport`
- `Scenario::from_toml_str(&str) -> Result<Scenario, EvalError>` /
  `Scenario::from_json_str(&str) -> Result<Scenario, EvalError>` /
  `Scenario::load_csv(&mut self, &str) -> Result<(), EvalError>`
- `pub struct Scenario { mode: RunMode, inputs: Vec<InputSeries>, duration_s: f64,
  base_rate_hz: f64, overrides: Vec<InputSeries> }`
- `pub enum RunMode { Function(String), Cone(String), WholeProject }`
- `pub struct Trace { time: Vec<f64>, channels: BTreeMap<String, Vec<Value>>,
  exprs: BTreeMap<(String, usize), Vec<Value>>, external: BTreeSet<String> }`,
  `Trace::is_external(&self, path: &str) -> bool`
- `pub enum Value { Bool(bool), Int(i64), Uint(u64), Float(f64),
  Enum { id: usize, member: String }, Str(String) }`
- `pub struct Counterfactual { trace: Trace, diff: Diff }`;
  `Diff { time, channels: BTreeMap<String, ChannelDiff>, eps }`;
  `ChannelDiff { logged, counterfactual, delta, max_abs_delta, changed }`

Important corrections vs the design sketch: the methods named `apply_scenario` and
`override` in `m1-eval/docs/specs/...` **do not exist**. The real surface is
`run(&scenario)` and `override_channel(spec)`. Plan to the real API.

The clean boundary holds: every signature above uses only m1-eval's own types
(`Scenario`, `Trace`, `Value`, `EvalError`, `Counterfactual`) — no `m1-core` /
`m1-typecheck` type leaks. The LSP already depends on both upstreams, so `Value`
rendering needs nothing extra.

## m1-lsp facts (verified)

- Capabilities are built in `server_capabilities(encoding)` (`src/backend.rs:600`);
  `hover_provider` at `:636`, `inlay_hint_provider: Some(OneOf::Left(true))` at `:679`.
- `async fn hover` (`src/backend.rs:1039`) → `hover::hover(cst.root(), byte,
  p.map(|lp| &lp.project), doc.file_name.as_deref(), &doc.line_index, doc.enc)` inside
  `self.store.with_project(...)`.
- `async fn inlay_hint` (`src/backend.rs:1254`) → `inlay::inlay_hints(cst.root(),
  params.range, &doc.line_index, doc.enc, p.map(|lp| &lp.project),
  doc.file_name.as_deref())`.
- `pub fn hover::hover(root, byte, project: Option<&Project>, file_name, li, enc) ->
  Option<Hover>` (`src/features/hover.rs:590`). The branch that resolves a segment to a
  project symbol is `Resolution::Symbol(sym) => symbol_markdown(sym, project)`. **`sym.path`
  is the canonical channel path** (e.g. `Root.Demo.Output`) — the exact key shape used
  in `Trace::channels`. This is the join point for hover-to-evaluate.
- `pub fn inlay::inlay_hints(root, range, li, enc, project, file_name) -> Vec<InlayHint>`
  (`src/features/inlay.rs:14`). It already walks `MemberExpression` references and
  resolves them to symbols (`collect_unit_hints`) and visits `LocalDeclaration` /
  assignment targets — the same traversal an `= value` hint reuses.
- `pub fn locate::build_scope(root, project, file_name) -> Scope` and
  `locate::path_at_byte` / `segment_nodes` / `segment_at_byte` exist and are how
  hover canonicalises the segment under the cursor.
- `LoadedProject { project: Project, root: PathBuf, m1prj_path: PathBuf,
  m1cfg_path: Option<PathBuf>, dbc_paths, script_files }`
  (`src/project_store.rs:10`). `m1prj_path` + `m1cfg_path` are exactly the two `Path`
  arguments `Engine::load(project, cfg)` needs.
- `ProjectStore::with_project(|Option<&LoadedProject>| ...)` (`src/project_store.rs:209`)
  is the read accessor; `invalidate_call_graph` (`:219`) is the existing "project/buffer
  changed, drop caches" hook — the eval cache invalidates at the same points.
- `Backend` holds `config: RwLock<M1Config>`, `editor_settings: RwLock<Option<Value>>`,
  `config_root: RwLock<Option<PathBuf>>` (`src/backend.rs:59-65`); `reapply_config`
  (`:196`) and `did_change_configuration` (`:1805`) are where new settings land.
- `file_name` (the open script's basename) maps to its function symbol via
  `Project::function_symbol_for_script` and to the `RunMode::Function`/`Cone` target —
  used to scope which scenario mode the offline default run uses.

---

## TDD milestones (ordered)

Each milestone: write failing tests first, implement minimally, run the gate
(`cargo test && cargo clippy --all-targets -- -D warnings && cargo fmt --all -- --check`).
Keep every change additive; with eval off, existing hover/inlay tests stay green.

### E0 — Add the dependency and an `eval` module skeleton (no behaviour change)

- Add to `Cargo.toml [dependencies]`:
  `m1-eval = { git = "https://github.com/C-Nucifora/m1-eval.git", tag = "v0.1.0" }`
  (no `features = ["ld"]`). Confirm `cargo build` resolves with the existing single
  m1-core/m1-typecheck tags (no lockfile skew).
- New module `src/eval/mod.rs` (declared in `src/lib.rs`), initially empty re-exports.
- Test: a compile-level test asserting `m1_eval::Engine`, `Scenario`, `Trace`, `Value`
  are reachable and the crate builds. No runtime behaviour yet.
- Gate: lockfile resolves to one m1-core tag; `cargo tree` shows no second m1-core.

### E1 — `EvalConfig` from LSP editor settings (off by default)

- In `src/config.rs` (or a new `src/eval/config.rs`), add an LSP-local `EvalConfig`
  deserialised from the `m1.eval.*` editor-settings JSON the backend already receives:
  `enabled: bool` (default `false`), `scenario: Option<PathBuf>`,
  `log: Option<PathBuf>`, `tick: TickPolicy` (default = last tick),
  `inlay_values: bool` (default `false`). Do **not** touch `M1ToolsConfig` /
  `m1-tools.toml` (no upstream change needed).
- Wire it into the existing `editor_settings` → `reapply_config` path so
  `did_change_configuration` re-reads it live.
- Tests: default config has eval disabled; a settings JSON with
  `{"eval": {"enabled": true, "scenario": "s.toml"}}` parses into the expected
  `EvalConfig`; an unknown/garbage eval payload degrades to disabled with an issue line
  (mirrors the existing `resolve_with_issues` pattern), never panics.

### E2 — Scenario sourcing: build the Engine and run once

- New `src/eval/engine.rs`: a function that, given a `&LoadedProject` and `&EvalConfig`,
  resolves the scenario source in precedence order and produces a `Result<Trace, …>`:
  1. `scenario` path set → `Scenario::from_{toml,json}_str` (by extension) → `engine.run`.
  2. else `log` path set → `engine.load_log` (+ optional `override_channel`) →
     `engine.run_counterfactual_diff().trace`.
  3. else → a synthesised offline-default `RunMode::WholeProject` scenario (no inputs,
     project-derived base rate, short bounded duration) → `engine.run`.
- `Engine::load(&lp.m1prj_path, lp.m1cfg_path.as_deref())` — the two paths already on
  `LoadedProject`.
- Carry a `provenance` enum alongside the trace: `Scenario(path) | Log(path) |
  OfflineDefault`, so downstream rendering can be honest.
- Fail-loud surfacing: an unparsable scenario / missing log / `.ld`-without-feature
  returns an error that the backend logs once via `window/logMessage` and then **falls
  back to the offline default** (so hover still works), never crashing the handler.
- Tests (using a small fixture project under `tests/fixtures/`, mirroring m1-eval's
  `mini`): scenario path → trace has the expected channel column; missing scenario →
  offline-default provenance; bad `.ld` path → fail-loud error captured, fallback engaged.

### E3 — Cached, debounced evaluation in the ProjectStore

- Add an eval cache to `ProjectStore` (or a sibling holder on `Backend`):
  `RwLock<Option<CachedEval>>` where `CachedEval { trace: Arc<Trace>,
  provenance: Provenance, key: EvalKey }`. `EvalKey` hashes (project reload generation +
  resolved `EvalConfig`). A `with_eval(|Option<&CachedEval>| …)` accessor builds on miss
  and reuses on hit — same shape as `with_call_graph` (`src/project_store.rs:230`).
- Invalidate the eval cache at exactly the points the call-graph cache is dropped
  (`invalidate_call_graph`, called on edit/open/close/save/reload) **plus** on
  `did_change_configuration`. The trace reflects the saved project model, not unsaved
  keystrokes — document this (a buffer edit invalidates; the rebuild uses the
  last-loaded project, which is the honest source for evaluated values).
- Debounce: the cache means hover/inlay never trigger a run directly; the first
  hover/inlay after an invalidation triggers one build, subsequent ones hit the cache.
  Optionally compute lazily off the async worker via `block_in_place` (the pattern used
  for call-hierarchy/diagnostics at `src/backend.rs:1302` etc.) so a hover never blocks
  the runtime.
- Tests: two hovers in a row build the trace once (a build counter asserts a single
  run); a config change bumps `EvalKey` and forces a rebuild; a project reload
  invalidates.

### E4 — Hover-to-evaluate (the headline)

- New `src/eval/render.rs`: `value_markdown(v: &Value) -> String` (reuses
  `value_type_str` styling; renders enums as `member`, floats compactly), and
  `eval_hover_fragment(path: &str, trace: &Trace, provenance: &Provenance,
  tick: TickPolicy) -> Option<String>` that looks up `trace.channels.get(path)` at the
  chosen tick and formats:
  `\n\nvalue: \`50\` (@ t=0.02s)` plus an honesty suffix —
  `(offline default — no scenario)` when `provenance == OfflineDefault`, and
  `(externally driven)` when `trace.is_external(path)`.
- In `hover::hover`, **extend** the `Resolution::Symbol(sym)` arm only: after
  `symbol_markdown(sym, project)`, append `eval_hover_fragment(&sym.path, …)` when an
  eval trace is available. Plumb an optional `Option<&Trace>` + provenance + tick into
  `hover::hover` (new params with `None` defaults so existing call sites/tests are
  unaffected; the backend passes the cached trace). Type/symbol info is unchanged and
  always shown first.
- Backend: in `async fn hover`, fetch the cached trace via `with_eval` and pass it in.
  When eval is disabled or no trace exists, pass `None` → byte-identical to today.
- Tests:
  - With a scenario fixture, hovering `Root.Demo.Output` shows `value:` alongside the
    existing `type:`/badges.
  - With no scenario, the same hover shows the value **and** the
    `(offline default — no scenario)` label.
  - A Tier-3 / external channel shows `(externally driven)`.
  - Eval disabled → hover markdown equals the pre-eval baseline (regression guard).
  - A non-value symbol (group/function/table) gets no `value:` line.

### E5 — Expression-level hover (per-node values)

- `Trace::exprs` is keyed by `(script_name, byte_offset)`. When the hovered segment is
  an expression occurrence rather than a channel symbol, look up
  `trace.exprs.get(&(file_name.to_string(), seg_byte_offset))` and render the same
  `value:` fragment. Requires the offline-default/scenario run to have an active expr
  sink for the open script (note: the sink only records expressions the runner evaluated;
  a sparse miss simply yields no value line — honest, not an error).
- Tests: hovering a sub-expression whose `(script, offset)` is present renders its value;
  a segment with no recorded expr value adds no `value:` line and the rest of the hover is
  unchanged.
- Risk note: byte offsets in `Trace::exprs` are the evaluator's view of the **saved**
  script; if the open buffer is edited, offsets can drift. Gate expr-hover on the buffer
  being unmodified-since-load (or skip it), documented as a known limitation — channel
  hover (E4) is unaffected since it keys on canonical paths, not offsets.

### E6 — Inline computed-value inlay hints (opt-in)

- Gate behind `EvalConfig.inlay_values` (default off) **and** a check that an eval trace
  exists. Extend `inlay::inlay_hints` with an optional `Option<&Trace>` + provenance +
  tick (new params, `None`-default so existing tests/call sites are unchanged).
- New `collect_value_hints`: reuse the existing `MemberExpression`/assignment-target
  traversal in `inlay.rs`; for each channel reference/assignment target that resolves to
  a symbol with a column in `trace.channels`, emit a trailing
  `= <value>` `InlayHintKind::TYPE` hint (padding mirrors the existing `[unit]` hints).
  Offline-default provenance renders a muted marker (e.g. `= 50?`) or a tooltip noting
  it is the offline default, so an inline number is never mistaken for a measured one.
- Backend `async fn inlay_hint` passes the cached trace when `inlay_values` is on.
- Tests: with a scenario and `inlay_values=true`, an assignment line gets a `= value`
  hint; with `inlay_values=false` (default) no value hints appear (only the existing
  type/unit/param hints); offline-default run renders the muted/annotated form.

### E7 — Capability gating, docs, and editor wiring

- The hover enrichment rides on the existing `hover_provider` (no new server
  capability needed — it is the same response, enriched). The value-inlay is gated by the
  `m1.eval.inlay_values` **setting**, not a new LSP capability, so a client that does not
  set it sees today's behaviour. No change to the negotiated capability set in
  `server_capabilities` is required; document this explicitly.
- Update `README.md` / editor settings schemas (`editors/`, and note the downstream
  `m1-vscode` / `nvim-m1` settings) to describe `m1.eval.enabled`, `m1.eval.scenario`,
  `m1.eval.log`, `m1.eval.inlay_values`, and the offline-default honesty behaviour.
- Tests: an end-to-end backend test (in `tests/`) driving `initialize` →
  `didChangeConfiguration` (enable eval + scenario) → `hover` showing a value; and the
  inverse (eval off) showing the unchanged hover. Verify the corpus smoke path and the
  build gate stay green.

### E8 (optional, later) — `.ld` log support and `[eval]` in `m1-tools.toml`

- Optionally enable m1-eval's `ld` feature for binary log import (pulls `motec-i2`);
  weigh the heavier dependency against the CSV-only default. Until then, a configured
  `.ld` log fails loud with the "rebuild with --features ld" message surfaced as a
  notice.
- Once m1-workspace ships an `[eval]` section in `M1ToolsConfig`, add it as a config
  layer **under** the LSP editor settings (mirroring the `defaults < m1-tools.toml <
  tool file < editor` precedence the design specifies), without changing the hover/inlay
  surface built in E4–E6.

---

## Risks / honesty notes

- **Offline default is the common case.** Most users will not configure a scenario/log,
  so most hovered values come from the offline default world. Surfacing this clearly
  (E4/E6) is non-negotiable — an unlabelled number would be misleading.
- **Expr-offset drift on edits** (E5): channel hover is path-keyed and safe; expr hover
  is offset-keyed and gated on an unmodified buffer.
- **Run cost on large projects:** whole-project offline runs over a real corpus can be
  non-trivial; the cache (E3) plus a bounded default duration and `block_in_place`
  off-loading keep the editor responsive. If still too slow, fall back to a `Cone` run
  scoped to the open script's target channels.
- **No second engine, no analysis here:** all evaluation is m1-eval's; the LSP only
  caches a `Trace` and renders it (AGENTS.md boundary discipline).
