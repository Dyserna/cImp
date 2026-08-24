//! The embedded CozoDB store for one project's graph. `GraphIndex` owns a
//! SQLite-backed `DbInstance` at `<root>/<db_subdir>/graph.db`, ensures the
//! schema, writes [`FileGraph`]s (delete-then-insert per file so a re-index is
//! idempotent and isolated), and answers the first queries.
//!
//! The query API broadens in Phase B; this stage proves the round trip
//! (parse → store → `find_symbol`).

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};

use cozo::{DataValue, DbInstance, MultiTransaction, Num, ScriptMutability};

use crate::error::{AppError, AppResult};

use super::model::{Confidence, EdgeKind, FileGraph, Lang};
use super::schema::{GRAPH_SCHEMA_VERSION, RELATIONS};

/// V15 Feature 2's architecture overview (V42 R13). One 300-line topology pass
/// with one entry point, moved out whole; the report types it returns stay
/// below with the rest of this store's row types.
mod arch;
/// V10 session/action memory (V42 R13): the event log, the working set, the
/// advisor signals, project facts, git provenance, and the two eviction
/// cascades every session-keyed relation has to appear in. Ensured outside
/// `RELATIONS`, so a rebuild never wipes it.
mod memory;
/// V32 Phase C2 / #47: the session-notes relation, its migration and the ONE
/// quarantine filter. A submodule rather than another 200 lines of this file
/// because locked decision 10's read exclusion holds only while a single query
/// applies the filter, and "the second note query in an 8,000-line file is the
/// bug" is not a property review can hold. See its module docs for why the
/// boundary is encapsulation and what still backstops the residue.
mod notes;
/// V14 Phase C/D's token/cost accounting ring and its read boundary (V42 R13).
/// Its own module partly for size and partly because `harness::layering`'s
/// literal exemption for cImp's `"tool_result"` discriminator covers a whole
/// file — and this one is 1,500 lines rather than 9,000.
mod usage;
/// V11 Phase G's two semantic-search vector stores plus the Phase F digest
/// cache (V42 R13). The lifecycle half of the doc/code pair used to be two
/// hand-copied sets of the same bodies; it is one set now, parameterised by the
/// three relation names that actually differ.
mod vectors;
/// V15 Feature 4's Graph View queries (V42 R13). Five methods over one shared
/// file-level rollup, with the four caps that keep the view off a hairball; the
/// row types they return stay below, where `graph::service` names them.
mod viz;

/// One symbol returned by a lookup query.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SymbolHit {
    pub id: String,
    pub name: String,
    pub kind: String,
    pub file: String,
    pub start_line: u32,
    pub signature: String,
    /// Stored visibility tag (`public`/`private`/`crate`/`unknown`).
    pub visibility: String,
    /// Last source line of the definition's span. Projected as the 8th column
    /// (after `visibility`); queries that don't select it fall back to
    /// `start_line` (a single-line span). Feeds `graph_snippet` (V11 Phase A).
    pub end_line: u32,
    /// Whether this definition IS a test. Projected as the 9th column (after
    /// `end_line`); queries that don't select it default to `false` — same
    /// honest-default posture as `visibility`'s `"unknown"`. Feeds
    /// `GraphIndex::tests_for` / `graph_tests_for` (V12 Phase C).
    pub is_test: bool,
    /// V15 Feature 3: how certain the *edge* that surfaced this symbol is —
    /// `Some` on callers/callees rows (the effective confidence of the call
    /// edge, downgraded to `Ambiguous` when the queried name is multi-candidate),
    /// `None` for plain definition lookups where no edge is involved.
    pub confidence: Option<Confidence>,
}

/// V12 Phase B: one symbol found to (transitively) depend on a changed root
/// — it calls the root, directly or through a chain of other callers.
/// Returned by [`GraphIndex::dependents_transitive`], the engine behind
/// `graph_impact`'s blast-radius output.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DependentHit {
    pub symbol: SymbolHit,
    /// Hops from the nearest root (1 = a direct caller of a root).
    pub depth: u32,
    /// Name-only (unresolved) call-edge derivation — same honesty flag
    /// `graph_references` uses. Always `true`: the graph's call edges are
    /// name-keyed (`dst` is a callee NAME, not a resolved symbol id), so
    /// every dependents hit is approximate by construction.
    pub approx: bool,
    /// V15 Feature 3: the weakest edge confidence along this dependent's
    /// discovery chain (a chain is only as certain as its least-certain link).
    /// `Ambiguous` if any hop's callee name was multi-candidate.
    pub confidence: Confidence,
}

/// One node on a traced path (V15 Feature 1). `edge_to_next`/`confidence`
/// describe the edge leaving this node toward the next; they are `None` on the
/// final node.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PathNode {
    /// Internal node id (a symbol id, or `file:<path>` for a file node).
    pub id: String,
    /// Display label — the symbol name, or the file path for a file node.
    pub label: String,
    /// The file this node lives in (for click-through). Same as `label` for
    /// file nodes.
    pub file: String,
    /// 1-based definition line, or 0 for a file node.
    pub line: u32,
    /// Symbol kind tag, or `"file"` for a file node.
    pub kind: String,
    /// The edge kind leaving this node toward the next (`call`/`import`/
    /// `contains`), or `None` on the last node.
    pub edge_to_next: Option<String>,
    /// Confidence of `edge_to_next`, or `None` on the last node.
    pub confidence: Option<Confidence>,
}

/// One hub in the architecture overview (V15 Feature 2) — a highest-degree
/// symbol or file that much of the system flows through.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GodNode {
    pub id: String,
    pub label: String,
    pub file: String,
    /// Symbol kind tag, or `"file"`.
    pub kind: String,
    /// Combined inbound degree (call count for a symbol, call-centrality for a file).
    pub degree: u64,
}

/// One subsystem/community in the architecture overview (V15 Feature 2).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Subsystem {
    /// Derived name — the common path prefix of member files, else the hub's stem.
    pub name: String,
    /// Total member file count.
    pub size: usize,
    /// A sample of member files (bounded).
    pub files: Vec<String>,
    /// The most call-central member file.
    pub hub: String,
}

/// An edge crossing subsystem boundaries (V15 Feature 2) — candidate accidental
/// coupling between otherwise-separate parts of the system.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SurprisingEdge {
    pub from: String,
    pub to: String,
    /// Edge kind tag (`call`/`import`).
    pub kind: String,
    pub from_subsystem: String,
    pub to_subsystem: String,
}

/// The architecture overview (V15 Feature 2): god nodes, subsystems, and
/// surprising cross-subsystem edges. Topology only — no LLM, no embeddings.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ArchReport {
    pub god_nodes: Vec<GodNode>,
    pub subsystems: Vec<Subsystem>,
    pub surprising: Vec<SurprisingEdge>,
}

/// One node in the Graph View snapshot (V15 Feature 4).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VizNode {
    pub id: String,
    pub label: String,
    pub file: String,
    /// Symbol kind tag, or `"file"`.
    pub kind: String,
    /// Incident-edge count (node size in the view).
    pub degree: u64,
    /// Derived subsystem name (node color), or empty when uncommunitied.
    pub subsystem: String,
}

/// One edge in the Graph View snapshot (V15 Feature 4).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VizEdge {
    pub src: String,
    pub dst: String,
    /// Edge kind tag (`call`/`import`/`contains`) — edge color.
    pub kind: String,
    /// Confidence tag (`extracted`/`inferred`/`ambiguous`) — edge dash pattern.
    pub confidence: String,
    /// Whether the edge made the per-node drawn quota. `false` edges are NOT
    /// rendered as ambient lines — they exist so the frontend can list and
    /// highlight a selected node's FULL connection set.
    pub drawn: bool,
}

/// A bounded subgraph for the Graph View tab (V15 Feature 4): the top-degree
/// hubs and the edges among them, never the whole graph.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct VizGraph {
    pub nodes: Vec<VizNode>,
    pub edges: Vec<VizEdge>,
}

/// Per-file Graph View presence (drives the Workbench jump button's state).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VizFileStatus {
    pub path: String,
    /// The file exists in the graph index at all.
    pub indexed: bool,
    /// Rolled-up file-level call/import degree. 0 means the file never
    /// appears in the viz snapshot (degree-0 nodes are filtered out before
    /// the top-N cut), so there is nothing to jump to.
    pub degree: u64,
}

/// A traced shortest path between two entities (V15 Feature 1).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PathHit {
    /// The ordered chain from source to target (`nodes[0]` = source).
    pub nodes: Vec<PathNode>,
    /// Number of hops = `nodes.len() - 1`.
    pub hops: usize,
    /// How many *other* shortest paths of the same length exist (0 = unique).
    pub equal_alternatives: u64,
}

/// One reference (use site) returned by `references`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RefHit {
    pub name: String,
    pub file: String,
    pub line: u32,
    pub col: u32,
    /// V15 Feature 3: the reference's confidence — its stored parse-time value
    /// (`Extracted` if defined same-file, else `Inferred`), downgraded to
    /// `Ambiguous` when the name resolves to more than one definition.
    pub confidence: Confidence,
}

/// One documentation hit returned by `search_docs`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DocHit {
    pub source_path: String,
    pub anchor: String,
    pub snippet: String,
}

/// Indexed-file count for one language, for the status surface's per-language
/// breakdown. `lang` is the stored language tag (e.g. `"rust"`, `"markdown"`).
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize)]
pub struct LangCount {
    pub lang: String,
    pub files: u64,
}

/// Per-index counts for the status surface.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct GraphStats {
    pub files: u64,
    pub symbols: u64,
    pub edges: u64,
    /// Indexed files grouped by language, biggest first (zero-file languages
    /// are absent — a language only appears once it has at least one file).
    pub by_lang: Vec<LangCount>,
}

pub struct GraphIndex {
    db: DbInstance,
    /// Set true when [`Self::open`] had to `reset()` a stale-schema store. The
    /// service reads it **once** (via [`Self::take_schema_reset`]) to trigger a
    /// repopulating rebuild, so a migrated (emptied) store never lingers empty.
    schema_reset: AtomicBool,
}

/// One relation's crash-safe **stage-and-swap** migration, as data — the
/// arguments [`GraphIndex::stage_and_swap`] needs that differ between the two
/// relations that run one (`usage_stat`'s V24 `origin`, `mem_note`'s V32
/// `tainted`/`quarantine`).
///
/// A struct rather than nine positional parameters because six of the nine are
/// `&str` and a transposed pair would compile: `live`/`stage` swapped promotes
/// the live relation over an empty stage, which is the one direction that loses
/// rows.
struct StageAndSwap<'a> {
    /// The live relation being migrated, named for the idempotence probe, the
    /// "nothing to migrate" check and the abort message.
    live: &'a str,
    /// The staging relation. Its presence on open means "a prior migration was
    /// interrupted after the stage was durably populated — adopt me".
    stage: &'a str,
    /// The column the migration adds. Present on `live` ⇒ already migrated.
    added_column: &'a str,
    /// Reads every row in the OLD shape. Historical by definition, so it is
    /// spelled out by the caller rather than derived from the current DDL.
    read_script: &'a str,
    /// Appended to each old row, in the current shape's column order.
    defaults: &'a [DataValue],
    /// The CURRENT shape's column list, for the stage's `?[…] <- $rows` head.
    stage_columns: &'a str,
    /// The stage's `:create` DDL, already rendered for [`Self::stage`]. Shared
    /// with the live relation's own definition at every call site, so the two
    /// shapes cannot drift.
    stage_ddl: &'a str,
    /// Counts the rows the stage captured. A closure because the caller may
    /// need the relation named as a literal rather than interpolated — which is
    /// exactly `mem_note`'s constraint (`graph/index/notes.rs`'s house rule).
    count_stage: &'a dyn Fn() -> AppResult<usize>,
    /// Promotes a fully-populated stage over `live`. A closure for
    /// [`Self::count_stage`]'s reason: the two statements name the relation.
    promote: &'a dyn Fn() -> AppResult<()>,
}

impl GraphIndex {
    /// Hard cap on the number of nodes [`Self::transitive`] returns. Exposed so
    /// callers can detect when a result hit the cap (len == `TRANSITIVE_LIMIT`)
    /// and is therefore truncated, rather than presenting it as the exact reach.
    pub const TRANSITIVE_LIMIT: usize = 1000;

    /// Open the SQLite-backed store for `root` (creating the dir/db if needed)
    /// without touching the schema.
    fn open_db(root: &Path, db_subdir: &str) -> AppResult<GraphIndex> {
        let dir = root.join(db_subdir);
        std::fs::create_dir_all(&dir).map_err(AppError::Io)?;
        let db_path = dir.join("graph.db");
        let db = DbInstance::new(
            "sqlite",
            db_path.to_string_lossy().as_ref(),
            Default::default(),
        )
        .map_err(|e| AppError::Graph(format!("open {}: {e}", db_path.display())))?;
        Ok(GraphIndex {
            db,
            schema_reset: AtomicBool::new(false),
        })
    }

