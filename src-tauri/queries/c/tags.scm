; V9-02: capture @definition.function on the whole function_definition (so the
; body is in the span → calls inside it attribute to the caller, and end_line
; covers the body). The upstream tags.scm captured the function_declarator only
; (name+params), excluding the body. Prototypes (declarations without a body)
; are intentionally not captured.
(function_definition
  declarator: (function_declarator
    declarator: (identifier) @name)) @definition.function

(struct_specifier name: (type_identifier) @name body: (_)) @definition.class

(declaration type: (union_specifier name: (type_identifier) @name)) @definition.class

(type_definition declarator: (type_identifier) @name) @definition.type

(enum_specifier name: (type_identifier) @name) @definition.type

(call_expression
  function: (identifier) @name) @reference.call
