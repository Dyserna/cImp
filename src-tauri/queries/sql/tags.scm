; V9-02 hand-authored tags for tree-sitter-sequel (SQL). Tables as definitions;
; SQL has no call graph. Other create_* nodes have irregular field shapes in
; this grammar and are left to struct-search.
(create_table
  name: (identifier) @name) @definition.class