    /// Open (creating if needed) the graph store for `root`, ensuring the schema.
    /// This is the **writable / service** path: a store stamped with an older
    /// [`GRAPH_SCHEMA_VERSION`] is `reset()` to the current shape (CozoDB has no
    /// cheap `ALTER`, and every derived row is re-buildable from source) and
    /// flagged so the service re-indexes it. `db_subdir` is the per-project home
    /// (default `.cimp`).
    pub fn open(root: &Path, db_subdir: &str) -> AppResult<GraphIndex> {
        let index = Self::open_db(root, db_subdir)?;
        index.ensure_schema()?;
        index.ensure_memory_relations()?;
        index.ensure_schema_meta()?;
        if index.stored_schema_version()? != Some(GRAPH_SCHEMA_VERSION) {
            // A pre-V10 store has no `schema_meta` at all, so its version reads as
            // `None` — same as a brand-new empty store. Distinguish them by data:
            // only a *populated* store that predates the schema needs a rebuild
            // after the reset (flag it); a fresh/empty store is filled by the
            // normal enable/index flow, so a stray touch of it isn't auto-indexed.
            let had_data = index.count_files()? > 0;
            // `usage_stat` (a memory relation) survives `reset()`, so its V24
            // `origin` column is added by a bespoke recreate-and-copy rather than
            // the derived-relation reset — no usage data is lost across the bump.
            index.migrate_usage_stat_origin()?;
            // Same story for `mem_note`'s V32 Phase C2 `tainted` column: memory
            // relations survive `reset()`, so the column is added by a
            // stage-and-swap copy. Ordering against the reset is irrelevant
            // (neither migration touches a derived relation), but both must run
            // BEFORE `write_schema_version` — a crash between them and the stamp
            // simply re-runs a no-op migration on the next open, whereas
            // stamping first would strand an un-migrated relation at the current
            // version, where nothing would ever migrate it again.
            index.migrate_mem_note_tainted()?;
            // #48, F-24: and again for the `quarantine` column. TWO migrations
            // rather than one because they start from different shapes and the
            // first cannot reach the second's stores — `migrate_mem_note_tainted`
            // returns early the moment `tainted` is present, which is every store
            // this milestone has already shipped to. Order matters only in the
            // cheap direction: a pre-C2 store is brought fully current by the
            // first, and the second then finds its column already there and is a
            // no-op.
            index.migrate_mem_note_quarantine()?;
            index.reset()?;
            index.write_schema_version(GRAPH_SCHEMA_VERSION)?;
            if had_data {
                index.schema_reset.store(true, Ordering::Relaxed);
            }
        }
        index.prune_retention_on_open();
        Ok(index)
    }

    /// Open an **existing** graph store read-only, erroring if it hasn't been
    /// built yet OR if its schema predates [`GRAPH_SCHEMA_VERSION`]. Used by
    /// read-only consumers (the MCP child, the offload worker) that must never
    /// create-or-wipe a store: a stale store is left intact and surfaced as
    /// [`AppError::GraphNotReady`] so the app (which owns the rebuild) can
    /// migrate it, instead of silently serving an emptied/mis-shaped index.
    pub fn open_existing(root: &Path, db_subdir: &str) -> AppResult<GraphIndex> {
        let db_path = root.join(db_subdir).join("graph.db");
        if !db_path.exists() {
            return Err(AppError::GraphNotReady(format!(
                "no code graph at {} — enable the graph and index this project in cImp",
                db_path.display()
            )));
        }
        let index = Self::open_db(root, db_subdir)?;
        // Create-if-missing only (never resets); memory relations are additive.
        index.ensure_schema()?;
        index.ensure_memory_relations()?;
        index.ensure_schema_meta()?;
        if index.stored_schema_version()? != Some(GRAPH_SCHEMA_VERSION) {
            return Err(AppError::GraphNotReady(format!(
                "the code graph for {} is from an older cImp version — open the project in cImp to rebuild it",
                root.display()
            )));
        }
        Ok(index)
    }

    /// Apply the [`SESSION_RETENTION_DAYS`] sweep once, at the tail of
    /// [`Self::open`] — the app's warm path — and **deliberately NOT from
    /// [`Self::open_existing`]**. Do not "fix" that asymmetry: the handle
    /// discipline in `docs/MAINTENANCE.md` requires every `open_existing`
    /// consumer (the `--offload-mcp` child, `tabs/config.rs`, the audit paths)
    /// to stay strictly READ-ONLY on the store. Sweeping from there would make
    /// each of them a second cross-process WRITER against a live main app,
    /// which is the lock-contention/corruption hazard that discipline exists to
    /// prevent — a cost far above the value of expiring rows a few hours
    /// earlier. Coverage is unaffected in practice: the app reaches every store
    /// it serves through `GraphService::index_for` → `open`, so a root that
    /// only read-only consumers ever touch legitimately never expires.
    ///
    /// Placed AFTER the schema-version gate so a stale store is never written
    /// to. Never fatal: a failed sweep is logged and the open proceeds —
    /// retention is hygiene, not a correctness precondition.
    fn prune_retention_on_open(&self) {
        let now_ms = crate::activity::now_ms() as i64;
        match self.prune_expired_sessions(now_ms) {
            Ok(0) => {}
            Ok(n) => tracing::debug!(sessions = n, "graph: retention sweep dropped idle sessions"),
            Err(e) => tracing::debug!(error = %e, "graph: retention sweep failed"),
        }
    }

    /// Drop sessions idle longer than [`SESSION_RETENTION_DAYS`] relative to
    /// `now_ms`, in ONE bounded write transaction (a handful of `:rm`s per
    /// expired session, and none at all when nothing expired). `now_ms` is a
    /// parameter rather than read here so the boundary is directly testable.
    /// Returns the number of sessions removed.
    pub fn prune_expired_sessions(&self, now_ms: i64) -> AppResult<usize> {
        self.with_write_txn(|tx| memory::prune_expired_sessions_in_tx(tx, now_ms))
    }

    /// Take (and clear) the "stale schema was reset on open" flag. Returns `true`
    /// exactly once after a migrating open, so the service triggers one rebuild.
    pub fn take_schema_reset(&self) -> bool {
        self.schema_reset.swap(false, Ordering::Relaxed)
    }

    /// Number of indexed files — used to tell a populated (needs-rebuild-after-
    /// migration) store from a fresh/empty one. The `file` relation's shape is
    /// stable across schema versions, so this is safe to read on a stale store.
    fn count_files(&self) -> AppResult<u64> {
        let rows = self.run(
            "?[count(path)] := *file{path}",
            BTreeMap::new(),
            ScriptMutability::Immutable,
        )?;
        Ok(rows
            .rows
            .first()
            .and_then(|r| r.first())
            .map(dv_i64)
            .unwrap_or(0) as u64)
    }

    /// Drop every relation and recreate the schema empty. Used by a **full
    /// rebuild** so rows for files that were deleted since the last build don't
    /// linger (per-file `index_file_graph` only replaces a file's own rows, it
    /// can't know a path vanished). `::remove` of a not-yet-created relation is
    /// ignored so this is safe on a fresh store too.
    pub fn reset(&self) -> AppResult<()> {
        let existing = self.existing_relations()?;
        for (name, _) in RELATIONS {
            if existing.contains(*name) {
                self.run_mut(&format!("::remove {name}"), BTreeMap::new())?;
            }
        }
        // The lazily-created vector stores aren't in RELATIONS, so a bare reset
        // would leave `doc_vec`/`code_vec` full of orphan vectors after their
        // `doc_chunk`/`code_chunk` rows were wiped — making `embedding_coverage`
        // report embedded > total and suppress the backfill that should
        // re-embed. Drop them too (no-op when absent).
        self.clear_vectors()?;
        self.clear_code_vectors()?;
        self.ensure_schema()
    }

    fn ensure_schema(&self) -> AppResult<()> {
        let existing = self.existing_relations()?;
        for (name, create) in RELATIONS {
            if !existing.contains(*name) {
                self.run_mut(create, BTreeMap::new())?;
            }
        }
        Ok(())
    }

    fn ensure_schema_meta(&self) -> AppResult<()> {
        if !self.existing_relations()?.contains("schema_meta") {
            self.run_mut(
                "?[key, value] <- []\n:create schema_meta {key: String => value: Int}",
                BTreeMap::new(),
            )?;
        }
        Ok(())
    }

    fn stored_schema_version(&self) -> AppResult<Option<i64>> {
        if !self.existing_relations()?.contains("schema_meta") {
            return Ok(None);
        }
        let mut p = BTreeMap::new();
        p.insert("key".to_string(), DataValue::Str("schema_version".into()));
        let rows = self.run(
            "?[value] := *schema_meta{key, value}, key == $key",
            p,
            ScriptMutability::Immutable,
        )?;
        Ok(rows.rows.first().map(|r| cell_i64(r, 0)))
    }

    fn write_schema_version(&self, v: i64) -> AppResult<()> {
        let mut p = BTreeMap::new();
        p.insert("v".to_string(), DataValue::Num(Num::Int(v)));
        self.run_mut(
            "?[key, value] <- [['schema_version', $v]]\n:put schema_meta {key => value}",
            p,
        )?;
        Ok(())
    }

    fn existing_relations(&self) -> AppResult<HashSet<String>> {
        let rows = self.run("::relations", BTreeMap::new(), ScriptMutability::Immutable)?;
        // Fail loudly rather than silently reading column 0 if a future CozoDB
        // changes the `::relations` shape — guessing the wrong column would make
        // every schema-existence check misfire (re-create conflicts on startup,
        // or `reset` skipping live relations).
        let name_col = rows
            .headers
            .iter()
            .position(|h| h == "name")
            .ok_or_else(|| {
                AppError::Graph("::relations result has no 'name' column".to_string())
            })?;
        Ok(rows
            .rows
            .iter()
            .filter_map(|r| r.get(name_col))
            .map(dv_string)
            .collect())
    }

    /// Delete every row belonging to `file` (symbols, refs, doc-chunks,
    /// code-chunks, the `file` row, and edges keyed by the file-embedded id
    /// prefix `<file>#…` or `src == file` for imports). Used both by the
    /// per-file replace in [`index_file_graph`] and by the watcher when a file
    /// is deleted.
    pub fn remove_file(&self, file: &str) -> AppResult<()> {
        self.with_write_txn(|tx| {
            let removed = remove_file_in_tx(tx, file)?;
            purge_dangling_call_edges_in_tx(tx, removed)
        })
    }

    /// Delete every indexed file whose path lies under directory `dir` (i.e.
    /// `path` starts with `dir/`), returning the count removed. Used by the
    /// watcher for directory rename/delete events: such an event carries only
    /// the directory path (which has no indexable extension), so the per-file
    /// delete path never fires for its children and their rows would otherwise
    /// leak until a full rebuild.
    pub fn remove_files_under(&self, dir: &str) -> AppResult<usize> {
        let prefix = format!("{}/", dir.trim_end_matches('/'));
        self.with_write_txn(|tx| {
            let mut p = BTreeMap::new();
            p.insert("prefix".to_string(), DataValue::Str(prefix.as_str().into()));
            let rows = tx_run(tx, "?[path] := *file{path}, starts_with(path, $prefix)", p)?;
            let files: Vec<String> = rows.rows.iter().map(|r| cell_str(r, 0)).collect();
            let n = files.len();
            // Purge once after ALL files are removed: a name defined by two
            // files under `dir` would otherwise read as still-defined while
            // the sibling awaits removal, surviving as a ghost.
            let mut removed = std::collections::BTreeSet::new();
            for f in &files {
                removed.extend(remove_file_in_tx(tx, f)?);
            }
            purge_dangling_call_edges_in_tx(tx, removed)?;
            Ok(n)
        })
    }

    /// Write one file's extracted graph, replacing any prior rows for that
    /// path. Symbols/refs/doc-chunks are keyed by the file; edges are matched
    /// by the file-embedded id prefix (`<file>#…`) or, for imports, `src == file`.
    pub fn index_file_graph(&self, fg: &FileGraph) -> AppResult<()> {
        let file = fg.path.clone();

        // Build every row vector up front (pure data, no DB access) so the
        // transaction below only does writes.
        let file_rows = vec![DataValue::List(vec![
            DataValue::Str(file.as_str().into()),
            DataValue::Str(fg.lang_tag.as_str().into()),
            DataValue::Str(fg.hash.as_str().into()),
        ])];

        let symbol_rows = fg
            .symbols
            .iter()
            .map(|s| {
                DataValue::List(vec![
                    DataValue::Str(s.id.as_str().into()),
                    DataValue::Str(s.name.as_str().into()),
                    DataValue::Str(s.kind.tag().into()),
                    DataValue::Str(s.file.as_str().into()),
                    int(s.start_line),
                    int(s.end_line),
                    DataValue::Str(s.signature.as_str().into()),
                    s.doc
                        .as_deref()
                        .map(|d| DataValue::Str(d.into()))
                        .unwrap_or(DataValue::Null),
                    DataValue::Str(s.visibility.tag().into()),
                    DataValue::Bool(s.is_test),
                ])
            })
            .collect();

        let ref_rows = fg
            .references
            .iter()
            .map(|r| {
                DataValue::List(vec![
                    DataValue::Str(r.file.as_str().into()),
                    int(r.line),
                    int(r.col),
                    DataValue::Str(r.name.as_str().into()),
                    r.resolved_id
                        .as_deref()
                        .map(|d| DataValue::Str(d.into()))
                        .unwrap_or(DataValue::Null),
                    DataValue::Str(r.confidence.tag().into()),
                ])
            })
            .collect();

        let doc_rows = fg
            .docs
            .iter()
            .map(|d| {
                DataValue::List(vec![
                    DataValue::Str(d.id.as_str().into()),
                    DataValue::Str(d.source_path.as_str().into()),
                    DataValue::Str(d.anchor.as_str().into()),
                    DataValue::Str(d.text.as_str().into()),
                ])
            })
            .collect();

        let code_chunk_rows = fg
            .code_chunks
            .iter()
            .map(|c| {
                DataValue::List(vec![
                    DataValue::Str(c.id.as_str().into()),
                    DataValue::Str(c.file.as_str().into()),
                    DataValue::Str(c.text.as_str().into()),
                ])
            })
            .collect();

        let edge_rows = fg
            .edges
            .iter()
            .map(|e| {
                DataValue::List(vec![
                    DataValue::Str(e.kind.tag().into()),
                    DataValue::Str(e.src.as_str().into()),
                    DataValue::Str(e.dst.as_str().into()),
                    DataValue::Str(e.confidence.tag().into()),
                ])
            })
            .collect();

        // Replace-then-insert, all in one transaction: a mid-write failure
        // (disk full, killed process) rolls back instead of leaving the file
        // with, say, symbols but no edges.
        self.with_write_txn(move |tx| {
            let removed_names = remove_file_in_tx(tx, &file)?;
            tx_put(
                tx,
                "?[path, lang, hash] <- $rows\n:put file {path => lang, hash}",
                file_rows,
            )?;
            tx_put(
                tx,
                "?[id, name, kind, file, start_line, end_line, signature, doc, visibility, is_test] <- $rows\n\
                 :put symbol {id => name, kind, file, start_line, end_line, signature, doc, visibility, is_test}",
                symbol_rows,
            )?;
            tx_put(
                tx,
                "?[file, line, col, name, resolved_id, confidence] <- $rows\n\
                 :put ref {file, line, col, name => resolved_id, confidence}",
                ref_rows,
            )?;
            tx_put(
                tx,
                "?[id, source_path, anchor, text] <- $rows\n:put doc_chunk {id => source_path, anchor, text}",
                doc_rows,
            )?;
            tx_put(
                tx,
                "?[id, file, text] <- $rows\n:put code_chunk {id => file, text}",
                code_chunk_rows,
            )?;
            tx_put(
                tx,
                "?[kind, src, dst, confidence] <- $rows\n:put edge {kind, src, dst => confidence}",
                edge_rows,
            )?;
            // AFTER the re-insert: names the new file version still defines
            // read as defined and keep their inbound call edges from other
            // files; only names this edit genuinely removed are purged.
            purge_dangling_call_edges_in_tx(tx, removed_names)?;
            Ok(())
        })
    }

