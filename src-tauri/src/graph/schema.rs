//! CozoDB relation definitions for the code knowledge graph. These mirror the
//! [`super::model`] IR 1:1. Relations are created idempotently at index open
//! (only the missing ones), so an existing `graph.db` is reused as-is.

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
            start_line: Int, end_line: Int, signature: String, doc: String?}",
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
