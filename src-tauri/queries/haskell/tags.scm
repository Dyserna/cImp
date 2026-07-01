; V9-02 vendored tags for tree-sitter-haskell (no upstream tags.scm bundled).
; Conservative best-effort: top-level function bindings and type declarations.
; If a field/node name drifts in a future grammar version, the whole query
; fails to compile and the engine disables Haskell symbol extraction (graceful;
; struct-search still works). Expand once verified against a fixture.

(function
  name: (variable) @name) @definition.function

(data_type
  name: (name) @name) @definition.type

(newtype
  name: (name) @name) @definition.type