    /// Run `f` inside a single write transaction: commit on `Ok`, abort on
    /// `Err`. Lets a multi-step mutation (the per-file replace) be all-or-nothing
    /// rather than a sequence of independently-committed scripts that can leave a
    /// half-written file graph behind.
    fn with_write_txn<T>(&self, f: impl FnOnce(&MultiTransaction) -> AppResult<T>) -> AppResult<T> {
        let tx = self.db.multi_transaction(true);
        match f(&tx) {
            Ok(v) => {
                tx.commit()
                    .map_err(|e| AppError::Graph(format!("transaction commit failed: {e}")))?;
                Ok(v)
            }
            Err(e) => {
                let _ = tx.abort();
                Err(e)
            }
        }
    }

    /// Find definitions by exact name.
    pub fn find_symbol(&self, name: &str) -> AppResult<Vec<SymbolHit>> {
        let mut p = BTreeMap::new();
        p.insert("name".to_string(), DataValue::Str(name.into()));
        let rows = self.run(
            "?[id, name, kind, file, start_line, signature, visibility, end_line, is_test] := \
                *symbol{id, name, kind, file, start_line, signature, visibility, end_line, is_test}, name == $name\n\
             :order file, start_line, id",
            p,
            ScriptMutability::Immutable,
        )?;
        Ok(rows_to_symbols(&rows))
    }

    /// Symbols that call `name` (callers). Joins call edges (whose `dst` is the
    /// callee name) back to the caller symbol by `src` id.
    pub fn callers(&self, name: &str) -> AppResult<Vec<SymbolHit>> {
        let mut p = BTreeMap::new();
        p.insert("name".to_string(), DataValue::Str(name.into()));
        let rows = self.run(
            r#"?[sid, sname, skind, file, start_line, signature, visibility, end_line, is_test, conf] :=
                *edge{kind: ek, src: sid, dst: dn, confidence: conf}, ek == "call", dn == $name,
                *symbol{id: sid, name: sname, kind: skind, file, start_line, signature, visibility, end_line, is_test}
            :order file, start_line, sid
            :limit 500"#,
            p,
            ScriptMutability::Immutable,
        )?;
        // V15 Feature 3: if the callee name resolves to more than one definition
        // we can't know which one each caller actually targets — every hit is a
        // superset, so mark it `Ambiguous`. Otherwise carry the edge's own
        // confidence (col 9).
        let ambiguous = self.symbol_count(name)? > 1;
        Ok(rows
            .rows
            .iter()
            .map(|r| with_row_confidence(row_to_symbol(r), r, 9, ambiguous))
            .collect())
    }

    /// Symbols called by any symbol named `name` (callees, resolved by name).
    pub fn callees(&self, name: &str) -> AppResult<Vec<SymbolHit>> {
        let mut p = BTreeMap::new();
        p.insert("name".to_string(), DataValue::Str(name.into()));
        let rows = self.run(
            r#"?[id2, nm, skind, file, start_line, signature, visibility, end_line, is_test, conf] :=
                *symbol{id: cid, name: cn}, cn == $name,
                *edge{kind: ek, src: cid, dst: dn, confidence: conf}, ek == "call",
                *symbol{id: id2, name: nm, kind: skind, file, start_line, signature, visibility, end_line, is_test}, nm == dn
            :order file, start_line, id2
            :limit 500"#,
            p,
            ScriptMutability::Immutable,
        )?;
        // A callee name that resolved to >1 symbol in this result is a superset
        // (Ambiguous); one that resolved uniquely keeps the edge's confidence.
        let mut name_counts: HashMap<String, usize> = HashMap::new();
        for r in &rows.rows {
            *name_counts.entry(cell_str(r, 1)).or_default() += 1;
        }
        Ok(rows
            .rows
            .iter()
            .map(|r| {
                let amb = name_counts.get(&cell_str(r, 1)).copied().unwrap_or(0) > 1;
                with_row_confidence(row_to_symbol(r), r, 9, amb)
            })
            .collect())
    }

    /// All reference (use) sites of `name`.
    pub fn references(&self, name: &str) -> AppResult<Vec<RefHit>> {
        let mut p = BTreeMap::new();
        p.insert("name".to_string(), DataValue::Str(name.into()));
        let rows = self.run(
            "?[name, file, line, col, conf] := *ref{name, file, line, col, confidence: conf}, name == $name\n:order file, line, col\n:limit 1000",
            p,
            ScriptMutability::Immutable,
        )?;
        // Multi-candidate name → the use sites could bind to any of them.
        let ambiguous = self.symbol_count(name)? > 1;
        Ok(rows
            .rows
            .iter()
            .map(|r| RefHit {
                name: cell_str(r, 0),
                file: cell_str(r, 1),
                line: cell_i64(r, 2) as u32,
                col: cell_i64(r, 3) as u32,
                confidence: if ambiguous {
                    Confidence::Ambiguous
                } else {
                    Confidence::from_tag(&cell_str(r, 4))
                },
            })
            .collect())
    }

    /// Project-relative paths of every indexed file of `lang_tag` (e.g.
    /// `"rust"`). Used by structural search to scope which files to re-parse.
    pub fn files_for_lang(&self, lang_tag: &str) -> AppResult<Vec<String>> {
        let mut p = BTreeMap::new();
        p.insert("lang".to_string(), DataValue::Str(lang_tag.into()));
        let rows = self.run(
            "?[path] := *file{path, lang}, lang == $lang",
            p,
            ScriptMutability::Immutable,
        )?;
        Ok(rows.rows.iter().map(|r| cell_str(r, 0)).collect())
    }

    /// Module/symbol paths imported by `file`.
    pub fn imports(&self, file: &str) -> AppResult<Vec<String>> {
        let mut p = BTreeMap::new();
        p.insert("file".to_string(), DataValue::Str(file.into()));
        let rows = self.run(
            r#"?[dst] := *edge{kind: ek, src, dst}, ek == "import", src == $file
            :order dst
            :limit 500"#,
            p,
            ScriptMutability::Immutable,
        )?;
        Ok(rows.rows.iter().map(|r| cell_str(r, 0)).collect())
    }

    /// **Candidate** dead exports: public symbols with no reference use-site and
    /// no inbound call edge (by name or id), minus a small entrypoint allowlist
    /// (`main`, `test_*`, common trait/convention method names). Bounded to `max`.
    ///
    /// These are *candidates*, in two directions:
    /// - **False positives** — a symbol reached only through dynamic dispatch, an
    ///   external consumer, a macro, or reflection has no static edge and appears
    ///   here anyway.
    /// - **False negatives** — reference/call matching is by *name* (the graph is
    ///   name-keyed, references aren't fully id-resolved), so a genuinely-dead
    ///   symbol that shares its name with a used symbol elsewhere is masked. This
    ///   errs toward under-reporting on purpose (never flag live code).
    ///
    /// The caller/UI must state both caveats. Only languages that record real
    /// visibility (Rust/JS/TS/Python/Go) contribute; others are `unknown` and so
    /// never `"public"`.
    pub fn dead_exports(&self, max: usize) -> AppResult<Vec<SymbolHit>> {
        // No DB-side `:limit` — the entrypoint allowlist is applied in Rust after
        // the query, so limiting first would let conventionally-named public
        // symbols (new/default/fmt/…) consume the budget and then get stripped,
        // yielding far fewer than `max` (or zero). The candidate set (public AND
        // unreferenced) is naturally small, so an unbounded query is cheap.
        let rows = self.run(
            r#"call_dst[dst] := *edge{kind: k, src, dst}, k == "call"
?[id, name, kind, file, start_line, signature, visibility, end_line, is_test] :=
    *symbol{id, name, kind, file, start_line, signature, visibility, end_line, is_test},
    visibility == "public",
    not *ref{name: name},
    not call_dst[name],
    not call_dst[id]"#,
            BTreeMap::new(),
            ScriptMutability::Immutable,
        )?;
        let mut syms = rows_to_symbols(&rows);
        syms.retain(|s| !is_entrypoint_name(&s.name));
        syms.sort_by(|a, b| a.file.cmp(&b.file).then(a.start_line.cmp(&b.start_line)));
        syms.truncate(max);
        Ok(syms)
    }

    /// Import cycles between files (each a loop of ≥ 2 files). Import edges store
    /// a raw module string as `dst`, so this resolves each `(file, module)` to a
    /// concrete file with a best-effort per-language resolver, builds the
    /// file→file import graph, and returns its strongly-connected components of
    /// size ≥ 2 (a self-import is ignored). Modules that don't resolve to a known
    /// indexed file are dropped — languages without a resolver simply never
    /// report cycles, which is honest rather than wrong.
    pub fn import_cycles(&self, max: usize) -> AppResult<Vec<Vec<String>>> {
        // file → lang tag, so the resolver can pick per-language rules.
        let file_rows = self.run(
            "?[path, lang] := *file{path, lang}",
            BTreeMap::new(),
            ScriptMutability::Immutable,
        )?;
        let mut lang_of: std::collections::HashMap<String, String> =
            std::collections::HashMap::new();
        let mut known: HashSet<String> = HashSet::new();
        for r in &file_rows.rows {
            let path = cell_str(r, 0);
            known.insert(path.clone());
            lang_of.insert(path, cell_str(r, 1));
        }

        // Every import edge (src file → raw module string).
        let import_rows = self.run(
            r#"?[src, dst] := *edge{kind: k, src, dst}, k == "import""#,
            BTreeMap::new(),
            ScriptMutability::Immutable,
        )?;

        // Resolve each to a file→file adjacency (indices into `files`).
        let files: Vec<String> = known.iter().cloned().collect();
        let idx_of: std::collections::HashMap<&str, usize> = files
            .iter()
            .enumerate()
            .map(|(i, f)| (f.as_str(), i))
            .collect();
        let mut adj: Vec<Vec<usize>> = vec![Vec::new(); files.len()];
        for r in &import_rows.rows {
            let src = cell_str(r, 0);
            let module = cell_str(r, 1);
            let Some(&si) = idx_of.get(src.as_str()) else {
                continue;
            };
            let lang = Lang::from_tag(lang_of.get(&src).map(|s| s.as_str()).unwrap_or(""));
            if let Some(target) = resolve_import(lang, &src, &module, &known) {
                if let Some(&ti) = idx_of.get(target.as_str()) {
                    if ti != si {
                        adj[si].push(ti);
                    }
                }
            }
        }

        let mut cycles = tarjan_sccs(&adj)
            .into_iter()
            .filter(|scc| scc.len() >= 2)
            .map(|scc| {
                scc.into_iter()
                    .map(|i| files[i].clone())
                    .collect::<Vec<_>>()
            })
            .collect::<Vec<_>>();
        // Stable order (biggest cycles first, then lexicographic) and bound.
        for c in cycles.iter_mut() {
            c.sort();
        }
        cycles.sort_by(|a, b| b.len().cmp(&a.len()).then_with(|| a.cmp(b)));
        cycles.truncate(max);
        Ok(cycles)
    }

    /// Every definition in `file`, ordered by start line.
    pub fn outline(&self, file: &str) -> AppResult<Vec<SymbolHit>> {
        let mut p = BTreeMap::new();
        p.insert("file".to_string(), DataValue::Str(file.into()));
        let rows = self.run(
            "?[id, name, kind, file, start_line, signature, visibility, end_line, is_test] := \
                *symbol{id, name, kind, file, start_line, signature, visibility, end_line, is_test}, file == $file",
            p,
            ScriptMutability::Immutable,
        )?;
        let mut syms = rows_to_symbols(&rows);
        syms.sort_by_key(|s| s.start_line);
        Ok(syms)
    }

    /// How many definitions share the exact name `name`. Feeds the V15 Feature 3
    /// query-time `Ambiguous` override: a name with more than one candidate makes
    /// every name-keyed resolution of it a superset.
    pub fn symbol_count(&self, name: &str) -> AppResult<u64> {
        let mut p = BTreeMap::new();
        p.insert("name".to_string(), DataValue::Str(name.into()));
        let rows = self.run(
            "?[count(id)] := *symbol{id, name}, name == $name",
            p,
            ScriptMutability::Immutable,
        )?;
        Ok(rows
            .rows
            .first()
            .and_then(|r| r.first())
            .map(dv_i64)
            .unwrap_or(0) as u64)
    }

    /// The set of symbol names defined by more than one definition — the names
    /// whose every name-keyed resolution is `Ambiguous`. One aggregate scan,
    /// shared by the impact BFS and any other multi-hop confidence pass.
    pub fn multi_candidate_names(&self) -> AppResult<HashSet<String>> {
        let rows = self.run(
            "?[name, count(id)] := *symbol{id, name}",
            BTreeMap::new(),
            ScriptMutability::Immutable,
        )?;
        Ok(rows
            .rows
            .iter()
            .filter(|r| cell_i64(r, 1) > 1)
            .map(|r| cell_str(r, 0))
            .collect())
    }

