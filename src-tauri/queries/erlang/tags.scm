; V9-02 hand-authored tags for tree-sitter-erlang (no upstream tags.scm).
; fun_decl wraps all clauses of a function; the engine dedups the per-clause
; captures to one symbol by (name, start line).
(fun_decl
  (function_clause name: (atom) @name)) @definition.function

(call
  expr: (atom) @name) @reference.call
