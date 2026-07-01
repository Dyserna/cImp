; V9-02 vendored tags for tree-sitter-swift, trimmed to patterns whose
; @definition capture sits on the actual construct node (not the enclosing
; class), so the generic engine derives correct spans/containment. The bare
; function_declaration pattern also matches methods inside class/protocol
; bodies (classified as functions), which is sufficient for navigation.

(class_declaration
  name: (type_identifier) @name) @definition.class

(protocol_declaration
  name: (type_identifier) @name) @definition.interface

(function_declaration
  name: (simple_identifier) @name) @definition.function

(property_declaration
  (pattern (simple_identifier) @name)) @definition.property