    /// The smallest symbol whose span encloses `line` (1-based) in `file`. Used
    /// by `graph_snippet`'s `file`+`line` mode. `None` when no indexed symbol
    /// contains the line (e.g. a top-of-file import region or a blank line).
    pub fn symbol_at(&self, file: &str, line: u32) -> AppResult<Option<SymbolHit>> {
        let mut p = BTreeMap::new();
        p.insert("file".to_string(), DataValue::Str(file.into()));
        p.insert("line".to_string(), DataValue::Num(Num::Int(line as i64)));
        let rows = self.run(
            "?[id, name, kind, file, start_line, signature, visibility, end_line] := \
                *symbol{id, name, kind, file, start_line, signature, visibility, end_line}, \
                file == $file, start_line <= $line, end_line >= $line",
            p,
            ScriptMutability::Immutable,
        )?;
        let mut syms = rows_to_symbols(&rows);
        // Smallest enclosing span wins — the most specific definition (a method
        // inside an impl inside a module resolves to the method).
        syms.sort_by_key(|s| s.end_line.saturating_sub(s.start_line));
        Ok(syms.into_iter().next())
    }

    /// Number of distinct callers of a symbol name — a cheap orientation figure
    /// for the `graph_snippet` header. The `edge` relation stores `(kind, src,
    /// dst)` as a set, so with `kind`/`dst` fixed this counts distinct caller
    /// ids. Name-keyed like the other call queries (approximate by convention).
    pub fn callers_count(&self, name: &str) -> AppResult<u64> {
        let mut p = BTreeMap::new();
        p.insert("name".to_string(), DataValue::Str(name.into()));
        let rows = self.run(
            r#"?[count(src)] := *edge{kind: k, src, dst}, k == "call", dst == $name"#,
            p,
            ScriptMutability::Immutable,
        )?;
        Ok(rows
            .rows
            .first()
            .and_then(|r| r.first())
            .map(dv_i64)
            .unwrap_or(0) as u64)
    }

    /// The stored content hash for an indexed file, or `None` if not indexed.
    /// `graph_snippet` compares it against the on-disk content hash to flag a
    /// span as possibly stale (file edited since the last index pass).
    pub fn stored_file_hash(&self, file: &str) -> AppResult<Option<String>> {
        let mut p = BTreeMap::new();
        p.insert("file".to_string(), DataValue::Str(file.into()));
        let rows = self.run(
            "?[hash] := *file{path, hash}, path == $file",
            p,
            ScriptMutability::Immutable,
        )?;
        Ok(rows.rows.first().map(|r| cell_str(r, 0)))
    }

    /// Every indexed file's rel path. Feeds the ignore-resync pass
    /// (`GraphService::spawn_ignore_resync`), which must test each stored file
    /// against the new `graph.ignore` globs to drop the now-excluded ones.
    pub fn all_file_paths(&self) -> AppResult<Vec<String>> {
        let rows = self.run(
            "?[path] := *file{path}",
            BTreeMap::new(),
            ScriptMutability::Immutable,
        )?;
        Ok(rows.rows.iter().map(|r| cell_str(r, 0)).collect())
    }

    /// Files ranked by inbound-call centrality (how many call sites target a
    /// symbol defined in the file), most-central first, capped at `max`. Feeds
    /// the V11 Phase B project map. Name-keyed like the other call queries, so a
    /// file defining a very common name ranks high — an orientation signal, not
    /// a precise metric.
    pub fn file_centrality(&self, max: usize) -> AppResult<Vec<(String, u64)>> {
        // Project DISTINCT (file, src, dst) then count in Rust. A `count(src)`
        // aggregate over the join would multiply a single call edge by the number
        // of same-named symbols in the target file (e.g. `A::new` + `B::new` in
        // one file both match `dst == "new"`), inflating that file's centrality.
        // The distinct projection collapses those duplicate matches to one row.
        let rows = self.run(
            r#"?[file, src, dst] := *edge{kind: k, src, dst}, k == "call", *symbol{name: dst, file}"#,
            BTreeMap::new(),
            ScriptMutability::Immutable,
        )?;
        let mut counts: std::collections::HashMap<String, u64> = std::collections::HashMap::new();
        for r in &rows.rows {
            *counts.entry(cell_str(r, 0)).or_default() += 1;
        }
        let mut v: Vec<(String, u64)> = counts.into_iter().collect();
        v.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        v.truncate(max);
        Ok(v)
    }

    /// Transitive call-chain names. `forward = true` returns everything `name`
    /// transitively calls; `false` returns everything that transitively calls
    /// `name`. Recursive Datalog over a name-level call graph; terminates on
    /// cycles (set saturation).
    pub fn transitive(&self, name: &str, forward: bool) -> AppResult<Vec<String>> {
        let mut p = BTreeMap::new();
        p.insert("name".to_string(), DataValue::Str(name.into()));
        let limit = Self::TRANSITIVE_LIMIT;
        // F10: SEED the recursion from `$name` rather than materializing the
        // whole-graph closure `reach[x, y]` (every source/target pair) and
        // filtering `x == $name` only at the head — that forced Cozo to compute
        // reachability for every symbol before discarding all but `$name`'s row,
        // an O(V·E) blowup on a large call graph. The seeded single-argument
        // `reach[_]` only explores the subgraph actually reachable from `$name`.
        let rules = if forward {
            // Everything `$name` transitively calls.
            r#"reach[y] := calls[x, y], x == $name
reach[y] := reach[z], calls[z, y]"#
        } else {
            // Everything that transitively calls `$name`.
            r#"reach[x] := calls[x, y], y == $name
reach[x] := reach[z], calls[x, z]"#
        };
        let script = format!(
            r#"calls[cn, dn] := *symbol{{id: cid, name: cn}}, *edge{{kind: ek, src: cid, dst: dn}}, ek == "call"
{rules}
?[n] := reach[n]
:order n
:limit {limit}"#
        );
        let rows = self.run(&script, p, ScriptMutability::Immutable)?;
        Ok(rows.rows.iter().map(|r| cell_str(r, 0)).collect())
    }

    /// V12 Phase B: everything that (transitively) calls one of `roots`, up to
    /// `depth` hops — the "blast radius" behind `graph_impact`. A plain Rust
    /// BFS over a reverse name-level call adjacency built from a single scan
    /// of the `edge`/`symbol` relations, rather than recursive Datalog with a
    /// depth counter threaded through it: easier to cap, dedupe by minimum
    /// depth, and test. `depth` is clamped to `1..=6`; results are capped at
    /// `max`, sorted by `(depth, name)`. `roots` themselves are never reported
    /// (their callers are found directly since every root seeds the BFS's
    /// first frontier, so nothing is lost by not also re-expanding through a
    /// root discovered as someone else's caller). Empty `roots` → empty
    /// result, no query run.
    ///
    /// `min_confidence` filters the blast radius to dependents at least that
    /// certain. It is applied to the full dependent set *before* the `max`
    /// cap, so a confidence-filtered result is not silently truncated by the
    /// cap dropping certain rows in favour of less-certain ones.
    pub fn dependents_transitive(
        &self,
        roots: &[String],
        depth: u32,
        max: usize,
        min_confidence: Option<Confidence>,
    ) -> AppResult<Vec<DependentHit>> {
        if roots.is_empty() {
            return Ok(Vec::new());
        }
        let depth = depth.clamp(1, 6);

        // One scan of every name-level call edge: (caller name, callee name,
        // stored edge confidence).
        let rows = self.run(
            r#"?[caller, callee, conf] := *symbol{id: cid, name: caller}, *edge{kind: ek, src: cid, dst: callee, confidence: conf}, ek == "call""#,
            BTreeMap::new(),
            ScriptMutability::Immutable,
        )?;
        // Names defined by more than one symbol: any call edge targeting such a
        // name is `Ambiguous` regardless of its stored confidence (we can't tell
        // which definition it hits). One scan, reused across the whole BFS.
        let multi = self.multi_candidate_names()?;
        // Reverse adjacency: callee name -> distinct (caller name, effective edge
        // confidence). Effective = Ambiguous when the callee is multi-candidate.
        let mut rev: HashMap<String, HashMap<String, Confidence>> = HashMap::new();
        for r in &rows.rows {
            let caller = cell_str(r, 0);
            let callee = cell_str(r, 1);
            let ec = if multi.contains(&callee) {
                Confidence::Ambiguous
            } else {
                Confidence::from_tag(&cell_str(r, 2))
            };
            // Keep the strongest edge if a pair appears twice — the caller does
            // reach the callee at least that certainly.
            let slot = rev.entry(callee).or_default().entry(caller).or_insert(ec);
            *slot = slot.stronger(ec);
        }

        let root_set: HashSet<&str> = roots.iter().map(|s| s.as_str()).collect();
        // Per dependent: (min depth, weakest confidence along its discovery chain).
        let mut best: HashMap<String, (u32, Confidence)> = HashMap::new();
        // Roots seed the BFS at full certainty; each hop weakens by its edge.
        let mut frontier: HashMap<String, Confidence> = roots
            .iter()
            .map(|n| (n.clone(), Confidence::Extracted))
            .collect();
        for d in 1..=depth {
            let mut next_frontier: HashMap<String, Confidence> = HashMap::new();
            for (name, chain_conf) in &frontier {
                let Some(callers) = rev.get(name) else {
                    continue;
                };
                for (caller, ec) in callers {
                    if root_set.contains(caller.as_str()) {
                        continue;
                    }
                    let cc = chain_conf.weaker(*ec);
                    match best.get(caller) {
                        // Already discovered at a shallower depth: min depth
                        // wins; it was expanded there, don't revisit.
                        Some((bd, _)) if *bd < d => {}
                        // Reached again at the SAME depth via another path: keep
                        // the strongest chain confidence. `stronger` is
                        // commutative, so the result is independent of the
                        // (randomized) HashMap iteration order — the fix for the
                        // previous "first path processed wins" non-determinism.
                        Some((bd, bc)) if *bd == d => {
                            let merged = bc.stronger(cc);
                            best.insert(caller.clone(), (d, merged));
                            next_frontier.insert(caller.clone(), merged);
                        }
                        // First discovery: this depth is the min depth.
                        _ => {
                            best.insert(caller.clone(), (d, cc));
                            next_frontier.insert(caller.clone(), cc);
                        }
                    }
                }
            }
            if next_frontier.is_empty() {
                break;
            }
            frontier = next_frontier;
        }

        let mut names: Vec<(String, u32, Confidence)> =
            best.into_iter().map(|(n, (d, c))| (n, d, c)).collect();
        // Confidence filter runs BEFORE the `max` cap below, so a filtered
        // blast radius keeps its most-certain rows rather than losing them to
        // the cap (which is applied during symbol resolution).
        if let Some(floor) = min_confidence {
            names.retain(|(_, _, c)| c.rank() >= floor.rank());
        }
        names.sort_by(|a, b| a.1.cmp(&b.1).then_with(|| a.0.cmp(&b.0)));

        // F15: resolve every dependent name to its symbol(s) in ONE query — a
        // join of the `symbol` relation against the collected name set passed as
        // an inline relation — instead of a `find_symbol` round trip per name. A
        // `graph_impact` with 100+ transitive dependents otherwise fired 100+
        // individual queries on this interactive path.
        let mut by_name: HashMap<String, Vec<SymbolHit>> = HashMap::new();
        if !names.is_empty() {
            let name_rows: Vec<DataValue> = names
                .iter()
                .map(|(n, _, _)| DataValue::List(vec![DataValue::Str(n.as_str().into())]))
                .collect();
            let mut p = BTreeMap::new();
            p.insert("names".to_string(), DataValue::List(name_rows));
            let rows = self.run(
                "want[name] <- $names\n\
                 ?[id, name, kind, file, start_line, signature, visibility, end_line, is_test] := \
                    want[name], \
                    *symbol{id, name, kind, file, start_line, signature, visibility, end_line, is_test}",
                p,
                ScriptMutability::Immutable,
            )?;
            for s in rows_to_symbols(&rows) {
                by_name.entry(s.name.clone()).or_default().push(s);
            }
        }

        let mut hits: Vec<DependentHit> = Vec::new();
        for (name, d, conf) in names {
            if let Some(syms) = by_name.remove(&name) {
                for symbol in syms {
                    hits.push(DependentHit {
                        symbol,
                        depth: d,
                        approx: true,
                        confidence: conf,
                    });
                    if hits.len() >= max {
                        return Ok(hits);
                    }
                }
            }
        }
        Ok(hits)
    }

