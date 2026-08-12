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
/// V24 == 5: the `usage_stat` memory relation gains an `origin` column (S/A
/// attribution). Unlike the derived `RELATIONS`, `usage_stat` survives
/// `reset()`, so the bump *triggers* the open path but the actual column add is
/// a bespoke recreate-and-copy (`GraphIndex::migrate_usage_stat_origin`) that
/// defaults existing rows to `"session"` — no usage data is lost.
/// V32 == 6: the `mem_note` memory relation gains a `tainted` column (Phase C2
/// memory quarantine). Same shape as the V24 bump and for the same two reasons:
/// (a) `mem_note` survives `reset()`, so the column is added by a bespoke
/// stage-and-swap (`GraphIndex::migrate_mem_note_tainted`) defaulting existing
/// rows to `false`; (b) the version is what makes the migration *run at all* —
/// `GraphIndex::open` only migrates inside the version-mismatch branch — and
/// what protects the read-only `open_existing` consumers, which reject a stale
/// store rather than query a `mem_note` that has no `tainted` column yet.
/// The cost is one derived-relation rebuild per project, which is the accepted
/// price of this discipline (every `RELATIONS` row is re-derivable from source).
/// V32 (#48, F-24) == 7: `mem_note` gains a `quarantine` column carrying **why**
/// a held note is held, for the human who must promote or discard it. Everything
/// the V32 == 6 note says applies unchanged, including the part that is the
/// reason this bump exists at all: `GraphIndex::open` only migrates inside the
/// version-mismatch branch, so without the bump a store already stamped 6 would
/// keep a `mem_note` with no `quarantine` column while every note query names
/// one. The column add is
/// `GraphIndex::migrate_mem_note_quarantine`; existing rows default to *no
/// record*, which the Memory view shows as "Reason not recorded" rather than
/// inventing a cause it cannot know.
pub const GRAPH_SCHEMA_VERSION: i64 = 7;

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
