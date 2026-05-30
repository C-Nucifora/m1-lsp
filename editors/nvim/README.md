# m1-lsp in Neovim

Two independent layers. You can use either or both.

## 1. Diagnostics + formatting (m1-lsp)

1. Build the server: `cargo build --release` (or `cargo install --path .`).
   Ensure `m1-lsp` is on your `$PATH`.
2. Load `m1.lua` from your config: `require("m1")` (after putting `m1.lua` on
   your `runtimepath`), or paste its contents into `init.lua`.
3. Open a `.m1scr` file. Diagnostics appear as you type; format with
   `:lua vim.lsp.buf.format()` or your usual mapping.

## 2. Syntax highlighting / indent / fold (tree-sitter-m1)

This is separate from the LSP and provided by the grammar via `nvim-treesitter`.

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