    /// V15 Feature 1: the shortest ordered path between two code entities across
    /// a unified view of the `Call`/`Import`/`Contains` edge kinds (restricted by
    /// `kinds`). `from`/`to` accept a symbol name, a `file:line`, or a file path
    /// (resolved like `graph_snippet`); an ambiguous endpoint seeds/accepts every
    /// candidate. A Rust-side **BFS with parent pointers** over a name-resolved,
    /// multi-kind adjacency built from one scan of each relation — the same
    /// pattern `transitive`/`dependents_transitive` use, extended to span edge
    /// kinds and record predecessors. `symmetric` walks edges undirected ("are
    /// these related at all?"). Bounded by `max_hops`. Returns `None` when an
    /// endpoint is unresolvable or no path exists within the bound.
    pub fn shortest_path(
        &self,
        from: &str,
        to: &str,
        kinds: &[EdgeKind],
        max_hops: usize,
        symmetric: bool,
    ) -> AppResult<Option<PathHit>> {
        let want_call = kinds.contains(&EdgeKind::Call);
        let want_import = kinds.contains(&EdgeKind::Import);
        let want_contains = kinds.contains(&EdgeKind::Contains);

        // 1. Symbols → node metadata + name→ids map.
        let sym_rows = self.run(
            "?[id, name, kind, file, start_line] := *symbol{id, name, kind, file, start_line}",
            BTreeMap::new(),
            ScriptMutability::Immutable,
        )?;
        let mut meta: HashMap<String, PathNode> = HashMap::new();
        let mut name_to_ids: HashMap<String, Vec<String>> = HashMap::new();
        for r in &sym_rows.rows {
            let id = cell_str(r, 0);
            let name = cell_str(r, 1);
            name_to_ids
                .entry(name.clone())
                .or_default()
                .push(id.clone());
            meta.insert(
                id.clone(),
                PathNode {
                    id,
                    label: name,
                    file: cell_str(r, 3),
                    line: cell_i64(r, 4) as u32,
                    kind: cell_str(r, 2),
                    edge_to_next: None,
                    confidence: None,
                },
            );
        }
        let multi = self.multi_candidate_names()?;

        // 2. Files → node metadata + lang table (for import resolution).
        let file_rows = self.run(
            "?[path, lang] := *file{path, lang}",
            BTreeMap::new(),
            ScriptMutability::Immutable,
        )?;
        let mut file_lang: HashMap<String, String> = HashMap::new();
        let mut known_files: HashSet<String> = HashSet::new();
        for r in &file_rows.rows {
            let path = cell_str(r, 0);
            file_lang.insert(path.clone(), cell_str(r, 1));
            let node_id = format!("file:{path}");
            meta.insert(
                node_id.clone(),
                PathNode {
                    id: node_id,
                    label: path.clone(),
                    file: path.clone(),
                    line: 0,
                    kind: "file".to_string(),
                    edge_to_next: None,
                    confidence: None,
                },
            );
            known_files.insert(path);
        }

        // 3. Build the multi-kind adjacency + a per-pair edge (kind, confidence)
        //    lookup for reconstruction. Scoped so the `add` closure's mutable
        //    borrow of `adj`/`edge_kind` is released before the BFS reads them.
        let mut adj: HashMap<String, Vec<String>> = HashMap::new();
        let mut edge_kind: HashMap<(String, String), (EdgeKind, Confidence)> = HashMap::new();
        {
            let mut add = |a: String, b: String, k: EdgeKind, c: Confidence| {
                adj.entry(a.clone()).or_default().push(b.clone());
                let e = edge_kind.entry((a.clone(), b.clone())).or_insert((k, c));
                e.1 = e.1.stronger(c);
                if symmetric {
                    adj.entry(b.clone()).or_default().push(a.clone());
                    let er = edge_kind.entry((b, a)).or_insert((k, c));
                    er.1 = er.1.stronger(c);
                }
            };

            if want_call {
                let rows = self.run(
                    r#"?[src, dst, conf] := *edge{kind: k, src, dst, confidence: conf}, k == "call""#,
                    BTreeMap::new(),
                    ScriptMutability::Immutable,
                )?;
                for r in &rows.rows {
                    let src = cell_str(r, 0);
                    if !meta.contains_key(&src) {
                        continue;
                    }
                    let dst = cell_str(r, 1);
                    let conf = if multi.contains(&dst) {
                        Confidence::Ambiguous
                    } else {
                        Confidence::from_tag(&cell_str(r, 2))
                    };
                    if let Some(ids) = name_to_ids.get(&dst) {
                        for callee in ids {
                            add(src.clone(), callee.clone(), EdgeKind::Call, conf);
                        }
                    }
                }
            }
            if want_contains {
                let rows = self.run(
                    r#"?[src, dst] := *edge{kind: k, src, dst}, k == "contains""#,
                    BTreeMap::new(),
                    ScriptMutability::Immutable,
                )?;
                for r in &rows.rows {
                    let (src, dst) = (cell_str(r, 0), cell_str(r, 1));
                    if meta.contains_key(&src) && meta.contains_key(&dst) {
                        add(src, dst, EdgeKind::Contains, Confidence::Extracted);
                    }
                }
                // Synthesize file→symbol containment so a file node connects to
                // every definition it holds (covers top-level defs that have no
                // stored Contains parent).
                for r in &sym_rows.rows {
                    let id = cell_str(r, 0);
                    let file_node = format!("file:{}", cell_str(r, 3));
                    if meta.contains_key(&file_node) {
                        add(file_node, id, EdgeKind::Contains, Confidence::Extracted);
                    }
                }
            }
            if want_import {
                let rows = self.run(
                    r#"?[src, dst, conf] := *edge{kind: k, src, dst, confidence: conf}, k == "import""#,
                    BTreeMap::new(),
                    ScriptMutability::Immutable,
                )?;
                for r in &rows.rows {
                    let from_file = cell_str(r, 0);
                    let module = cell_str(r, 1);
                    let lang =
                        Lang::from_tag(file_lang.get(&from_file).map(|s| s.as_str()).unwrap_or(""));
                    if let Some(target) = resolve_import(lang, &from_file, &module, &known_files) {
                        let conf = Confidence::from_tag(&cell_str(r, 2));
                        add(
                            format!("file:{from_file}"),
                            format!("file:{target}"),
                            EdgeKind::Import,
                            conf,
                        );
                    }
                }
            }
        }

        // 4. Resolve endpoints (each may yield several candidate nodes).
        let from_nodes = self.resolve_path_endpoint(from, &name_to_ids, &known_files)?;
        let to_set: HashSet<String> = self
            .resolve_path_endpoint(to, &name_to_ids, &known_files)?
            .into_iter()
            .collect();
        if from_nodes.is_empty() || to_set.is_empty() {
            return Ok(None);
        }
        // A source that is already a target → a zero-hop path.
        let mut src_is_target: Vec<String> = from_nodes
            .iter()
            .filter(|n| to_set.contains(*n))
            .cloned()
            .collect();
        src_is_target.sort();
        if let Some(t) = src_is_target.first() {
            if let Some(node) = meta.get(t).cloned() {
                return Ok(Some(PathHit {
                    nodes: vec![node],
                    hops: 0,
                    equal_alternatives: (src_is_target.len() - 1) as u64,
                }));
            }
        }

        // 5. Level-synchronized BFS with predecessor + shortest-path-count
        //    tracking (so `equal_alternatives` is exact, and the reported path is
        //    deterministic — smallest-id predecessor wins ties).
        let mut dist: HashMap<String, usize> = HashMap::new();
        let mut parent: HashMap<String, String> = HashMap::new();
        let mut npaths: HashMap<String, u64> = HashMap::new();
        let mut frontier: Vec<String> = Vec::new();
        for f in &from_nodes {
            if meta.contains_key(f) && dist.insert(f.clone(), 0).is_none() {
                npaths.insert(f.clone(), 1);
                frontier.push(f.clone());
            }
        }

        for depth in 1..=max_hops {
            frontier.sort();
            let mut next: Vec<String> = Vec::new();
            for u in &frontier {
                let up = npaths.get(u).copied().unwrap_or(0);
                let Some(neighbors) = adj.get(u) else {
                    continue;
                };
                let mut ns = neighbors.clone();
                ns.sort();
                ns.dedup();
                for v in ns {
                    match dist.get(&v).copied() {
                        None => {
                            dist.insert(v.clone(), depth);
                            parent.insert(v.clone(), u.clone());
                            npaths.insert(v.clone(), up);
                            next.push(v);
                        }
                        Some(dv) if dv == depth => {
                            *npaths.entry(v.clone()).or_insert(0) += up;
                            if parent.get(&v).map(|p| u < p).unwrap_or(false) {
                                parent.insert(v, u.clone());
                            }
                        }
                        _ => {}
                    }
                }
            }
            let mut hits: Vec<String> = next
                .iter()
                .filter(|n| to_set.contains(*n))
                .cloned()
                .collect();
            if !hits.is_empty() {
                hits.sort();
                let target = &hits[0];
                let mut chain: Vec<String> = Vec::new();
                let mut cur = target.clone();
                loop {
                    chain.push(cur.clone());
                    match parent.get(&cur) {
                        Some(p) => cur = p.clone(),
                        None => break,
                    }
                }
                chain.reverse();
                let mut nodes: Vec<PathNode> = Vec::with_capacity(chain.len());
                for (i, id) in chain.iter().enumerate() {
                    let mut n = meta.get(id).cloned().unwrap_or_else(|| PathNode {
                        id: id.clone(),
                        label: id.clone(),
                        file: String::new(),
                        line: 0,
                        kind: "?".to_string(),
                        edge_to_next: None,
                        confidence: None,
                    });
                    if let Some(nxt) = chain.get(i + 1) {
                        if let Some((k, c)) = edge_kind.get(&(id.clone(), nxt.clone())) {
                            n.edge_to_next = Some(k.tag().to_string());
                            n.confidence = Some(*c);
                        }
                    }
                    nodes.push(n);
                }
                let hops = nodes.len() - 1;
                let alt = npaths.get(target).copied().unwrap_or(1).saturating_sub(1);
                return Ok(Some(PathHit {
                    nodes,
                    hops,
                    equal_alternatives: alt,
                }));
            }
            if next.is_empty() {
                break;
            }
            frontier = next;
        }
        Ok(None)
    }

    /// Resolve a path endpoint string to its candidate node ids: a `file:line`
    /// (→ the enclosing symbol, or the file node), a symbol name (→ every
    /// definition of it), or a bare file path (→ the file node). Empty when it
    /// resolves to nothing indexed.
    fn resolve_path_endpoint(
        &self,
        s: &str,
        name_to_ids: &HashMap<String, Vec<String>>,
        known_files: &HashSet<String>,
    ) -> AppResult<Vec<String>> {
        let s = s.trim();
        if let Some((f, l)) = s.rsplit_once(':') {
            if let Ok(line) = l.trim().parse::<u32>() {
                if known_files.contains(f) {
                    return Ok(match self.symbol_at(f, line)? {
                        Some(sym) => vec![sym.id],
                        None => vec![format!("file:{f}")],
                    });
                }
            }
        }
        if let Some(ids) = name_to_ids.get(s) {
            if !ids.is_empty() {
                return Ok(ids.clone());
            }
        }
        if known_files.contains(s) {
            return Ok(vec![format!("file:{s}")]);
        }
        Ok(Vec::new())
    }

    /// V12 Phase C: the **candidate** tests that (transitively) depend on one
    /// of `roots` — the engine behind `graph_tests_for` and `graph_impact`'s
    /// `include_tests` block. Reuses [`Self::dependents_transitive`] outright
    /// (same BFS, same `depth`/`max`/approx semantics) and filters to symbols a
    /// walker tagged `is_test`, rather than a second recursion. Because `max`
    /// caps the *pre-filter* dependent set, a very tight `max` can undercount
    /// tests that would otherwise surface a little further down the ranked
    /// list — callers that want an exhaustive test list should pass a
    /// generous `max`. Same caveat convention as `dead_exports`: dynamic
    /// dispatch and fixture-driven tests have no static call edge and won't
    /// appear here.
    pub fn tests_for(&self, roots: &[String], depth: u32, max: usize) -> AppResult<Vec<SymbolHit>> {
        Ok(self
            .dependents_transitive(roots, depth, max, None)?
            .into_iter()
            .filter(|hit| hit.symbol.is_test)
            .map(|hit| hit.symbol)
            .collect())
    }

    /// Full-text-ish documentation search. MVP: a case-insensitive substring
    /// match over chunked doc text (a real FTS index is a Phase-G refinement).
    pub fn search_docs(
        &self,
        query: &str,
        max_rows: usize,
        max_snippet: usize,
    ) -> AppResult<Vec<DocHit>> {
        let rows = self.run(
            "?[source_path, anchor, text] := *doc_chunk{source_path, anchor, text}",
            BTreeMap::new(),
            ScriptMutability::Immutable,
        )?;
        let needle = query.to_lowercase();
        let mut hits = Vec::new();
        for r in &rows.rows {
            let source_path = cell_str(r, 0);
            let anchor = cell_str(r, 1);
            let text = cell_str(r, 2);
            if text.to_lowercase().contains(&needle) || anchor.to_lowercase().contains(&needle) {
                hits.push(DocHit {
                    source_path,
                    anchor,
                    snippet: truncate_chars(&text, max_snippet),
                });
                if hits.len() >= max_rows {
                    break;
                }
            }
        }
        Ok(hits)
    }

    /// Count, per source file, how many of `terms` appear in its doc chunks —
    /// in a **single** scan of `doc_chunk` (each chunk lower-cased once). Used by
    /// context ranking on the per-prompt hot path, where calling `search_docs`
    /// once per term would re-scan and re-lowercase the whole table N times.
    /// `terms` are matched case-insensitively; empty terms are ignored.
    pub fn doc_source_hits(&self, terms: &[String]) -> AppResult<HashMap<String, u32>> {
        let needles: Vec<String> = terms
            .iter()
            .map(|t| t.to_lowercase())
            .filter(|t| !t.is_empty())
            .collect();
        let mut out: HashMap<String, u32> = HashMap::new();
        if needles.is_empty() {
            return Ok(out);
        }
        let rows = self.run(
            "?[source_path, anchor, text] := *doc_chunk{source_path, anchor, text}",
            BTreeMap::new(),
            ScriptMutability::Immutable,
        )?;
        for r in &rows.rows {
            let source_path = cell_str(r, 0);
            let hay = format!("{} {}", cell_str(r, 1), cell_str(r, 2)).to_lowercase();
            let hits = needles.iter().filter(|n| hay.contains(n.as_str())).count() as u32;
            if hits > 0 {
                *out.entry(source_path).or_default() += hits;
            }
        }
        Ok(out)
    }

    /// Row counts for the status surface.
    pub fn stats(&self) -> AppResult<GraphStats> {
        let count = |rel: &str| -> AppResult<u64> {
            let script = format!("?[count(x)] := *{rel}{{}}, x = 1");
            let rows = self.run(&script, BTreeMap::new(), ScriptMutability::Immutable)?;
            Ok(rows
                .rows
                .first()
                .and_then(|r| r.first())
                .map(dv_i64)
                .unwrap_or(0) as u64)
        };
        Ok(GraphStats {
            files: count("file")?,
            symbols: count("symbol")?,
            edges: count("edge")?,
            by_lang: self.files_by_lang()?,
        })
    }

    /// Indexed-file count per language, biggest first (ties broken by language
    /// name so the order is stable across scans). Feeds the monitor tab's
    /// per-language table.
    fn files_by_lang(&self) -> AppResult<Vec<LangCount>> {
        let rows = self.run(
            "?[lang, count(path)] := *file{path, lang}",
            BTreeMap::new(),
            ScriptMutability::Immutable,
        )?;
        let mut out: Vec<LangCount> = rows
            .rows
            .iter()
            .map(|r| LangCount {
                lang: cell_str(r, 0),
                files: cell_i64(r, 1) as u64,
            })
            .collect();
        out.sort_by(|a, b| b.files.cmp(&a.files).then_with(|| a.lang.cmp(&b.lang)));
        Ok(out)
    }

