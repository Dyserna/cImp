; V9-02: capture @definition on the whole function_definition (body included in
; the span → calls attribute to the caller, end_line covers the body). The
; upstream tags.scm captured function_declarator only, excluding the body.
; Out-of-line methods (Foo::bar) and free functions are both function_definitions.
(function_definition
  declarator: (function_declarator
    declarator: [
      (identifier) @name
      (field_identifier) @name
      (qualified_identifier name: (identifier) @name)
    ])) @definition.function

(struct_specifier name: (type_identifier) @name body: (_)) @definition.class

(class_specifier name: (type_identifier) @name) @definition.class

(enum_specifier name: (type_identifier) @name) @definition.type

(type_definition declarator: (type_identifier) @name) @definition.type

(call_expression
  function: (identifier) @name) @reference.call

(call_expression
  function: (field_expression field: (field_identifier) @name)) @reference.call
