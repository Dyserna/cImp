; V9-02 hand-authored tags for tree-sitter-ada (from the grammar's highlights
; node names). Ada has no simple call node we attribute reliably; defs only.
(procedure_specification
  name: (_) @name) @definition.function

(function_specification
  name: (_) @name) @definition.function

(package_declaration
  name: (_) @name) @definition.module