    fn put(&self, script: &str, rows: Vec<DataValue>) -> AppResult<()> {
        let mut p = BTreeMap::new();
        p.insert("rows".to_string(), DataValue::List(rows));
        self.run_mut(script, p)?;
        Ok(())
    }

    fn run_mut(
        &self,
        script: &str,
        params: BTreeMap<String, DataValue>,
    ) -> AppResult<cozo::NamedRows> {
        self.run(script, params, ScriptMutability::Mutable)
    }

    fn run(
        &self,
        script: &str,
        params: BTreeMap<String, DataValue>,
        m: ScriptMutability,
    ) -> AppResult<cozo::NamedRows> {
        self.db
            .run_script(script, params, m)
            .map_err(|e| AppError::Graph(format!("query failed: {e}")))
    }

    /// The crash-safe **stage-and-swap** engine every memory-relation shape
    /// migration runs: read the old shape with [`StageAndSwap::read_script`],
    /// append the new columns' [`StageAndSwap::defaults`], build a
    /// fully-populated CURRENT-shape stage, verify its row count, then swap.
    ///
    /// # Why it is shaped this way
    ///
    /// CozoDB autocommits each script, so a naive read → `::remove` → `:create`
    /// → `:put` sequence has a window where a kill after the remove loses the
    /// whole relation. Instead the old relation stays the source of truth until
    /// a fully-populated new-shape STAGE is durable and verified; only then is
    /// the original dropped and the stage promoted. The stage is built with a
    /// single atomic `:create … <- $rows`, so it is never partial — its mere
    /// presence on a later open (the recovery branch) means the migrated data is
    /// safe and should be adopted, even though `ensure_memory_relations` runs
    /// first on open and may have recreated the live relation empty in the
    /// meantime. A short copy means something went wrong: the suspect stage is
    /// dropped and the call fails loudly rather than promoting it over the
    /// still-intact live data.
    ///
    /// # Why one engine (V42 R12)
    ///
    /// `usage_stat`'s V24 migration and `mem_note`'s V32 one were separate
    /// hand-written copies of the sequence above — the second one's own doc said
    /// "mechanically identical to" the first, which is a comment where a
    /// function belongs. The crash-safety is the subtle part, and a second copy
    /// of it is a second chance to get the abort path wrong. What stays per
    /// relation is only data: its historical shapes, its defaults, and the two
    /// closures whose statements have to name the relation as a literal.
    ///
    /// The recovery branch is shared too, and deliberately: the stage always
    /// holds the *current* shape, whichever migration built it, so whichever one
    /// runs first may adopt it. What must never happen is adopting the live
    /// relation over a populated stage — that is the direction that loses rows.
    fn stage_and_swap(&self, m: StageAndSwap<'_>) -> AppResult<()> {
        let existing = self.existing_relations()?;
        // Recovery: a leftover stage means a prior migration was interrupted
        // after the stage was durably populated (possibly mid-swap, after the
        // live relation was dropped and recreated empty by
        // `ensure_memory_relations`). Adopt the stage over whatever the live
        // relation currently is — never the reverse, so no rows are lost.
        if existing.contains(m.stage) {
            return (m.promote)();
        }
        if !existing.contains(m.live) {
            return Ok(());
        }
        if self.relation_has_column(m.live, m.added_column)? {
            return Ok(());
        }
        // Forward migration. Read every old-shape row, then build a
        // fully-populated new-shape stage, verify it captured every row, and
        // only THEN drop the original and promote the stage.
        let rows = self.run(m.read_script, BTreeMap::new(), ScriptMutability::Immutable)?;
        let expected = rows.rows.len();
        let migrated: Vec<DataValue> = rows
            .rows
            .into_iter()
            .map(|mut r| {
                r.extend(m.defaults.iter().cloned());
                DataValue::List(r)
            })
            .collect();
        // Build the stage as a single atomic create-and-populate so it is either
        // absent or complete — the invariant the recovery branch relies on. An
        // empty source has no rows to lose, so create the stage empty in that
        // case (avoids feeding `<- $rows` an empty list).
        if migrated.is_empty() {
            self.run_mut(m.stage_ddl, BTreeMap::new())?;
        } else {
            let mut p = BTreeMap::new();
            p.insert("rows".to_string(), DataValue::List(migrated));
            self.run_mut(
                &format!("?[{}] <- $rows\n{}", m.stage_columns, m.stage_ddl),
                p,
            )?;
        }
        // Verify the stage captured every old row before dropping the original.
        let staged = (m.count_stage)()?;
        if staged != expected {
            self.run_mut(&format!("::remove {}", m.stage), BTreeMap::new())?;
            return Err(AppError::Graph(format!(
                "{} migration stage captured {staged} of {expected} rows; aborting",
                m.live
            )));
        }
        (m.promote)()
    }

    /// Whether the on-disk relation `rel` carries column `col`. Introspects via
    /// `::columns` (its column-name header is `column`), failing loudly if that
    /// shape ever changes rather than mis-migrating. [`Self::stage_and_swap`]'s
    /// idempotence probe, so both the V24 `usage_stat` and V32 `mem_note`
    /// migrations can detect "already migrated" and calling either is safe.
    fn relation_has_column(&self, rel: &str, col_name: &str) -> AppResult<bool> {
        let rows = self.run(
            &format!("::columns {rel}"),
            BTreeMap::new(),
            ScriptMutability::Immutable,
        )?;
        let col = rows
            .headers
            .iter()
            .position(|h| h == "column")
            .ok_or_else(|| {
                AppError::Graph("::columns result has no 'column' column".to_string())
            })?;
        Ok(rows
            .rows
            .iter()
            .any(|r| r.get(col).map(dv_string).as_deref() == Some(col_name)))
    }
}

/// Run one script inside a multi-transaction, mapping cozo errors to [`AppError`].
fn tx_run(
    tx: &MultiTransaction,
    script: &str,
    params: BTreeMap<String, DataValue>,
) -> AppResult<cozo::NamedRows> {
    tx.run_script(script, params)
        .map_err(|e| AppError::Graph(format!("query failed: {e}")))
}

/// `:put` a batch of rows (bound to `$rows`) inside a multi-transaction.
fn tx_put(tx: &MultiTransaction, script: &str, rows: Vec<DataValue>) -> AppResult<()> {
    let mut p = BTreeMap::new();
    p.insert("rows".to_string(), DataValue::List(rows));
    tx_run(tx, script, p)?;
    Ok(())
}

/// Delete every row belonging to `file` within an open transaction (symbols,
/// refs, doc-chunks, code-chunks, the `file` row, and edges keyed by the file
/// id-prefix or `src == file`). Returns the names the file defined so the
/// caller can run [`purge_dangling_call_edges_in_tx`] — after the re-insert on
/// the replace path ([`GraphIndex::index_file_graph`]), so names the new file
/// version still defines keep their inbound call edges from other files, or
/// immediately on the true-delete path. The transaction makes remove + purge
/// (+ re-insert) atomic.
fn remove_file_in_tx(tx: &MultiTransaction, file: &str) -> AppResult<Vec<String>> {
    let prefix = format!("{file}#");
    let mut p = BTreeMap::new();
    p.insert("file".to_string(), DataValue::Str(file.into()));

    // Names this file DEFINES, captured before we delete its symbols. Any of
    // these left with no definition anywhere (checked by the purge helper once
    // the caller is done mutating) is "dangling": its inbound call edges
    // (owned by other files) would otherwise survive and make `callers()`
    // report ghosts of a symbol that no longer exists. External/unresolved
    // call targets (stdlib names that never had a symbol) are NOT in this set,
    // so they're left untouched.
    let defined = tx_run(
        tx,
        "?[name] := *symbol{file, name}, file == $file",
        p.clone(),
    )?;
    let defined_names: Vec<String> = defined.rows.iter().map(|r| cell_str(r, 0)).collect();

    tx_run(
        tx,
        "?[id] := *symbol{id, file}, file == $file\n:rm symbol {id}",
        p.clone(),
    )?;
    tx_run(
        tx,
        "?[file, line, col, name] := *ref{file, line, col, name}, file == $file\n:rm ref {file, line, col, name}",
        p.clone(),
    )?;
    tx_run(
        tx,
        "?[id] := *doc_chunk{id, source_path}, source_path == $file\n:rm doc_chunk {id}",
        p.clone(),
    )?;
    tx_run(
        tx,
        "?[id] := *code_chunk{id, file}, file == $file\n:rm code_chunk {id}",
        p.clone(),
    )?;
    tx_run(
        tx,
        "?[path] := *file{path}, path == $file\n:rm file {path}",
        p.clone(),
    )?;
    let mut pe = p;
    pe.insert("prefix".to_string(), DataValue::Str(prefix.as_str().into()));
    tx_run(
        tx,
        "?[kind, src, dst] := *edge{kind, src, dst}, (starts_with(src, $prefix) or src == $file)\n:rm edge {kind, src, dst}",
        pe,
    )?;
    Ok(defined_names)
}

/// Drop inbound call edges to any of `names` that no longer has a definition.
/// Must run in the same transaction as (and after) the removes/re-inserts it
/// cleans up for: a name the re-index just re-inserted still has a symbol row,
/// so its inbound edges from other files survive; only genuinely gone names
/// lose theirs. Running this BEFORE the re-insert is the bug this split fixes —
/// every uniquely-defined name read as dangling on an ordinary edit, deleting
/// call edges owned by unchanged caller files.
fn purge_dangling_call_edges_in_tx(
    tx: &MultiTransaction,
    names: impl IntoIterator<Item = String>,
) -> AppResult<()> {
    for name in names {
        let mut pn = BTreeMap::new();
        pn.insert("name".to_string(), DataValue::Str(name.as_str().into()));
        let still = tx_run(
            tx,
            "?[id] := *symbol{id, name}, name == $name\n:limit 1",
            pn.clone(),
        )?;
        if still.rows.is_empty() {
            tx_run(
                tx,
                "?[kind, src, dst] := *edge{kind, src, dst}, kind == \"call\", dst == $name\n:rm edge {kind, src, dst}",
                pn,
            )?;
        }
    }
    Ok(())
}

fn int(n: u32) -> DataValue {
    DataValue::Num(Num::Int(n as i64))
}

fn dv_string(v: &DataValue) -> String {
    match v {
        DataValue::Str(s) => s.to_string(),
        other => other.to_string(),
    }
}

fn dv_i64(v: &DataValue) -> i64 {
    match v {
        DataValue::Num(Num::Int(i)) => *i,
        DataValue::Num(Num::Float(f)) => *f as i64,
        _ => 0,
    }
}

fn cell_str(row: &[DataValue], i: usize) -> String {
    row.get(i).map(dv_string).unwrap_or_default()
}

fn cell_i64(row: &[DataValue], i: usize) -> i64 {
    row.get(i).map(dv_i64).unwrap_or(0)
}

fn cell_bool(row: &[DataValue], i: usize) -> bool {
    matches!(row.get(i), Some(DataValue::Bool(true)))
}

/// Map query rows shaped `[id, name, kind, file, start_line, signature,
/// visibility, end_line, is_test]` to [`SymbolHit`]s. Queries that don't
/// project `visibility` (7th column absent) get `"unknown"`; those that don't
/// project `end_line` (8th column absent) fall back to `start_line`; those
/// that don't project `is_test` (9th column absent) default to `false`.
fn rows_to_symbols(rows: &cozo::NamedRows) -> Vec<SymbolHit> {
    rows.rows.iter().map(|r| row_to_symbol(r)).collect()
}

/// Attach the effective confidence to an edge-bearing symbol hit: `Ambiguous`
/// when `ambiguous` (the name was multi-candidate), otherwise the stored edge
/// confidence read from column `conf_col`.
fn with_row_confidence(
    mut hit: SymbolHit,
    r: &[DataValue],
    conf_col: usize,
    ambiguous: bool,
) -> SymbolHit {
    hit.confidence = Some(if ambiguous {
        Confidence::Ambiguous
    } else {
        Confidence::from_tag(&cell_str(r, conf_col))
    });
    hit
}

/// Map one `[id, name, kind, file, start_line, signature, visibility?,
/// end_line?, is_test?]` row into a [`SymbolHit`]. `confidence` is `None` —
/// edge-bearing queries (callers/callees) set it from a projected `conf` column
/// afterward via [`with_row_confidence`].
fn row_to_symbol(r: &[DataValue]) -> SymbolHit {
    let start_line = cell_i64(r, 4) as u32;
    SymbolHit {
        id: cell_str(r, 0),
        name: cell_str(r, 1),
        kind: cell_str(r, 2),
        file: cell_str(r, 3),
        start_line,
        signature: cell_str(r, 5),
        visibility: if r.len() > 6 {
            cell_str(r, 6)
        } else {
            "unknown".to_string()
        },
        end_line: if r.len() > 7 {
            cell_i64(r, 7) as u32
        } else {
            start_line
        },
        is_test: cell_bool(r, 8),
        // A plain definition lookup involves no edge — callers/callees set this
        // explicitly from the surfacing edge's confidence.
        confidence: None,
    }
}

/// Whether `name` is a conventional entrypoint / trait-method that's routinely
/// "unreferenced by name" yet not actually dead — kept out of `dead_exports` to
/// cut the obvious false positives. Deliberately small and conservative.
fn is_entrypoint_name(name: &str) -> bool {
    if name == "main" || name.starts_with("test_") {
        return true;
    }
    matches!(
        name,
        "new"
            | "default"
            | "from"
            | "into"
            | "fmt"
            | "drop"
            | "clone"
            | "eq"
            | "ne"
            | "hash"
            | "cmp"
            | "partial_cmp"
            | "deref"
            | "as_ref"
            | "serialize"
            | "deserialize"
    )
}

