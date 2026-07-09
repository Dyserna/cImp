//! CozoDB relation definitions for the code knowledge graph. These mirror the
//! [`super::model`] IR 1:1. Relations are created idempotently at index open
//! (only the missing ones), so an existing `graph.db` is reused as-is.

/// Schema generation of the derived-from-source relations. Bumped whenever a
/// [`RELATIONS`] column or relation changes shape (CozoDB has no cheap `ALTER`).
/// On open, a `graph.db` stamped with an older version is `reset()` and fully
/// rebuilt from source — cheap, since every row is re-derivable. V9 == 1;
/// V10 adds `symbol.visibility` and the memory relations.
/// V11–V14 == 3: `symbol.is_test` (the sole column change) forces one rebuild;
/// every other new relation in this roadmap (`injected`, `digest`,
/// `code_chunk`/`code_vec`, `commit_touch`, `project_fact`, `session_distilled`,
/// `meta`, `usage_stat`) is additive create-if-missing and needs no bump. The
/// bump is front-loaded here so the whole V11→V14 roadmap costs users a
/// single rebuild.
/// V15 == 4: `ref.confidence` and `edge.confidence` (edge-confidence layer,
/// Feature 3) add a value column to two relations, forcing one rebuild — every
/// row is re-derivable from source, so the reset-migration repopulates it.
pub const GRAPH_SCHEMA_VERSION: i64 = 4;

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
            visibility: String, is_test: Bool}",
    ),
    (
        "ref",
        ":create ref {file: String, line: Int, col: Int, name: String => \
            resolved_id: String?, confidence: String default 'inferred'}",
    ),
    (
        "edge",
        ":create edge {kind: String, src: String, dst: String => \
            confidence: String default 'inferred'}",
    ),
    (
        "doc_chunk",
        ":create doc_chunk {id: String => source_path: String, anchor: String, text: String}",
    ),
    (
        "code_chunk",
        ":create code_chunk {id: String => file: String, text: String}",
    ),
];
