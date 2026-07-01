; V9-02 vendored tags for tree-sitter-bash (no upstream tags.scm bundled).
; Function definitions and command invocations (treated as call references).

(function_definition
  name: (word) @name) @definition.function

(command
  name: (command_name) @name) @reference.call