/// Best-effort resolution of an import's raw module string to a concrete indexed
/// file, per language. Returns `None` (dropped by the cycle detector) for
/// external/unresolvable modules or languages without a resolver. Paths are
/// project-relative with `/` separators (as stored on graph rows).
fn resolve_import(
    lang: Lang,
    from_file: &str,
    module: &str,
    known: &HashSet<String>,
) -> Option<String> {
    match lang {
        Lang::TypeScript | Lang::JavaScript => resolve_relative_js(from_file, module, known),
        Lang::Python => resolve_python(from_file, module, known),
        Lang::Rust => resolve_rust(module, known),
        _ => None,
    }
}

/// The directory portion of a project-relative file path (`""` for a top-level
/// file), split into components.
fn dir_parts(file: &str) -> Vec<String> {
    let mut parts: Vec<String> = file.split('/').map(|s| s.to_string()).collect();
    parts.pop(); // drop the file name
    parts
}

/// Normalize a component list, resolving `.` and `..` in place. Leading `..`
/// that escape the root are kept (they simply won't match a known file).
fn normalize(parts: Vec<String>) -> String {
    let mut out: Vec<String> = Vec::new();
    for p in parts {
        match p.as_str() {
            "" | "." => {}
            ".." => {
                if out.last().map(|s| s != "..").unwrap_or(false) {
                    out.pop();
                } else {
                    out.push(p);
                }
            }
            _ => out.push(p),
        }
    }
    out.join("/")
}

/// JS/TS relative import (`./x`, `../x`): resolve against the importer's dir and
/// try the usual extension/`index` candidates. Bare specifiers (node_modules)
/// return `None`.
fn resolve_relative_js(from_file: &str, module: &str, known: &HashSet<String>) -> Option<String> {
    if !module.starts_with('.') {
        return None;
    }
    let mut base = dir_parts(from_file);
    base.extend(module.split('/').map(|s| s.to_string()));
    let stem = normalize(base);
    const EXTS: &[&str] = &["ts", "tsx", "js", "jsx", "mts", "cts", "mjs", "cjs"];
    // Exact path first (module already had an extension), then <stem>.<ext>, then
    // <stem>/index.<ext>.
    if known.contains(&stem) {
        return Some(stem);
    }
    for ext in EXTS {
        let cand = format!("{stem}.{ext}");
        if known.contains(&cand) {
            return Some(cand);
        }
    }
    for ext in EXTS {
        let cand = format!("{stem}/index.{ext}");
        if known.contains(&cand) {
            return Some(cand);
        }
    }
    None
}

/// Python import resolution. Absolute dotted (`a.b.c`) → `a/b/c.py` or
/// `a/b/c/__init__.py`. Relative (`.mod`, `..pkg.mod`) resolves the leading dots
/// against the importer's package directory.
fn resolve_python(from_file: &str, module: &str, known: &HashSet<String>) -> Option<String> {
    let leading_dots = module.chars().take_while(|c| *c == '.').count();
    let rest = &module[leading_dots..];
    let rest_parts: Vec<String> = if rest.is_empty() {
        Vec::new()
    } else {
        rest.split('.').map(|s| s.to_string()).collect()
    };
    let mut base: Vec<String> = if leading_dots == 0 {
        Vec::new()
    } else {
        // One dot = the importer's own package dir; each extra dot goes up one.
        let mut d = dir_parts(from_file);
        for _ in 0..leading_dots.saturating_sub(1) {
            d.pop();
        }
        d
    };
    base.extend(rest_parts);
    let stem = normalize(base);
    if stem.is_empty() {
        return None;
    }
    [format!("{stem}.py"), format!("{stem}/__init__.py")]
        .into_iter()
        .find(|cand| known.contains(cand))
}

/// Rust import resolution for in-crate paths only. `crate::a::b` → `src/a/b.rs`
/// or `src/a/b/mod.rs`. External crates (`std::`, `serde::`, …) and `super`/
/// `self`-relative paths return `None` (dropped).
fn resolve_rust(module: &str, known: &HashSet<String>) -> Option<String> {
    let rest = module.strip_prefix("crate::")?;
    let segs: Vec<&str> = rest.split("::").collect();
    if segs.is_empty() {
        return None;
    }
    // Drop a trailing item that's likely a symbol, then a leaf module — try both
    // the full path as a module and one segment shorter.
    for take in [segs.len(), segs.len().saturating_sub(1)] {
        if take == 0 {
            continue;
        }
        let joined = segs[..take].join("/");
        for cand in [format!("src/{joined}.rs"), format!("src/{joined}/mod.rs")] {
            if known.contains(&cand) {
                return Some(cand);
            }
        }
    }
    None
}

/// Strongly-connected components of a directed graph given as an adjacency list
/// (Tarjan's algorithm, iterative to avoid deep recursion on large graphs).
fn tarjan_sccs(adj: &[Vec<usize>]) -> Vec<Vec<usize>> {
    let n = adj.len();
    let mut index = vec![usize::MAX; n];
    let mut low = vec![0usize; n];
    let mut on_stack = vec![false; n];
    let mut stack: Vec<usize> = Vec::new();
    let mut sccs: Vec<Vec<usize>> = Vec::new();
    let mut next_index = 0usize;

    // Explicit DFS stack of (node, next-child-cursor).
    let mut call: Vec<(usize, usize)> = Vec::new();
    for start in 0..n {
        if index[start] != usize::MAX {
            continue;
        }
        call.push((start, 0));
        while let Some(&(v, ci)) = call.last() {
            if ci == 0 {
                index[v] = next_index;
                low[v] = next_index;
                next_index += 1;
                stack.push(v);
                on_stack[v] = true;
            }
            if ci < adj[v].len() {
                let w = adj[v][ci];
                call.last_mut().unwrap().1 += 1;
                if index[w] == usize::MAX {
                    call.push((w, 0));
                } else if on_stack[w] {
                    low[v] = low[v].min(index[w]);
                }
            } else {
                if low[v] == index[v] {
                    let mut comp = Vec::new();
                    loop {
                        let w = stack.pop().unwrap();
                        on_stack[w] = false;
                        comp.push(w);
                        if w == v {
                            break;
                        }
                    }
                    sccs.push(comp);
                }
                call.pop();
                if let Some(&(parent, _)) = call.last() {
                    low[parent] = low[parent].min(low[v]);
                }
            }
        }
    }
    sccs
}

/// Truncate a string to at most `n` characters (on a char boundary), adding an
/// ellipsis when it was cut.
fn truncate_chars(s: &str, n: usize) -> String {
    match s.char_indices().nth(n) {
        Some((idx, _)) => {
            let mut out = s[..idx].to_string();
            out.push('…');
            out
        }
        None => s.to_string(),
    }
}

