; highlights.scm — syntax highlighting for M1 (.m1scr)
; (All identifiers referenced in comments are synthetic placeholders.)

; Keywords
[
  "local"
  "static"
] @keyword

[
  "if"
  "else"
  "when"
  "is"
] @keyword.conditional

[
  "expand"
  "to"
] @keyword.repeat

[
  "and"
  "or"
  "not"
  "eq"
  "neq"
] @keyword.operator

(boolean) @boolean

; Operators (note: `<` and `>` live here only; the angle brackets of a
; type_annotation are captured separately below as punctuation.bracket)
[
  "="
  "+="
  "-="
  "*="
  "/="
  "+"
  "-"
  "*"
  "/"
  "%"
  "<"
  ">"
  "<="
  ">="
  "=="
  "!="
  "&&"
  "||"
  "&"
  "|"
  "^"
  "<<"
  ">>"
  "!"
  "?"
  ":"
] @operator

; Punctuation
[ "(" ")" "{" "}" ] @punctuation.bracket
[ "." "," ";" ] @punctuation.delimiter

; Literals
(number) @number
(string) @string
(line_comment) @comment
(block_comment) @comment

; Compile-time interpolation: $(VAR)
(interpolation) @constant.macro

; Type annotation: local <Unsigned Integer> ...
; The angle brackets here are punctuation, not relational operators.
(type_annotation
  "<" @punctuation.bracket
  (identifier) @type
  ">" @punctuation.bracket)

; The expand loop variable is a parameter-like binding.
(expand_statement
  variable: (identifier) @variable.parameter)

; Calls: highlight the final property of the callee as a method, or the bare
; callee as a function.
(call_expression
  function: (member_expression property: (identifier) @function.method.call))
(call_expression
  function: (identifier) @function.call)

; A property after a `.` (channels, enum members, fields)
(member_expression property: (identifier) @property)

; Plain identifiers (channels/parameters/locals) — lowest priority catch-all.
(identifier) @variable
