; GDScript highlight rules for tree-sitter-gdscript.
; Kept intentionally conservative so parser/query drift degrades gracefully.

(comment) @comment

[
  (string)
  (string_name)
] @string

[
  (integer)
  (float)
] @number

[
  "class"
  "class_name"
  "extends"
  "func"
  "signal"
  "var"
  "const"
  "enum"
  "if"
  "elif"
  "else"
  "for"
  "while"
  "match"
  "break"
  "continue"
  "pass"
  "return"
  "in"
  "as"
  "is"
  "await"
] @keyword

[
  (remote_keyword)
  (static_keyword)
] @keyword

[
  (true)
  (false)
  (null)
] @constant

[
  "+"
  "-"
  "*"
  "/"
  "%"
  "**"
  "="
  "+="
  "-="
  "*="
  "/="
  "%="
  "=="
  "!="
  "<"
  "<="
  ">"
  ">="
  "and"
  "or"
  "not"
] @operator

[
  "("
  ")"
  "["
  "]"
  "{"
  "}"
] @punctuation.bracket

[
  "."
  ","
  ":"
] @punctuation.delimiter

(identifier) @variable

(type (identifier) @type)

(call (identifier) @function)
(attribute_call (identifier) @function)
(function_definition name: (name) @function)
(constructor_definition "_init" @function)
(class_definition name: (name) @type)
(class_name_statement name: (name) @type)
(signal_statement name: (name) @function)