/// The one-file fixture every suite under `graph::index` parses: two functions
/// (one calling the other) and a struct, which is the smallest source that
/// produces a symbol, a call edge and a doc chunk at once.
///
/// Module-level rather than inside this file's `mod tests` so the submodule
/// suites can reach it (V42 R13): a descendant module sees an ancestor's
/// private items, and one shared fixture beats a copy per file.
#[cfg(test)]
const SRC: &str = r#"
/// Adds two numbers.
pub fn add(a: i32, b: i32) -> i32 { helper(a) + b }
fn helper(x: i32) -> i32 { x * 2 }
pub struct Point { x: i32 }
"#;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::{parse_file, Lang};

    #[test]
    fn index_and_find_roundtrip() {
        let dir = std::env::temp_dir().join(format!("ckg-test-{}", uuid::Uuid::new_v4()));
        let idx = GraphIndex::open(&dir, ".ckg").expect("open");

        let fg = parse_file("src/geo.rs", SRC, Lang::Rust);
        idx.index_file_graph(&fg).expect("index");

        let hits = idx.find_symbol("add").expect("find");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].name, "add");
        assert_eq!(hits[0].kind, "function");
        assert_eq!(hits[0].file, "src/geo.rs");
        assert!(hits[0].signature.contains("fn add"));

        // Re-indexing the same file is idempotent (no duplicate rows).
        idx.index_file_graph(&fg).expect("reindex");
        assert_eq!(idx.find_symbol("add").expect("find2").len(), 1);

        // V11 Phase A: end_line is projected and covers the body.
        assert!(hits[0].end_line >= hits[0].start_line);
        // symbol_at resolves the smallest enclosing definition for a body line.
        let at = idx
            .symbol_at("src/geo.rs", hits[0].start_line)
            .expect("symbol_at");
        assert_eq!(at.as_ref().map(|s| s.name.as_str()), Some("add"));
        // The blank first line encloses no definition.
        assert!(idx
            .symbol_at("src/geo.rs", 1)
            .expect("symbol_at blank")
            .is_none());
        // callers_count: add calls helper → helper has ≥1 caller; add has none.
        assert!(idx.callers_count("helper").expect("cc helper") >= 1);
        assert_eq!(idx.callers_count("add").expect("cc add"), 0);
        // stored_file_hash returns the indexed content hash.
        assert_eq!(
            idx.stored_file_hash("src/geo.rs").expect("hash"),
            Some(fg.hash.clone())
        );

        let stats = idx.stats().expect("stats");
        assert_eq!(stats.files, 1);
        assert!(stats.symbols >= 3);
        assert!(stats.edges >= 1);

        drop(idx);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn shortest_path_traces_cross_file_call_chain() {
        let dir = std::env::temp_dir().join(format!("ckg-path-{}", uuid::Uuid::new_v4()));
        let idx = GraphIndex::open(&dir, ".ckg").expect("open");
        idx.index_file_graph(&parse_file("src/a.rs", "pub fn a() { b(); }\n", Lang::Rust))
            .unwrap();
        idx.index_file_graph(&parse_file("src/b.rs", "pub fn b() { c(); }\n", Lang::Rust))
            .unwrap();
        idx.index_file_graph(&parse_file("src/c.rs", "pub fn c() {}\n", Lang::Rust))
            .unwrap();

        let kinds = [EdgeKind::Call, EdgeKind::Import, EdgeKind::Contains];
        let hit = idx
            .shortest_path("a", "c", &kinds, 8, false)
            .unwrap()
            .expect("path a→c");
        let labels: Vec<&str> = hit.nodes.iter().map(|n| n.label.as_str()).collect();
        assert_eq!(labels, vec!["a", "b", "c"]);
        assert_eq!(hit.hops, 2);
        // Cross-file calls resolve name-only → each hop is Inferred.
        assert_eq!(hit.nodes[0].edge_to_next.as_deref(), Some("call"));
        assert_eq!(hit.nodes[0].confidence, Some(Confidence::Inferred));
        assert_eq!(hit.equal_alternatives, 0);

        // Directed: no reverse path within bound; symmetric finds it.
        assert!(idx
            .shortest_path("c", "a", &kinds, 8, false)
            .unwrap()
            .is_none());
        assert!(idx
            .shortest_path("c", "a", &kinds, 8, true)
            .unwrap()
            .is_some());
        // Unresolvable endpoint → None.
        assert!(idx
            .shortest_path("a", "nope", &kinds, 8, false)
            .unwrap()
            .is_none());
        // Bound too small → None.
        assert!(idx
            .shortest_path("a", "c", &kinds, 1, false)
            .unwrap()
            .is_none());
        drop(idx);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn file_centrality_counts_distinct_edges_not_join_cardinality() {
        let dir = std::env::temp_dir().join(format!("ckg-cent-{}", uuid::Uuid::new_v4()));
        let idx = GraphIndex::open(&dir, ".ckg").expect("open");
        // `run` is defined twice in one file; a single inbound call must count
        // ONCE for that file, not once per matching definition (the old
        // count-over-join bug double-counted files with recurring method names).
        idx.index_file_graph(&parse_file(
            "src/dup.rs",
            "pub fn run() {}\npub fn run() {}\n",
            Lang::Rust,
        ))
        .expect("index dup");
        idx.index_file_graph(&parse_file(
            "src/caller.rs",
            "pub fn c() { run() }\n",
            Lang::Rust,
        ))
        .expect("index caller");

        let central = idx.file_centrality(10).expect("centrality");
        let dup = central
            .iter()
            .find(|(f, _)| f == "src/dup.rs")
            .map(|(_, c)| *c)
            .unwrap_or(0);
        assert_eq!(
            dup, 1,
            "one call edge counts once despite two same-named defs"
        );

        drop(idx);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn query_api_callers_callees_refs_docs() {
        let dir = std::env::temp_dir().join(format!("ckg-q-{}", uuid::Uuid::new_v4()));
        let idx = GraphIndex::open(&dir, ".ckg").expect("open");
        idx.index_file_graph(&parse_file("src/geo.rs", SRC, Lang::Rust))
            .expect("index");

        // add() calls helper(); so add is a caller of helper, and helper is a callee of add.
        let callers = idx.callers("helper").expect("callers");
        assert!(callers.iter().any(|s| s.name == "add"));

        let callees = idx.callees("add").expect("callees");
        assert!(callees.iter().any(|s| s.name == "helper"));

        // helper is referenced at least once (the call site in add).
        let refs = idx.references("helper").expect("refs");
        assert!(!refs.is_empty());

        // outline lists the file's defs in line order.
        let outline = idx.outline("src/geo.rs").expect("outline");
        assert!(outline.iter().any(|s| s.name == "add"));
        assert!(outline
            .windows(2)
            .all(|w| w[0].start_line <= w[1].start_line));

        // transitive: add -> helper (forward), and helper <- add (backward).
        let reach = idx.transitive("add", true).expect("transitive");
        assert!(reach.contains(&"helper".to_string()));
        let back = idx.transitive("helper", false).expect("transitive back");
        assert!(
            back.contains(&"add".to_string()),
            "add transitively calls helper"
        );

        // doc search finds the "Adds two numbers." chunk.
        let docs = idx.search_docs("numbers", 10, 200).expect("docs");
        assert!(docs.iter().any(|d| d.anchor == "add"));

        drop(idx);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn dependents_transitive_finds_callers_at_min_depth() {
        // c() calls b(), b() calls a(). Changing a() should surface b() at
        // depth 1 and c() at depth 2 — the fixture from the IMPL-PLAN.
        let dir = std::env::temp_dir().join(format!("ckg-dep-{}", uuid::Uuid::new_v4()));
        let idx = GraphIndex::open(&dir, ".ckg").expect("open");
        idx.index_file_graph(&parse_file(
            "src/chain.rs",
            "pub fn a() {}\npub fn b() { a() }\npub fn c() { b() }\n",
            Lang::Rust,
        ))
        .expect("index");

        let hits = idx
            .dependents_transitive(&["a".to_string()], 3, 100, None)
            .expect("dependents");
        assert_eq!(hits.len(), 2, "{hits:?}");
        let b = hits
            .iter()
            .find(|h| h.symbol.name == "b")
            .expect("b present");
        let c = hits
            .iter()
            .find(|h| h.symbol.name == "c")
            .expect("c present");
        assert_eq!(b.depth, 1);
        assert_eq!(c.depth, 2);
        assert!(
            b.approx && c.approx,
            "every hit is approximate by construction"
        );
        // Sorted by (depth, name): b (depth 1) before c (depth 2).
        assert_eq!(hits[0].symbol.name, "b");
        assert_eq!(hits[1].symbol.name, "c");

        drop(idx);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn dependents_transitive_respects_depth_cap_and_max_and_roots() {
        let dir = std::env::temp_dir().join(format!("ckg-depcap-{}", uuid::Uuid::new_v4()));
        let idx = GraphIndex::open(&dir, ".ckg").expect("open");
        idx.index_file_graph(&parse_file(
            "src/chain.rs",
            "pub fn a() {}\npub fn b() { a() }\npub fn c() { b() }\n",
            Lang::Rust,
        ))
        .expect("index");

        // depth=1 only reaches b, not c.
        let capped = idx
            .dependents_transitive(&["a".to_string()], 1, 100, None)
            .expect("dependents");
        assert_eq!(capped.len(), 1);
        assert_eq!(capped[0].symbol.name, "b");

        // max=1 truncates even though depth would reach both.
        let truncated = idx
            .dependents_transitive(&["a".to_string()], 6, 1, None)
            .expect("dependents");
        assert_eq!(truncated.len(), 1);

        // A depth passed as 0 (or absurdly high) is clamped into 1..=6, not
        // rejected — 0 still finds the direct caller.
        let clamped = idx
            .dependents_transitive(&["a".to_string()], 0, 100, None)
            .expect("dependents");
        assert_eq!(clamped.len(), 1);
        assert_eq!(clamped[0].symbol.name, "b");

        // Empty roots is an empty result, not a query error.
        assert!(idx
            .dependents_transitive(&[], 3, 100, None)
            .expect("dependents")
            .is_empty());

        // A root that's also a caller of another root doesn't get reported as
        // its own dependent.
        let both_roots = idx
            .dependents_transitive(&["a".to_string(), "b".to_string()], 3, 100, None)
            .expect("dependents");
        assert!(!both_roots
            .iter()
            .any(|h| h.symbol.name == "a" || h.symbol.name == "b"));
        assert!(both_roots.iter().any(|h| h.symbol.name == "c"));

        drop(idx);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn tests_for_finds_test_two_hops_up_and_excludes_non_test_caller() {
        // one() <- two() <- test_it() (a #[test] fn); one() is also called
        // directly by a plain (non-test) fn, which must NOT show up.
        let dir = std::env::temp_dir().join(format!("ckg-testsfor-{}", uuid::Uuid::new_v4()));
        let idx = GraphIndex::open(&dir, ".ckg").expect("open");
        let src = "pub fn one() {}\npub fn two() { one() }\n#[test]\nfn test_it() { two() }\npub fn plain_caller() { one() }\n";
        idx.index_file_graph(&parse_file("src/chain.rs", src, Lang::Rust))
            .expect("index");

        let hits = idx
            .tests_for(&["one".to_string()], 3, 100)
            .expect("tests_for");
        assert_eq!(hits.len(), 1, "{hits:?}");
        assert_eq!(hits[0].name, "test_it");
        assert!(hits[0].is_test);
        assert!(
            !hits.iter().any(|s| s.name == "plain_caller"),
            "non-test caller excluded: {hits:?}"
        );
        assert!(
            !hits.iter().any(|s| s.name == "two"),
            "the intermediate non-test hop is excluded: {hits:?}"
        );

        // Depth 1 only reaches `two` (not a test), so no tests found yet.
        let shallow = idx
            .tests_for(&["one".to_string()], 1, 100)
            .expect("tests_for");
        assert!(shallow.is_empty(), "{shallow:?}");

        drop(idx);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn remove_file_drops_ghost_inbound_call_edges() {
        let dir = std::env::temp_dir().join(format!("ckg-ghost-{}", uuid::Uuid::new_v4()));
        let idx = GraphIndex::open(&dir, ".ckg").expect("open");
        // a.rs defines `baz`; b.rs's `bar` calls it.
        idx.index_file_graph(&parse_file("src/a.rs", "pub fn baz() {}\n", Lang::Rust))
            .expect("index a");
        idx.index_file_graph(&parse_file(
            "src/b.rs",
            "pub fn bar() { baz() }\n",
            Lang::Rust,
        ))
        .expect("index b");
        assert!(idx.callers("baz").unwrap().iter().any(|s| s.name == "bar"));

        // Delete a.rs: with `baz` gone, the inbound call edge from `bar` must
        // not survive to report a ghost caller of a non-existent symbol.
        idx.remove_file("src/a.rs").expect("remove");
        assert!(idx.find_symbol("baz").unwrap().is_empty());
        assert!(
            idx.callers("baz").unwrap().is_empty(),
            "no ghost callers of a deleted symbol"
        );

        drop(idx);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn reindex_keeps_inbound_call_edges_from_other_files() {
        // Regression: the per-file replace in `index_file_graph` used to run
        // the dangling-name purge BEFORE re-inserting the file's symbols, so
        // any ordinary edit of a file deleted the inbound call edges owned by
        // its unchanged callers until they were themselves re-indexed.
        let dir = std::env::temp_dir().join(format!("ckg-reidx-{}", uuid::Uuid::new_v4()));
        let idx = GraphIndex::open(&dir, ".ckg").expect("open");
        idx.index_file_graph(&parse_file("src/a.rs", "pub fn baz() {}\n", Lang::Rust))
            .expect("index a");
        idx.index_file_graph(&parse_file(
            "src/b.rs",
            "pub fn bar() { baz() }\n",
            Lang::Rust,
        ))
        .expect("index b");
        assert!(idx.callers("baz").unwrap().iter().any(|s| s.name == "bar"));

        // Re-index a.rs (an ordinary edit): `baz` is still defined, so bar's
        // inbound call edge must survive.
        idx.index_file_graph(&parse_file(
            "src/a.rs",
            "pub fn baz() {}\npub fn qux() {}\n",
            Lang::Rust,
        ))
        .expect("reindex a");
        assert!(
            idx.callers("baz").unwrap().iter().any(|s| s.name == "bar"),
            "re-indexing the definer must not drop inbound call edges from unchanged callers"
        );

        // Re-index a.rs with `baz` genuinely removed: now the ghost cleanup
        // must still fire, exactly like the delete path.
        idx.index_file_graph(&parse_file("src/a.rs", "pub fn qux() {}\n", Lang::Rust))
            .expect("reindex a without baz");
        assert!(
            idx.callers("baz").unwrap().is_empty(),
            "no ghost callers of a symbol the edit removed"
        );

        drop(idx);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn code_chunk_emission_respects_kind_and_size_filters() {
        // const → not a "shaped" kind, never chunked. short() → a shaped kind
        // but a 1-line span, below the 3-line floor. long_fn → shaped + a
        // 4-line span, so it earns a chunk whose text carries the body.
        let src = "pub const N: i32 = 1;\n\
                   pub fn short() { 1 }\n\
                   pub fn long_fn(a: i32, b: i32) -> i32 {\n    let c = a + b;\n    c * 2\n}\n";
        let fg = parse_file("src/a.rs", src, Lang::Rust);
        let ids: Vec<&str> = fg.code_chunks.iter().map(|c| c.id.as_str()).collect();
        assert!(
            !ids.iter().any(|i| i.contains("N@")),
            "const not chunked: {ids:?}"
        );
        assert!(
            !ids.iter().any(|i| i.contains("short@")),
            "1-line fn not chunked: {ids:?}"
        );
        let chunk = fg
            .code_chunks
            .iter()
            .find(|c| c.id.contains("long_fn@"))
            .expect("long_fn chunked");
        assert_eq!(chunk.file, "src/a.rs");
        assert!(
            chunk.text.contains("let c = a + b"),
            "body present: {}",
            chunk.text
        );
    }

    #[test]
    fn stale_schema_is_refused_by_open_existing_and_migrated_by_open() {
        let dir = std::env::temp_dir().join(format!("ckg-migr-{}", uuid::Uuid::new_v4()));
        let idx = GraphIndex::open(&dir, ".ckg").expect("open");
        idx.index_file_graph(&parse_file("src/a.rs", "pub fn f() {}\n", Lang::Rust))
            .expect("index");
        assert!(!idx.find_symbol("f").unwrap().is_empty());
        // Simulate an older on-disk schema generation.
        idx.write_schema_version(1).expect("downgrade");
        drop(idx);

        // Read-only consumers must REFUSE a stale store (not silently wipe it).
        let refused = GraphIndex::open_existing(&dir, ".ckg");
        assert!(
            matches!(refused, Err(AppError::GraphNotReady(_))),
            "open_existing must refuse a stale store with GraphNotReady"
        );
        // The data is still intact (open_existing didn't reset it).
        {
            let peek = GraphIndex::open_db(&dir, ".ckg").expect("peek");
            assert!(
                !peek.find_symbol("f").unwrap().is_empty(),
                "open_existing must not wipe"
            );
        }

        // The writable path migrates: resets (empties) + flags exactly once.
        let idx2 = GraphIndex::open(&dir, ".ckg").expect("reopen");
        assert!(idx2.take_schema_reset(), "migration flags a rebuild");
        assert!(!idx2.take_schema_reset(), "flag is one-shot");
        assert!(
            idx2.find_symbol("f").unwrap().is_empty(),
            "stale rows were reset"
        );

        drop(idx2);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn dead_exports_flags_only_unused_public_symbols() {
        let dir = std::env::temp_dir().join(format!("ckg-dead-{}", uuid::Uuid::new_v4()));
        let idx = GraphIndex::open(&dir, ".ckg").expect("open");
        // used_pub is called by driver; unused_pub is not; priv_fn is private.
        let src = "pub fn used_pub() {}\n\
                   pub fn unused_pub() {}\n\
                   fn priv_fn() {}\n\
                   pub fn driver() { used_pub() }\n";
        idx.index_file_graph(&parse_file("src/a.rs", src, Lang::Rust))
            .expect("index");

        let dead = idx.dead_exports(100).expect("dead");
        let names: Vec<&str> = dead.iter().map(|s| s.name.as_str()).collect();
        assert!(
            names.contains(&"unused_pub"),
            "unused public fn is a candidate"
        );
        assert!(!names.contains(&"used_pub"), "a called fn is not dead");
        assert!(
            !names.contains(&"priv_fn"),
            "a private fn is never a dead export"
        );
        // driver() is public + unused, but it *does* reference used_pub, so it's
        // not itself referenced — it should appear (honest candidate).
        assert!(names.contains(&"driver"));

        drop(idx);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn import_cycles_finds_a_two_file_loop() {
        let dir = std::env::temp_dir().join(format!("ckg-cyc-{}", uuid::Uuid::new_v4()));
        let idx = GraphIndex::open(&dir, ".ckg").expect("open");
        // a.ts imports ./b, b.ts imports ./a → a 2-file cycle.
        idx.index_file_graph(&parse_file(
            "src/a.ts",
            "import { x } from './b';\nexport const y = 1;\n",
            Lang::TypeScript,
        ))
        .expect("a");
        idx.index_file_graph(&parse_file(
            "src/b.ts",
            "import { y } from './a';\nexport const x = 1;\n",
            Lang::TypeScript,
        ))
        .expect("b");

        let cycles = idx.import_cycles(50).expect("cycles");
        assert_eq!(cycles.len(), 1, "exactly one cycle");
        let mut c = cycles[0].clone();
        c.sort();
        assert_eq!(c, vec!["src/a.ts".to_string(), "src/b.ts".to_string()]);

        drop(idx);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn import_cycles_none_for_acyclic_graph() {
        let dir = std::env::temp_dir().join(format!("ckg-acyc-{}", uuid::Uuid::new_v4()));
        let idx = GraphIndex::open(&dir, ".ckg").expect("open");
        idx.index_file_graph(&parse_file(
            "src/a.ts",
            "import { x } from './b';\n",
            Lang::TypeScript,
        ))
        .expect("a");
        idx.index_file_graph(&parse_file(
            "src/b.ts",
            "export const x = 1;\n",
            Lang::TypeScript,
        ))
        .expect("b");
        assert!(idx.import_cycles(50).expect("cycles").is_empty());
        // An unresolvable/external import must not crash.
        idx.index_file_graph(&parse_file(
            "src/c.ts",
            "import fs from 'node:fs';\n",
            Lang::TypeScript,
        ))
        .expect("c");
        assert!(idx.import_cycles(50).expect("cycles2").is_empty());

        drop(idx);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn remove_file_drops_all_rows_for_a_path() {
        let dir = std::env::temp_dir().join(format!("ckg-rm-{}", uuid::Uuid::new_v4()));
        let idx = GraphIndex::open(&dir, ".ckg").expect("open");
        idx.index_file_graph(&parse_file("src/geo.rs", SRC, Lang::Rust))
            .expect("index");
        assert!(!idx.find_symbol("add").unwrap().is_empty());
        let before = idx.stats().unwrap();
        assert_eq!(before.files, 1);

        // The watcher's delete path: remove every row for the file.
        idx.remove_file("src/geo.rs").expect("remove");
        assert!(idx.find_symbol("add").unwrap().is_empty());
        assert!(idx.references("helper").unwrap().is_empty());
        let after = idx.stats().unwrap();
        assert_eq!(after.files, 0);
        assert_eq!(after.symbols, 0);
        assert_eq!(after.edges, 0);

        drop(idx);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
