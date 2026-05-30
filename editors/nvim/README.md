# m1-lsp in Neovim

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
