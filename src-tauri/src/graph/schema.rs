//! CozoDB relation definitions for the code knowledge graph. These mirror the
//! [`super::model`] IR 1:1. Relations are created idempotently at index open
//! (only the missing ones), so an existing `graph.db` is reused as-is.

/// Schema generation of the derived-from-source relations. Bumped whenever a
/// [`RELATIONS`] column or relation changes shape (CozoDB has no cheap `ALTER`).
/// On open, a `graph.db` stamped with an older version is `reset()` and fully
/// rebuilt from source — cheap, since every row is re-derivable. V9 == 1;
/// V10 adds `symbol.visibility` and the memory relations.
pub const GRAPH_SCHEMA_VERSION: i64 = 2;

/// `(name, create-script)` for every stored relation. Order matters only in
/// that all are ensured before any write.
pub const RELATIONS: &[(&str, &str)] = &[
    (
        "file",
        ":create file {path: String => lang: String, hash: String}",
    ),
    (
        "symbol",
        ":create symbol {id: String => \
            name: String, kind: String, file: String, \
            start_line: Int, end_line: Int, signature: String, doc: String?, \
            visibility: String}",
    ),
    (
        "ref",
        ":create ref {file: String, line: Int, col: Int, name: String => resolved_id: String?}",
    ),
    (
        "edge",
        ":create edge {kind: String, src: String, dst: String}",
    ),
    (
        "doc_chunk",
        ":create doc_chunk {id: String => source_path: String, anchor: String, text: String}",
    ),
];
