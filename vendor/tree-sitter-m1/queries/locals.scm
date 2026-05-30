; locals.scm — scope-aware highlighting for M1 (.m1scr)
; Consumed by Neovim's nvim-treesitter locals module. CST-only: channel and
; parameter resolution that needs the .m1prj symbol model lives in m1-core.
; (All identifiers referenced in comments are synthetic placeholders.)

; Scopes
(source_file) @local.scope
(block) @local.scope

; Definitions
(local_declaration
  name: (identifier) @local.definition.var)

(expand_statement
  variable: (identifier) @local.definition.var)

; References — every other identifier; the locals engine resolves each against
; the nearest enclosing definition, falling back to "unbound" (a channel/param).
(identifier) @local.reference
