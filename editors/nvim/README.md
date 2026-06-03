# m1-lsp in Neovim

> **Using more than one M1 tool?** The recommended way to set up M1 in Neovim is
> the unified [nvim-m1](https://github.com/C-Nucifora/nvim-m1) plugin, which wires
> tree-sitter, `m1-lsp`, `m1-fmt`, and `m1-lint` together behind a single `setup`
> call. The standalone setup below configures **only `m1-lsp`** (plus optional
> tree-sitter highlighting) — use it if you want the language server on its own.

Two independent layers. You can use either or both.

## 1. Diagnostics + formatting (m1-lsp)

### Option A — lazy.nvim plugin (recommended)

Add one entry to your lazy.nvim spec. The `build` step compiles the server
inside the plugin directory; no separate install or `$PATH` config needed.

```lua
{
  'C-Nucifora/m1-lsp',
  build = 'cargo build --release',
  dependencies = { 'neovim/nvim-lspconfig' },
  config = function()
    require('m1_lsp').setup({})
  end,
}
```

The filetype (`m1scr`) is registered automatically when the plugin loads.

#### Passing custom options

`setup()` accepts any key that `lspconfig`'s `setup()` accepts. Common
overrides:

```lua
require('m1_lsp').setup({
  -- attach keymaps and enable completion
  on_attach = function(client, bufnr)
    local opts = { buffer = bufnr, silent = true }
    vim.keymap.set('n', 'K',  vim.lsp.buf.hover,            opts)
    vim.keymap.set('n', 'gd', vim.lsp.buf.definition,        opts)
    vim.keymap.set('n', 'gO', vim.lsp.buf.document_symbol,   opts)
    if vim.lsp.completion and vim.lsp.completion.enable then
      vim.lsp.completion.enable(true, client.id, bufnr, { autotrigger = false })
    end
  end,

  -- forward nvim-cmp / blink.cmp capabilities
  capabilities = require('cmp_nvim_lsp').default_capabilities(),

  -- override root detection (e.g. always use cwd)
  root_dir = function(_fname)
    return vim.fn.getcwd()
  end,
})
```

### Option B — manual (no plugin manager)

1. Build the server: `cargo build --release` (or `cargo install --path .`).
   Ensure `m1-lsp` is on your `$PATH`.
2. Load `m1.lua` from your config: `require("m1")` (after putting `m1.lua` on
   your `runtimepath`), or paste its contents into `init.lua`.
3. Open a `.m1scr` file. Diagnostics appear as you type; format with
   `:lua vim.lsp.buf.format()` or your usual mapping.

### Diagnostics sources

Diagnostics come from three sources: `m1-core` (syntax), `m1-lint` (style
rules), and `m1-typecheck` (type rules `T001`-`T011`). When a project is
loaded, the type-aware float-equality rule `T002` supersedes the `m1-lint`
`L006` heuristic (no double-reporting).

### v2 features (symbol model)

Powered by `m1-typecheck`'s project symbol model. The `m1.lua` snippet wires
buffer-local keymaps on `LspAttach`:

- **Hover** — `K`: shows a symbol's kind / value type / unit, or a local's
  inferred (Hungarian) type, or "built-in / opaque".
- **Goto-definition** — `gd`: jumps to a function's/method's defining `.m1scr`
  file (the only symbols that carry a backing file). Channels/parameters/etc.
  are declared in `Project.m1prj` with no source span, so they have no jump
  target.
- **Document symbols** — `gO`: a flat outline of the file's locals and
  top-level assignment targets.
- **Completion** — project symbols (both their absolute path and the
  group-relative tail for the current file's group) plus in-scope locals. Use
  the Nvim 0.11+ built-in completion (enabled by the snippet) or
  `<C-x><C-o>` via `omnifunc`.
- **Rename** — `<leader>rn` (or the built-in `grn` on Nvim 0.11+): renames a
  `local` variable and every reference to it in the file. Only locals are
  renameable — channels, parameters and other project symbols are declared in
  `Project.m1prj`, not the script, so `prepareRename` rejects them.
- **References** — `gr` (or the built-in `grr` on Nvim 0.11+): lists every
  in-file occurrence of the local / channel / symbol under the cursor.
- **Document highlights** — occurrences of the symbol under the cursor are
  underlined while it rests there (read vs write classified), via the
  `CursorHold` autocmd in the snippet.
- **Code actions** — `<leader>ca` (or the built-in `gra` on Nvim 0.11+): a
  quick-fix replaces an unsupported C operator with its M1 keyword
  (`==`→`eq`, `!=`→`neq`, `&&`→`and`, `||`→`or`, `!`→`not`).
- **Signature help** — `<leader>k` in normal mode (and `<C-s>` in insert on Nvim
  0.11+); also auto-pops on `(` / `,`. Shows the library-function overload with
  the active argument highlighted.
- **Inline type hints** — an inlay `: Type` after each `local` whose type is
  inferred and that isn't already `<Type>`-annotated (same inference as hover).
  Enabled by the snippet; toggle off with `:lua vim.lsp.inlay_hint.enable(false)`.
- **Folding** — `{ … }` blocks and multi-line block comments fold via the
  server (use `zc` / `za`); this is independent of the tree-sitter fold queries
  in section 2.

**`root_dir` matters now.** The project model (and therefore `T001`, hover,
goto, and project completions) only loads when the server's `root_dir` is at or
above the directory containing `Project.m1prj`. The default `root_dir` rule
already does this. With no project found, the server runs in project-less mode:
local-only hover/completion and the non-`T001` type rules still work.

**Auto-reload.** Editing `Project.m1prj` or any `*.m1cfg` triggers a
server-side reload via watched files; the project model and all diagnostics
refresh without restarting the editor.

## 2. Syntax highlighting / indent / fold (tree-sitter-m1)

This is separate from the LSP and provided by the grammar via `nvim-treesitter`.
It is an optional companion — the LSP works without it.

1. Register the `m1` parser (point it at the sibling `tree-sitter-m1` checkout):

   ```lua
   local parsers = require("nvim-treesitter.parsers").get_parser_configs()
   parsers.m1 = {
     install_info = { url = "/path/to/tree-sitter-m1", files = { "src/parser.c", "src/scanner.c" } },
     filetype = "m1scr",
   }
   vim.treesitter.language.register("m1", "m1scr")
   ```
2. `:TSInstall m1`
3. Highlight/indent/fold queries ship in `tree-sitter-m1/queries/`.

## Notes

- Example identifiers in any docs here are synthetic placeholders.
- The server speaks standard LSP over stdio, so the same binary works with VS
  Code, Helix, and Emacs eglot.
