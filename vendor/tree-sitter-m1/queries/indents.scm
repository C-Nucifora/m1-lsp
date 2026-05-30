; indents.scm — indentation hints for M1 (.m1scr)
; Consumed by Neovim's nvim-treesitter indent module.

[
  (block)
  (argument_list)
  (parenthesized_expression)
] @indent.begin

[ "}" ")" ] @indent.branch

[ "}" ")" ] @indent.end
