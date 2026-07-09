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

use super::memory::{
    MemNote, ProjectFact, SessionInfo, SessionUsageRow, TurnUsage, UsageEvent, UsageTotals,
    WorkingSetEntry, MAX_EVENTS_PER_SESSION, MAX_LIVE_PROJECT_FACTS, MAX_SESSIONS_PER_ROOT,
    MAX_USAGE_PER_SESSION,
};
use super::model::{Confidence, FileGraph, Lang};
use super::schema::{GRAPH_SCHEMA_VERSION, RELATIONS};

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

impl GraphIndex {
    /// Open the SQLite-backed store for `root` (creating the dir/db if needed)
    /// without touching the schema.
    fn open_db(root: &Path, db_subdir: &str) -> AppResult<GraphIndex> {
        let dir = root.join(db_subdir);
        std::fs::create_dir_all(&dir).map_err(AppError::Io)?;
        let db_path = dir.join("graph.db");
        let db = DbInstance::new("sqlite", db_path.to_string_lossy().as_ref(), Default::default())
            .map_err(|e| AppError::Graph(format!("open {}: {e}", db_path.display())))?;
        Ok(GraphIndex { db, schema_reset: AtomicBool::new(false) })
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
            index.reset()?;
            index.write_schema_version(GRAPH_SCHEMA_VERSION)?;
            if had_data {
                index.schema_reset.store(true, Ordering::Relaxed);
            }
        }
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
        Ok(rows.rows.first().and_then(|r| r.first()).map(dv_i64).unwrap_or(0) as u64)
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
        let rows = self.run(
            "::relations",
            BTreeMap::new(),
            ScriptMutability::Immutable,
        )?;
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
        self.with_write_txn(|tx| remove_file_in_tx(tx, file))
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
            remove_file_in_tx(tx, &file)?;
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
            Ok(())
        })
    }

    /// Run `f` inside a single write transaction: commit on `Ok`, abort on
    /// `Err`. Lets a multi-step mutation (the per-file replace) be all-or-nothing
    /// rather than a sequence of independently-committed scripts that can leave a
    /// half-written file graph behind.
    fn with_write_txn<T>(
        &self,
        f: impl FnOnce(&MultiTransaction) -> AppResult<T>,
    ) -> AppResult<T> {
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
                *symbol{id, name, kind, file, start_line, signature, visibility, end_line, is_test}, name == $name",
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
            "?[name, file, line, col, conf] := *ref{name, file, line, col, confidence: conf}, name == $name\n:limit 1000",
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
        let idx_of: std::collections::HashMap<&str, usize> =
            files.iter().enumerate().map(|(i, f)| (f.as_str(), i)).collect();
        let mut adj: Vec<Vec<usize>> = vec![Vec::new(); files.len()];
        for r in &import_rows.rows {
            let src = cell_str(r, 0);
            let module = cell_str(r, 1);
            let Some(&si) = idx_of.get(src.as_str()) else { continue };
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
            .map(|scc| scc.into_iter().map(|i| files[i].clone()).collect::<Vec<_>>())
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
        Ok(rows.rows.first().and_then(|r| r.first()).map(dv_i64).unwrap_or(0) as u64)
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
        Ok(rows.rows.first().and_then(|r| r.first()).map(dv_i64).unwrap_or(0) as u64)
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
:limit 1000"#
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
    pub fn dependents_transitive(
        &self,
        roots: &[String],
        depth: u32,
        max: usize,
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
        let mut frontier: HashMap<String, Confidence> =
            roots.iter().map(|n| (n.clone(), Confidence::Extracted)).collect();
        for d in 1..=depth {
            let mut next_frontier: HashMap<String, Confidence> = HashMap::new();
            for (name, chain_conf) in &frontier {
                let Some(callers) = rev.get(name) else { continue };
                for (caller, ec) in callers {
                    if root_set.contains(caller.as_str()) {
                        continue;
                    }
                    // First discovery (BFS order) wins the min depth; its chain
                    // confidence is the weakest link from a root to here.
                    if !best.contains_key(caller) {
                        let cc = chain_conf.weaker(*ec);
                        best.insert(caller.clone(), (d, cc));
                        next_frontier.insert(caller.clone(), cc);
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
                    hits.push(DependentHit { symbol, depth: d, approx: true, confidence: conf });
                    if hits.len() >= max {
                        return Ok(hits);
                    }
                }
            }
        }
        Ok(hits)
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
            .dependents_transitive(roots, depth, max)?
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

    // ── Semantic search (Phase G): vector store + epoch + k-NN ───────────
    //
    // The vector store is created lazily because its column type bakes in the
    // dimension (`<F32; N>`), which we only know once an embedder is configured
    // (or auto-probed). Vectors are keyed by `(chunk_id, epoch)` so a model
    // change (same dim) can keep old vectors alongside new; a dim change forces
    // a drop+recreate (handled in `ensure_vector_store`). Each vector also
    // stores its chunk's content hash, so a re-indexed chunk whose text changed
    // is detected as needing a fresh embedding.

    /// Ensure the `doc_vec` relation + HNSW index exist sized for `dim`, and
    /// stamp `model`/`dim`/`epoch` into the `embed_meta` singleton. If a store
    /// exists at a DIFFERENT dim, it's dropped and recreated (returns `true` so
    /// the caller knows the old vectors are gone and a full re-embed is due).
    pub fn ensure_vector_store(&self, dim: usize, model: &str, epoch: &str) -> AppResult<bool> {
        self.ensure_meta_relation()?;
        let existing_dim = self.stored_dim()?;
        let mut reset = false;
        let have_doc_vec = self.existing_relations()?.contains("doc_vec");

        if !have_doc_vec || existing_dim != Some(dim) {
            if have_doc_vec {
                // A dim change: drop the store (its HNSW index must go first —
                // CozoDB refuses to remove a relation with indices attached).
                self.drop_vector_store()?;
                reset = true;
            }
            self.run_mut(
                &format!("?[chunk_id, epoch, hash, vec] <- []\n:create doc_vec {{chunk_id: String, epoch: String => hash: String, vec: <F32; {dim}>}}"),
                BTreeMap::new(),
            )?;
            self.run_mut(
                &format!(
                    "::hnsw create doc_vec:vec_idx {{dim: {dim}, m: 16, dtype: F32, fields: [vec], distance: Cosine, ef_construction: 50}}"
                ),
                BTreeMap::new(),
            )?;
        }

        // Upsert the singleton meta row.
        let mut p = BTreeMap::new();
        p.insert("model".to_string(), DataValue::Str(model.into()));
        p.insert("dim".to_string(), int(dim as u32));
        p.insert("epoch".to_string(), DataValue::Str(epoch.into()));
        self.run_mut(
            "?[id, model, dim, epoch] <- [['1', $model, $dim, $epoch]]\n:put embed_meta {id => model, dim, epoch}",
            p,
        )?;
        Ok(reset)
    }

    /// Drop the vector store (and its HNSW index) so the next backfill
    /// re-embeds everything from scratch. Used by "Rebuild embeddings" / a
    /// silent model swap behind the same name. No-op if there's no store.
    pub fn clear_vectors(&self) -> AppResult<()> {
        if self.existing_relations()?.contains("doc_vec") {
            self.drop_vector_store()?;
        }
        Ok(())
    }

    /// Remove the `doc_vec` relation, dropping its HNSW index first. CozoDB
    /// refuses to `::remove` a relation that still has an index attached, so the
    /// index must go first. The index drop is best-effort — ignored if it's
    /// absent (a partially-created store), leaving the relation removal to
    /// surface any real error.
    fn drop_vector_store(&self) -> AppResult<()> {
        let _ = self.run_mut("::index drop doc_vec:vec_idx", BTreeMap::new());
        self.run_mut("::remove doc_vec", BTreeMap::new())?;
        Ok(())
    }

    /// Delete vectors whose `doc_chunk` no longer exists — chunks dropped by a
    /// file delete/rename or replaced under a new anchor when a file changed.
    /// Without this, orphaned vectors keep being counted as "embedded" (so
    /// coverage can read >100% and suppress backfill) and linger in the HNSW
    /// index as dead candidates. Keeps every still-valid vector, so it never
    /// forces a needless re-embed. No-op when there's no vector store. Returns
    /// the number of orphans removed.
    pub fn prune_orphan_vectors(&self) -> AppResult<u64> {
        if !self.existing_relations()?.contains("doc_vec") {
            return Ok(0);
        }
        // Count first — a `:rm` returns a status row, not the deleted rows.
        let n = {
            let rows = self.run(
                "?[count(chunk_id)] := *doc_vec{chunk_id, epoch}, not *doc_chunk{id: chunk_id}",
                BTreeMap::new(),
                ScriptMutability::Immutable,
            )?;
            rows.rows.first().and_then(|r| r.first()).map(dv_i64).unwrap_or(0) as u64
        };
        if n > 0 {
            self.run_mut(
                "?[chunk_id, epoch] := *doc_vec{chunk_id, epoch}, not *doc_chunk{id: chunk_id}\n\
                 :rm doc_vec {chunk_id, epoch}",
                BTreeMap::new(),
            )?;
        }
        Ok(n)
    }

    /// V11 Phase F — a cached local-model digest for `(file, content_hash)`, or
    /// `None` on a miss (stale hash / never computed).
    pub fn get_digest(&self, file: &str, content_hash: &str) -> AppResult<Option<String>> {
        let mut p = BTreeMap::new();
        p.insert("file".to_string(), DataValue::Str(file.into()));
        p.insert("hash".to_string(), DataValue::Str(content_hash.into()));
        let rows = self.run(
            "?[text] := *digest{file, content_hash, text}, file == $file, content_hash == $hash",
            p,
            ScriptMutability::Immutable,
        )?;
        Ok(rows.rows.first().map(|r| cell_str(r, 0)))
    }

    /// Upsert a computed digest for `file`, keeping at most ONE row per file.
    pub fn put_digest(&self, file: &str, content_hash: &str, text: &str, ts_ms: i64) -> AppResult<()> {
        let mut p = BTreeMap::new();
        p.insert("file".to_string(), DataValue::Str(file.into()));
        p.insert("hash".to_string(), DataValue::Str(content_hash.into()));
        // F11: drop any superseded-hash rows for this file BEFORE inserting the
        // current one. The relation is keyed `(file, content_hash)`, so without
        // this every edit left its old digest row behind forever —
        // `prune_orphan_digests` only removes rows whose FILE is gone, so a
        // still-indexed file leaked one dead row per edit and `digest_count`
        // (the coverage readout) over-reported far above the real cache state.
        self.run_mut(
            "?[file, content_hash] := *digest{file, content_hash}, file == $file, content_hash != $hash\n\
             :rm digest {file, content_hash}",
            p.clone(),
        )?;
        p.insert("text".to_string(), DataValue::Str(text.into()));
        p.insert("ts".to_string(), DataValue::Num(Num::Int(ts_ms)));
        self.run_mut(
            "?[file, content_hash, text, ts_ms] <- [[$file, $hash, $text, $ts]]\n\
             :put digest {file, content_hash => text, ts_ms}",
            p,
        )?;
        Ok(())
    }

    /// Number of cached digests, for the Context section's coverage readout.
    pub fn digest_count(&self) -> AppResult<u64> {
        if !self.existing_relations()?.contains("digest") {
            return Ok(0);
        }
        let rows = self.run(
            "?[count(file)] := *digest{file, content_hash}",
            BTreeMap::new(),
            ScriptMutability::Immutable,
        )?;
        Ok(rows.rows.first().and_then(|r| r.first()).map(dv_i64).unwrap_or(0) as u64)
    }

    /// Drop cached digests whose file is no longer indexed (mirrors the doc_vec
    /// orphan sweep).
    pub fn prune_orphan_digests(&self) -> AppResult<u64> {
        if !self.existing_relations()?.contains("digest") {
            return Ok(0);
        }
        let n = {
            let rows = self.run(
                "?[count(file)] := *digest{file, content_hash}, not *file{path: file}",
                BTreeMap::new(),
                ScriptMutability::Immutable,
            )?;
            rows.rows.first().and_then(|r| r.first()).map(dv_i64).unwrap_or(0) as u64
        };
        if n > 0 {
            self.run_mut(
                "?[file, content_hash] := *digest{file, content_hash}, not *file{path: file}\n\
                 :rm digest {file, content_hash}",
                BTreeMap::new(),
            )?;
        }
        Ok(n)
    }

    fn ensure_meta_relation(&self) -> AppResult<()> {
        if !self.existing_relations()?.contains("embed_meta") {
            self.run_mut(
                "?[id, model, dim, epoch] <- []\n:create embed_meta {id: String => model: String, dim: Int, epoch: String}",
                BTreeMap::new(),
            )?;
        }
        Ok(())
    }

    fn stored_dim(&self) -> AppResult<Option<usize>> {
        if !self.existing_relations()?.contains("embed_meta") {
            return Ok(None);
        }
        let rows = self.run(
            "?[dim] := *embed_meta{dim}",
            BTreeMap::new(),
            ScriptMutability::Immutable,
        )?;
        Ok(rows.rows.first().map(|r| cell_i64(r, 0) as usize))
    }

    /// The current embedding epoch fingerprint, or `None` if never embedded.
    pub fn current_epoch(&self) -> AppResult<Option<String>> {
        if !self.existing_relations()?.contains("embed_meta") {
            return Ok(None);
        }
        let rows = self.run(
            "?[epoch] := *embed_meta{epoch}",
            BTreeMap::new(),
            ScriptMutability::Immutable,
        )?;
        Ok(rows.rows.first().map(|r| cell_str(r, 0)))
    }

    /// Store `(chunk_id, embedding)` rows for the current `epoch`, stamping each
    /// with its chunk content hash. Vectors are plain `f32` lists.
    pub fn put_doc_vectors(
        &self,
        epoch: &str,
        items: &[(String, String, Vec<f32>)], // (chunk_id, text_hash, vector)
    ) -> AppResult<()> {
        if items.is_empty() {
            return Ok(());
        }
        let rows: Vec<DataValue> = items
            .iter()
            .map(|(id, hash, vec)| {
                let v = DataValue::List(
                    vec.iter()
                        .map(|f| DataValue::Num(Num::Float(*f as f64)))
                        .collect(),
                );
                DataValue::List(vec![
                    DataValue::Str(id.as_str().into()),
                    DataValue::Str(epoch.into()),
                    DataValue::Str(hash.as_str().into()),
                    v,
                ])
            })
            .collect();
        self.put(
            "?[chunk_id, epoch, hash, vec] <- $rows\n:put doc_vec {chunk_id, epoch => hash, vec}",
            rows,
        )
    }

    /// Doc chunks lacking a current-epoch vector whose hash matches their text
    /// — i.e. new or changed chunks that need (re-)embedding. Bounded by `limit`.
    /// Returns `(chunk_id, text_hash, text)`.
    pub fn chunks_needing_vectors(
        &self,
        epoch: &str,
        limit: usize,
    ) -> AppResult<Vec<(String, String, String)>> {
        // Current-epoch (chunk_id -> hash) already embedded.
        let mut embedded: std::collections::HashMap<String, String> =
            std::collections::HashMap::new();
        if self.existing_relations()?.contains("doc_vec") {
            let mut p = BTreeMap::new();
            p.insert("epoch".to_string(), DataValue::Str(epoch.into()));
            let rows = self.run(
                "?[chunk_id, hash] := *doc_vec{chunk_id, epoch, hash}, epoch == $epoch",
                p,
                ScriptMutability::Immutable,
            )?;
            for r in &rows.rows {
                embedded.insert(cell_str(r, 0), cell_str(r, 1));
            }
        }
        let rows = self.run(
            "?[id, text] := *doc_chunk{id, text}",
            BTreeMap::new(),
            ScriptMutability::Immutable,
        )?;
        let mut out = Vec::new();
        for r in &rows.rows {
            let id = cell_str(r, 0);
            let text = cell_str(r, 1);
            let hash = text_hash(&text);
            if embedded.get(&id) != Some(&hash) {
                out.push((id, hash, text));
                if out.len() >= limit {
                    break;
                }
            }
        }
        Ok(out)
    }

    /// k-NN semantic doc search: nearest current-epoch chunk vectors to
    /// `query_vec`, joined back to their doc text. Returns `(DocHit, distance)`
    /// ascending by distance. Empty when there's no vector store/epoch.
    pub fn semantic_doc_search(
        &self,
        query_vec: &[f32],
        epoch: &str,
        k: usize,
        max_snippet: usize,
    ) -> AppResult<Vec<(DocHit, f32)>> {
        if !self.existing_relations()?.contains("doc_vec") {
            return Ok(Vec::new());
        }
        let mut p = BTreeMap::new();
        p.insert(
            "q".to_string(),
            DataValue::List(
                query_vec
                    .iter()
                    .map(|f| DataValue::Num(Num::Float(*f as f64)))
                    .collect(),
            ),
        );
        p.insert("epoch".to_string(), DataValue::Str(epoch.into()));
        p.insert("k".to_string(), int(k as u32));
        p.insert("ef".to_string(), int((k * 10).max(50) as u32));
        let rows = self.run(
            r#"sem[chunk_id, dist] := ~doc_vec:vec_idx{chunk_id, epoch | query: q, k: $k, ef: $ef, bind_distance: dist, filter: epoch == $epoch}, q = vec($q)
?[source_path, anchor, text, dist] := sem[cid, dist], *doc_chunk{id: cid, source_path, anchor, text}
:order dist
:limit $k"#,
            p,
            ScriptMutability::Immutable,
        )?;
        Ok(rows
            .rows
            .iter()
            .map(|r| {
                (
                    DocHit {
                        source_path: cell_str(r, 0),
                        anchor: cell_str(r, 1),
                        snippet: truncate_chars(&cell_str(r, 2), max_snippet),
                    },
                    cell_f64(r, 3) as f32,
                )
            })
            .collect())
    }

    /// `(embedded_current_epoch, total_doc_chunks)` for the coverage readout.
    pub fn embedding_coverage(&self, epoch: &str) -> AppResult<(u64, u64)> {
        let total = {
            let rows = self.run(
                "?[count(id)] := *doc_chunk{id}",
                BTreeMap::new(),
                ScriptMutability::Immutable,
            )?;
            rows.rows.first().and_then(|r| r.first()).map(dv_i64).unwrap_or(0) as u64
        };
        let embedded = if self.existing_relations()?.contains("doc_vec") {
            let mut p = BTreeMap::new();
            p.insert("epoch".to_string(), DataValue::Str(epoch.into()));
            let rows = self.run(
                "?[count(chunk_id)] := *doc_vec{chunk_id, epoch}, epoch == $epoch",
                p,
                ScriptMutability::Immutable,
            )?;
            rows.rows.first().and_then(|r| r.first()).map(dv_i64).unwrap_or(0) as u64
        } else {
            0
        };
        Ok((embedded, total))
    }

    // ── Semantic *code* search (V11 Phase G) ──────────────────────────────
    //
    // The `code_chunk`/`code_vec` pair is the near-exact twin of `doc_chunk`/
    // `doc_vec` above, over symbol bodies instead of prose. It keeps its own
    // `code_embed_meta` singleton (rather than sharing `embed_meta`) so a dim
    // change is detected independently of whichever store `ensure_*` runs
    // first — sharing one singleton would have the second call see the first
    // call's just-written new dim and wrongly conclude nothing changed.

    /// Ensure the `code_vec` relation + HNSW index exist sized for `dim`, and
    /// stamp `model`/`dim`/`epoch` into the `code_embed_meta` singleton.
    /// Mirrors [`Self::ensure_vector_store`]. Returns `true` on a dim change
    /// (old code vectors dropped — a full code re-embed is due).
    pub fn ensure_code_vector_store(&self, dim: usize, model: &str, epoch: &str) -> AppResult<bool> {
        self.ensure_code_meta_relation()?;
        let existing_dim = self.stored_code_dim()?;
        let mut reset = false;
        let have_code_vec = self.existing_relations()?.contains("code_vec");

        if !have_code_vec || existing_dim != Some(dim) {
            if have_code_vec {
                self.drop_code_vector_store()?;
                reset = true;
            }
            self.run_mut(
                &format!("?[chunk_id, epoch, hash, vec] <- []\n:create code_vec {{chunk_id: String, epoch: String => hash: String, vec: <F32; {dim}>}}"),
                BTreeMap::new(),
            )?;
            self.run_mut(
                &format!(
                    "::hnsw create code_vec:vec_idx {{dim: {dim}, m: 16, dtype: F32, fields: [vec], distance: Cosine, ef_construction: 50}}"
                ),
                BTreeMap::new(),
            )?;
        }

        let mut p = BTreeMap::new();
        p.insert("model".to_string(), DataValue::Str(model.into()));
        p.insert("dim".to_string(), int(dim as u32));
        p.insert("epoch".to_string(), DataValue::Str(epoch.into()));
        self.run_mut(
            "?[id, model, dim, epoch] <- [['1', $model, $dim, $epoch]]\n:put code_embed_meta {id => model, dim, epoch}",
            p,
        )?;
        Ok(reset)
    }

    /// Drop the `code_vec` store, forcing the next backfill to re-embed every
    /// code chunk from scratch. Mirrors [`Self::clear_vectors`]. No-op if
    /// there's no store.
    pub fn clear_code_vectors(&self) -> AppResult<()> {
        if self.existing_relations()?.contains("code_vec") {
            self.drop_code_vector_store()?;
        }
        Ok(())
    }

    /// Remove the `code_vec` relation, dropping its HNSW index first. Mirrors
    /// [`Self::drop_vector_store`].
    fn drop_code_vector_store(&self) -> AppResult<()> {
        let _ = self.run_mut("::index drop code_vec:vec_idx", BTreeMap::new());
        self.run_mut("::remove code_vec", BTreeMap::new())?;
        Ok(())
    }

    /// Delete code vectors whose `code_chunk` no longer exists — chunks
    /// dropped by a file delete/rename or a symbol that shrank below the
    /// chunking threshold. Mirrors [`Self::prune_orphan_vectors`]. Returns the
    /// number of orphans removed.
    pub fn prune_orphan_code_vectors(&self) -> AppResult<u64> {
        if !self.existing_relations()?.contains("code_vec") {
            return Ok(0);
        }
        let n = {
            let rows = self.run(
                "?[count(chunk_id)] := *code_vec{chunk_id, epoch}, not *code_chunk{id: chunk_id}",
                BTreeMap::new(),
                ScriptMutability::Immutable,
            )?;
            rows.rows.first().and_then(|r| r.first()).map(dv_i64).unwrap_or(0) as u64
        };
        if n > 0 {
            self.run_mut(
                "?[chunk_id, epoch] := *code_vec{chunk_id, epoch}, not *code_chunk{id: chunk_id}\n\
                 :rm code_vec {chunk_id, epoch}",
                BTreeMap::new(),
            )?;
        }
        Ok(n)
    }

    fn ensure_code_meta_relation(&self) -> AppResult<()> {
        if !self.existing_relations()?.contains("code_embed_meta") {
            self.run_mut(
                "?[id, model, dim, epoch] <- []\n:create code_embed_meta {id: String => model: String, dim: Int, epoch: String}",
                BTreeMap::new(),
            )?;
        }
        Ok(())
    }

    fn stored_code_dim(&self) -> AppResult<Option<usize>> {
        if !self.existing_relations()?.contains("code_embed_meta") {
            return Ok(None);
        }
        let rows = self.run(
            "?[dim] := *code_embed_meta{dim}",
            BTreeMap::new(),
            ScriptMutability::Immutable,
        )?;
        Ok(rows.rows.first().map(|r| cell_i64(r, 0) as usize))
    }

    /// The current code-embedding epoch fingerprint, or `None` if never embedded.
    pub fn current_code_epoch(&self) -> AppResult<Option<String>> {
        if !self.existing_relations()?.contains("code_embed_meta") {
            return Ok(None);
        }
        let rows = self.run(
            "?[epoch] := *code_embed_meta{epoch}",
            BTreeMap::new(),
            ScriptMutability::Immutable,
        )?;
        Ok(rows.rows.first().map(|r| cell_str(r, 0)))
    }

    /// Store `(chunk_id, embedding)` rows for the current code `epoch`,
    /// stamping each with its chunk's content hash. Mirrors
    /// [`Self::put_doc_vectors`].
    pub fn put_code_vectors(
        &self,
        epoch: &str,
        items: &[(String, String, Vec<f32>)], // (chunk_id, text_hash, vector)
    ) -> AppResult<()> {
        if items.is_empty() {
            return Ok(());
        }
        let rows: Vec<DataValue> = items
            .iter()
            .map(|(id, hash, vec)| {
                let v = DataValue::List(
                    vec.iter()
                        .map(|f| DataValue::Num(Num::Float(*f as f64)))
                        .collect(),
                );
                DataValue::List(vec![
                    DataValue::Str(id.as_str().into()),
                    DataValue::Str(epoch.into()),
                    DataValue::Str(hash.as_str().into()),
                    v,
                ])
            })
            .collect();
        self.put(
            "?[chunk_id, epoch, hash, vec] <- $rows\n:put code_vec {chunk_id, epoch => hash, vec}",
            rows,
        )
    }

    /// Code chunks lacking a current-epoch vector whose hash matches their
    /// text — i.e. new or changed symbol bodies that need (re-)embedding.
    /// Mirrors [`Self::chunks_needing_vectors`]. Bounded by `limit`. Returns
    /// `(chunk_id, text_hash, text)`.
    pub fn pending_code_chunks(
        &self,
        epoch: &str,
        limit: usize,
    ) -> AppResult<Vec<(String, String, String)>> {
        let mut embedded: std::collections::HashMap<String, String> =
            std::collections::HashMap::new();
        if self.existing_relations()?.contains("code_vec") {
            let mut p = BTreeMap::new();
            p.insert("epoch".to_string(), DataValue::Str(epoch.into()));
            let rows = self.run(
                "?[chunk_id, hash] := *code_vec{chunk_id, epoch, hash}, epoch == $epoch",
                p,
                ScriptMutability::Immutable,
            )?;
            for r in &rows.rows {
                embedded.insert(cell_str(r, 0), cell_str(r, 1));
            }
        }
        let rows = self.run(
            "?[id, text] := *code_chunk{id, text}",
            BTreeMap::new(),
            ScriptMutability::Immutable,
        )?;
        let mut out = Vec::new();
        for r in &rows.rows {
            let id = cell_str(r, 0);
            let text = cell_str(r, 1);
            let hash = text_hash(&text);
            if embedded.get(&id) != Some(&hash) {
                out.push((id, hash, text));
                if out.len() >= limit {
                    break;
                }
            }
        }
        Ok(out)
    }

    /// k-NN semantic **code** search: nearest current-epoch code-chunk
    /// vectors to `query_vec`, joined back to the defining symbol (a code
    /// chunk's id IS its symbol's id, so this is a direct join — no separate
    /// text lookup). Returns `(SymbolHit, distance)` ascending by distance;
    /// deliberately carries no body text — callers chain into `graph_snippet`
    /// for that. Empty when there's no vector store/epoch.
    pub fn semantic_code_search(
        &self,
        query_vec: &[f32],
        epoch: &str,
        k: usize,
    ) -> AppResult<Vec<(SymbolHit, f32)>> {
        if !self.existing_relations()?.contains("code_vec") {
            return Ok(Vec::new());
        }
        let mut p = BTreeMap::new();
        p.insert(
            "q".to_string(),
            DataValue::List(
                query_vec
                    .iter()
                    .map(|f| DataValue::Num(Num::Float(*f as f64)))
                    .collect(),
            ),
        );
        p.insert("epoch".to_string(), DataValue::Str(epoch.into()));
        p.insert("k".to_string(), int(k as u32));
        p.insert("ef".to_string(), int((k * 10).max(50) as u32));
        let rows = self.run(
            r#"sem[chunk_id, dist] := ~code_vec:vec_idx{chunk_id, epoch | query: q, k: $k, ef: $ef, bind_distance: dist, filter: epoch == $epoch}, q = vec($q)
?[cid, name, kind, file, start_line, signature, visibility, end_line, dist] := sem[cid, dist], *symbol{id: cid, name, kind, file, start_line, end_line, signature, visibility}
:order dist
:limit $k"#,
            p,
            ScriptMutability::Immutable,
        )?;
        Ok(rows
            .rows
            .iter()
            .map(|r| {
                let hit = SymbolHit {
                    id: cell_str(r, 0),
                    name: cell_str(r, 1),
                    kind: cell_str(r, 2),
                    file: cell_str(r, 3),
                    start_line: cell_i64(r, 4) as u32,
                    signature: cell_str(r, 5),
                    visibility: cell_str(r, 6),
                    end_line: cell_i64(r, 7) as u32,
                    // Not projected by this query (out of Phase C's scoped 5
                    // heads) — honest default, matching `rows_to_symbols`.
                    is_test: false,
                    confidence: None,
                };
                (hit, cell_f64(r, 8) as f32)
            })
            .collect())
    }

    /// `(embedded_current_epoch, total_code_chunks)` for the coverage readout.
    pub fn code_embedding_coverage(&self, epoch: &str) -> AppResult<(u64, u64)> {
        let total = {
            let rows = self.run(
                "?[count(id)] := *code_chunk{id}",
                BTreeMap::new(),
                ScriptMutability::Immutable,
            )?;
            rows.rows.first().and_then(|r| r.first()).map(dv_i64).unwrap_or(0) as u64
        };
        let embedded = if self.existing_relations()?.contains("code_vec") {
            let mut p = BTreeMap::new();
            p.insert("epoch".to_string(), DataValue::Str(epoch.into()));
            let rows = self.run(
                "?[count(chunk_id)] := *code_vec{chunk_id, epoch}, epoch == $epoch",
                p,
                ScriptMutability::Immutable,
            )?;
            rows.rows.first().and_then(|r| r.first()).map(dv_i64).unwrap_or(0) as u64
        } else {
            0
        };
        Ok((embedded, total))
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
                .map(|v| dv_i64(v))
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

    fn run_mut(&self, script: &str, params: BTreeMap<String, DataValue>) -> AppResult<cozo::NamedRows> {
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
}

// ── V10 session / action memory ──────────────────────────────────────────
//
// Stored in the same `graph.db` but in relations ensured OUTSIDE `RELATIONS`,
// so a full index `reset()` (rebuild) never wipes memory. Memory is runtime
// event data, not derived from source.
impl GraphIndex {
    /// Ensure the memory relations exist. Idempotent; called at every open.
    pub fn ensure_memory_relations(&self) -> AppResult<()> {
        let existing = self.existing_relations()?;
        let defs: &[(&str, &str)] = &[
            (
                "session",
                ":create session {session_id: String => agent: String, started_ms: Int, last_ms: Int}",
            ),
            (
                "mem_event",
                ":create mem_event {session_id: String, seq: Int => \
                    kind: String, path: String, symbol: String?, line: Int?, ts_ms: Int, detail: String?}",
            ),
            (
                "mem_note",
                ":create mem_note {note_id: String => session_id: String, text: String, ts_ms: Int, pinned: Bool}",
            ),
            // V11 Phase F: cached local-model digests, keyed by file + content
            // hash so a stale entry is simply ignored. Additive (survives a
            // graph rebuild — recomputing digests costs local GPU time).
            (
                "digest",
                ":create digest {file: String, content_hash: String => text: String, ts_ms: Int}",
            ),
            // V12 Phase D: per-file git churn (last touch time/subject, 90-day
            // touch count). Additive (survives a graph rebuild) — it's
            // git-derived and fully repopulated at the end of every rebuild
            // pass and refreshed incrementally on watcher batches, never
            // built from parsed source like the `RELATIONS` set.
            (
                "commit_touch",
                ":create commit_touch {file: String => last_ts: Int, last_subject: String, touches_90d: Int}",
            ),
            // V12 Phase E: durable project facts distilled from session memory
            // (or added manually) — additive, survives a graph rebuild AND a
            // session's own eviction (that's the point: a fact outlives the
            // session it came from).
            (
                "project_fact",
                ":create project_fact {fact_id: String => text: String, source_session: String, \
                    ts_ms: Int, pinned: Bool, archived: Bool}",
            ),
            // V12 Phase E: whether a session has already run the distiller, kept
            // as its OWN additive relation rather than a column on `session` so
            // the eviction-cascade shape (see `prune_sessions_in_tx`) and the
            // core session upsert (`record_mem_event`) never need to touch it.
            (
                "session_distilled",
                ":create session_distilled {session_id: String => distilled: Bool, ts_ms: Int}",
            ),
            // V12 Phase F: a small generic key/value store — see `get_meta`/
            // `put_meta` below. Additive, survives a graph rebuild.
            (
                "meta",
                ":create meta {key: String => value: String}",
            ),
            // V14 Phase C: token/cost accounting ring (the X-ray backend).
            // Additive, survives a graph rebuild, ring-bounded + evicted with
            // its session exactly like `mem_event` (see `record_usage_event`
            // and `prune_sessions_in_tx`). NOT part of a schema-version bump
            // — same posture as every other memory relation in this list.
            (
                "usage_stat",
                ":create usage_stat {session_id: String, seq: Int => \
                    kind: String, model: String?, msg_id: String?, \
                    in_tok: Int, out_tok: Int, cache_read: Int, cache_make: Int, \
                    tool: String?, chars: Int, ts_ms: Int}",
            ),
        ];
        for (name, create) in defs {
            if !existing.contains(*name) {
                self.run_mut(create, BTreeMap::new())?;
            }
        }
        Ok(())
    }

    /// Append one memory event for `session_id`, upserting the session's
    /// last-seen time and pruning old events / evicting old sessions — all in a
    /// single write transaction so the monotonic `seq` allocation is race-free.
    #[allow(clippy::too_many_arguments)]
    pub fn record_mem_event(
        &self,
        session_id: &str,
        agent: &str,
        kind: &str,
        path: &str,
        symbol: Option<&str>,
        line: Option<u32>,
        ts_ms: i64,
        detail: Option<&str>,
    ) -> AppResult<()> {
        let sid = session_id.to_string();
        let agent = agent.to_string();
        let kind = kind.to_string();
        let path = path.to_string();
        let symbol = symbol.map(|s| s.to_string());
        let detail = detail.map(|s| s.to_string());
        self.with_write_txn(move |tx| {
            let mut p = BTreeMap::new();
            p.insert("sid".to_string(), DataValue::Str(sid.as_str().into()));

            // Next per-session seq — monotonic even after a prune: max(seq)+1.
            // Aggregations must live in the rule head in CozoScript.
            let rows = tx_run(
                tx,
                "?[count(seq), max(seq)] := *mem_event{session_id, seq}, session_id == $sid",
                p.clone(),
            )?;
            let (cnt, mx) = rows
                .rows
                .first()
                .map(|r| (cell_i64(r, 0), cell_i64(r, 1)))
                .unwrap_or((0, 0));
            let seq = if cnt == 0 { 0 } else { mx + 1 };

            // Upsert the session: keep its started_ms, bump last_ms to now.
            let existing = tx_run(
                tx,
                "?[started_ms] := *session{session_id, started_ms}, session_id == $sid",
                p.clone(),
            )?;
            let started = existing.rows.first().map(|r| cell_i64(r, 0)).unwrap_or(ts_ms);
            let mut ps = BTreeMap::new();
            ps.insert("sid".to_string(), DataValue::Str(sid.as_str().into()));
            ps.insert("agent".to_string(), DataValue::Str(agent.as_str().into()));
            ps.insert("st".to_string(), DataValue::Num(Num::Int(started)));
            ps.insert("last".to_string(), DataValue::Num(Num::Int(ts_ms)));
            tx_run(
                tx,
                "?[session_id, agent, started_ms, last_ms] <- [[$sid, $agent, $st, $last]]\n\
                 :put session {session_id => agent, started_ms, last_ms}",
                ps,
            )?;

            // Insert the event.
            let row = DataValue::List(vec![
                DataValue::Str(sid.as_str().into()),
                DataValue::Num(Num::Int(seq)),
                DataValue::Str(kind.as_str().into()),
                DataValue::Str(path.as_str().into()),
                symbol.as_deref().map(|s| DataValue::Str(s.into())).unwrap_or(DataValue::Null),
                line.map(|l| DataValue::Num(Num::Int(l as i64))).unwrap_or(DataValue::Null),
                DataValue::Num(Num::Int(ts_ms)),
                detail.as_deref().map(|s| DataValue::Str(s.into())).unwrap_or(DataValue::Null),
            ]);
            tx_put(
                tx,
                "?[session_id, seq, kind, path, symbol, line, ts_ms, detail] <- $rows\n\
                 :put mem_event {session_id, seq => kind, path, symbol, line, ts_ms, detail}",
                vec![row],
            )?;

            // Ring-prune this session's oldest events beyond the cap.
            let cutoff = seq - MAX_EVENTS_PER_SESSION;
            if cutoff >= 0 {
                let mut pc = BTreeMap::new();
                pc.insert("sid".to_string(), DataValue::Str(sid.as_str().into()));
                pc.insert("cut".to_string(), DataValue::Num(Num::Int(cutoff)));
                tx_run(
                    tx,
                    "?[session_id, seq] := *mem_event{session_id, seq}, session_id == $sid, seq <= $cut\n:rm mem_event {session_id, seq}",
                    pc,
                )?;
            }

            // Evict sessions beyond the per-root cap (cascade events + unpinned
            // notes; pinned notes survive).
            prune_sessions_in_tx(tx)?;
            Ok(())
        })
    }

    // ── V14 Phase C: usage / cost accounting ──────────────────────────────

    /// Append one usage event for `session_id`: upserts the session's
    /// last-seen time (same shape as `record_mem_event` — usage rows are
    /// often the FIRST activity a chat-only turn ever produces, so this tap
    /// must be able to create the `session` row on its own, not just piggy-
    /// back on a tool call). A `Turn` event is UPSERTED in place when a row
    /// for its `msg_id` already exists (a streamed message's `usage` block
    /// firming up across updates); every other write appends a new row.
    /// Ring-prunes beyond [`MAX_USAGE_PER_SESSION`], then evicts old sessions
    /// via the same cascade `record_mem_event` uses — all in one write
    /// transaction so the monotonic `seq` allocation is race-free.
    pub fn record_usage_event(
        &self,
        session_id: &str,
        agent: &str,
        event: &UsageEvent,
        ts_ms: i64,
    ) -> AppResult<()> {
        let sid = session_id.to_string();
        let agent = agent.to_string();
        let event = event.clone();
        self.with_write_txn(move |tx| {
            let mut p = BTreeMap::new();
            p.insert("sid".to_string(), DataValue::Str(sid.as_str().into()));

            // Upsert the session (identical shape to `record_mem_event`).
            let existing = tx_run(
                tx,
                "?[started_ms] := *session{session_id, started_ms}, session_id == $sid",
                p.clone(),
            )?;
            let started = existing.rows.first().map(|r| cell_i64(r, 0)).unwrap_or(ts_ms);
            let mut ps = BTreeMap::new();
            ps.insert("sid".to_string(), DataValue::Str(sid.as_str().into()));
            ps.insert("agent".to_string(), DataValue::Str(agent.as_str().into()));
            ps.insert("st".to_string(), DataValue::Num(Num::Int(started)));
            ps.insert("last".to_string(), DataValue::Num(Num::Int(ts_ms)));
            tx_run(
                tx,
                "?[session_id, agent, started_ms, last_ms] <- [[$sid, $agent, $st, $last]]\n\
                 :put session {session_id => agent, started_ms, last_ms}",
                ps,
            )?;

            // A "turn" event upserts by msg_id: look for an existing row's
            // seq first so we overwrite in place rather than append.
            let existing_seq = if let UsageEvent::Turn { msg_id, .. } = &event {
                let mut pm = p.clone();
                pm.insert("mid".to_string(), DataValue::Str(msg_id.as_str().into()));
                let rows = tx_run(
                    tx,
                    "?[seq] := *usage_stat{session_id, seq, kind, msg_id}, \
                        session_id == $sid, kind == \"turn\", msg_id == $mid\n:limit 1",
                    pm,
                )?;
                rows.rows.first().map(|r| cell_i64(r, 0))
            } else {
                None
            };

            let seq = match existing_seq {
                Some(s) => s,
                None => {
                    let rows = tx_run(
                        tx,
                        "?[count(seq), max(seq)] := *usage_stat{session_id, seq}, session_id == $sid",
                        p.clone(),
                    )?;
                    let (cnt, mx) =
                        rows.rows.first().map(|r| (cell_i64(r, 0), cell_i64(r, 1))).unwrap_or((0, 0));
                    if cnt == 0 { 0 } else { mx + 1 }
                }
            };

            let (kind, model, msg_id, in_tok, out_tok, cache_read, cache_make, tool, chars): (
                &str,
                Option<String>,
                Option<String>,
                i64,
                i64,
                i64,
                i64,
                Option<String>,
                i64,
            ) = match &event {
                UsageEvent::Turn { msg_id, model, in_tok, out_tok, cache_read, cache_make } => (
                    "turn",
                    model.clone(),
                    Some(msg_id.clone()),
                    *in_tok as i64,
                    *out_tok as i64,
                    *cache_read as i64,
                    *cache_make as i64,
                    None,
                    0,
                ),
                UsageEvent::ToolResult { tool, chars } => {
                    ("tool_result", None, None, 0, 0, 0, 0, tool.clone(), *chars as i64)
                }
            };

            let row = DataValue::List(vec![
                DataValue::Str(sid.as_str().into()),
                DataValue::Num(Num::Int(seq)),
                DataValue::Str(kind.into()),
                model.as_deref().map(|s| DataValue::Str(s.into())).unwrap_or(DataValue::Null),
                msg_id.as_deref().map(|s| DataValue::Str(s.into())).unwrap_or(DataValue::Null),
                DataValue::Num(Num::Int(in_tok)),
                DataValue::Num(Num::Int(out_tok)),
                DataValue::Num(Num::Int(cache_read)),
                DataValue::Num(Num::Int(cache_make)),
                tool.as_deref().map(|s| DataValue::Str(s.into())).unwrap_or(DataValue::Null),
                DataValue::Num(Num::Int(chars)),
                DataValue::Num(Num::Int(ts_ms)),
            ]);
            tx_put(
                tx,
                "?[session_id, seq, kind, model, msg_id, in_tok, out_tok, cache_read, cache_make, tool, chars, ts_ms] <- $rows\n\
                 :put usage_stat {session_id, seq => kind, model, msg_id, in_tok, out_tok, cache_read, cache_make, tool, chars, ts_ms}",
                vec![row],
            )?;

            // Ring-prune this session's oldest usage rows beyond the cap.
            let cutoff = seq - MAX_USAGE_PER_SESSION;
            if cutoff >= 0 {
                let mut pc = BTreeMap::new();
                pc.insert("sid".to_string(), DataValue::Str(sid.as_str().into()));
                pc.insert("cut".to_string(), DataValue::Num(Num::Int(cutoff)));
                tx_run(
                    tx,
                    "?[session_id, seq] := *usage_stat{session_id, seq}, session_id == $sid, seq <= $cut\n:rm usage_stat {session_id, seq}",
                    pc,
                )?;
            }

            // Evict sessions beyond the per-root cap (cascade events + usage +
            // unpinned notes; pinned notes survive).
            prune_sessions_in_tx(tx)?;
            Ok(())
        })
    }

    /// Summed token totals for `session_id` across its "turn" rows
    /// ("tool_result" rows carry chars, not tokens, so they don't contribute).
    pub fn usage_session_totals(&self, session_id: &str) -> AppResult<UsageTotals> {
        let mut p = BTreeMap::new();
        p.insert("sid".to_string(), DataValue::Str(session_id.into()));
        // `seq` MUST stay in the projection even though it's unused below:
        // CozoScript relations are SETS, so two turns with identical token
        // counts would otherwise collapse into one row and undercount (the
        // same reason `mem_working_set` keeps `seq` in its own projection).
        let rows = self.run(
            "?[seq, in_tok, out_tok, cache_read, cache_make] := \
                *usage_stat{session_id, seq, kind, in_tok, out_tok, cache_read, cache_make}, \
                session_id == $sid, kind == \"turn\"",
            p,
            ScriptMutability::Immutable,
        )?;
        let mut t = UsageTotals::default();
        for r in &rows.rows {
            t.in_tok += cell_i64(r, 1).max(0) as u64;
            t.out_tok += cell_i64(r, 2).max(0) as u64;
            t.cache_read += cell_i64(r, 3).max(0) as u64;
            t.cache_make += cell_i64(r, 4).max(0) as u64;
        }
        Ok(t)
    }

    /// Estimated tool-result characters for `session_id`, grouped by tool
    /// name (`"unknown"` when the id → name join missed — see the claude
    /// tap's `ToolNameRing`), descending by chars.
    pub fn usage_per_tool(&self, session_id: &str) -> AppResult<Vec<(String, u64)>> {
        let mut p = BTreeMap::new();
        p.insert("sid".to_string(), DataValue::Str(session_id.into()));
        // Same `seq`-keeps-rows-distinct reasoning as `usage_session_totals`:
        // without it, two tool results with identical (tool, chars) — e.g.
        // two 1-char Bash results — would collapse into one row.
        let rows = self.run(
            "?[seq, tool, chars] := *usage_stat{session_id, seq, kind, tool, chars}, \
                session_id == $sid, kind == \"tool_result\"",
            p,
            ScriptMutability::Immutable,
        )?;
        let mut sums: HashMap<String, u64> = HashMap::new();
        for r in &rows.rows {
            let tool = cell_str_opt(r, 1).unwrap_or_else(|| "unknown".to_string());
            *sums.entry(tool).or_insert(0) += cell_i64(r, 2).max(0) as u64;
        }
        let mut out: Vec<(String, u64)> = sums.into_iter().collect();
        out.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        Ok(out)
    }

    /// Per-turn token breakdown for `session_id`, ordered oldest → newest.
    /// Each turn's `tool_chars` is the (estimated) tool-result characters
    /// that arrived AFTER the previous turn and before this one — i.e. the
    /// tool output this turn's assistant message actually read as input
    /// context. Tool-result rows after the LAST turn (mid-turn, the next
    /// assistant reply hasn't landed yet) aren't attributable to a turn yet
    /// and are dropped from this series; they still count toward
    /// `usage_per_tool`'s totals.
    pub fn usage_turn_series(&self, session_id: &str) -> AppResult<Vec<TurnUsage>> {
        let mut p = BTreeMap::new();
        p.insert("sid".to_string(), DataValue::Str(session_id.into()));
        let rows = self.run(
            "?[seq, kind, model, msg_id, in_tok, out_tok, cache_read, cache_make, tool, chars, ts_ms] := \
                *usage_stat{session_id, seq, kind, model, msg_id, in_tok, out_tok, cache_read, cache_make, tool, chars, ts_ms}, \
                session_id == $sid\n:order seq",
            p,
            ScriptMutability::Immutable,
        )?;
        let mut out = Vec::new();
        let mut pending_tool_chars: u64 = 0;
        for r in &rows.rows {
            if cell_str(r, 1) == "tool_result" {
                pending_tool_chars += cell_i64(r, 9).max(0) as u64;
                continue;
            }
            out.push(TurnUsage {
                msg_id: cell_str_opt(r, 3).unwrap_or_default(),
                model: cell_str_opt(r, 2),
                in_tok: cell_i64(r, 4).max(0) as u64,
                out_tok: cell_i64(r, 5).max(0) as u64,
                cache_read: cell_i64(r, 6).max(0) as u64,
                cache_make: cell_i64(r, 7).max(0) as u64,
                tool_chars: pending_tool_chars,
                ts_ms: cell_i64(r, 10),
            });
            pending_tool_chars = 0;
        }
        Ok(out)
    }

    /// One row per known session with usage token totals, cache-hit ratio,
    /// and whether the session is estimate-only (no exact `usage` block —
    /// currently every non-Claude agent; see the OpenCode C3 spike note atop
    /// `oob/opencode.rs`). Reuses [`Self::mem_sessions`] for the session list
    /// so a session with usage but zero classified `mem_event`s still shows.
    pub fn usage_all_sessions(&self) -> AppResult<Vec<SessionUsageRow>> {
        let sessions = self.mem_sessions()?;
        let mut out = Vec::with_capacity(sessions.len());
        for s in sessions {
            let totals = self.usage_session_totals(&s.session_id)?;
            let per_tool = self.usage_per_tool(&s.session_id)?;
            let tool_chars: u64 = per_tool.iter().map(|(_, c)| *c).sum();
            let denom = totals.cache_read + totals.in_tok;
            let cache_hit_ratio =
                if denom > 0 { totals.cache_read as f64 / denom as f64 } else { 0.0 };
            out.push(SessionUsageRow {
                est_only: s.agent != "claude",
                session_id: s.session_id,
                agent: s.agent,
                totals,
                tool_chars,
                cache_hit_ratio,
            });
        }
        Ok(out)
    }

    /// The session with the most recent activity (across all agents), or `None`.
    pub fn mem_current_session(&self) -> AppResult<Option<String>> {
        self.mem_current_session_for(None)
    }

    /// The most-recently-active session id, optionally filtered to one `agent`
    /// (`"claude"` / `"opencode"`). Filtering lets the memory tools scope to the
    /// **calling** agent so a Claude tab and an OpenCode tab on the same project
    /// don't read or write each other's session. `None` = across all agents.
    pub fn mem_current_session_for(&self, agent: Option<&str>) -> AppResult<Option<String>> {
        let rows = match agent {
            Some(a) => {
                let mut p = BTreeMap::new();
                p.insert("agent".to_string(), DataValue::Str(a.into()));
                self.run(
                    "?[session_id, last_ms] := *session{session_id, agent, last_ms}, agent == $agent\n:order -last_ms\n:limit 1",
                    p,
                    ScriptMutability::Immutable,
                )?
            }
            None => self.run(
                "?[session_id, last_ms] := *session{session_id, last_ms}\n:order -last_ms\n:limit 1",
                BTreeMap::new(),
                ScriptMutability::Immutable,
            )?,
        };
        Ok(rows.rows.first().map(|r| cell_str(r, 0)))
    }

    /// V14 Phase D2: the agent tag currently stored for `session_id` (`"claude"`
    /// / `"opencode"`), or `None` if the session has no row yet. Used so a
    /// SECOND writer to the same session (the read advisor's "remind"
    /// `mem_event`, distinct from the file-read/edit taps) never overwrites
    /// the session's real agent with a fabricated one — `record_mem_event`'s
    /// upsert always takes whatever `agent` it's given.
    pub fn session_agent(&self, session_id: &str) -> AppResult<Option<String>> {
        let mut p = BTreeMap::new();
        p.insert("sid".to_string(), DataValue::Str(session_id.into()));
        let rows = self.run(
            "?[agent] := *session{session_id, agent}, session_id == $sid",
            p,
            ScriptMutability::Immutable,
        )?;
        Ok(rows.rows.first().map(|r| cell_str(r, 0)))
    }

    /// V14 Phase D2: the distinct set of project-relative paths `session_id`
    /// has read or edited (`mem_event` kind `"read"`/`"edit"`) — the "was it
    /// subsequently touched" half of the injection ⋈ mem_event join (see
    /// `GraphService::injection_follow_rate`, which joins this against the
    /// V11-C in-memory injection state).
    pub fn mem_touched_paths(&self, session_id: &str) -> AppResult<HashSet<String>> {
        let mut p = BTreeMap::new();
        p.insert("sid".to_string(), DataValue::Str(session_id.into()));
        let rows = self.run(
            "?[path] := *mem_event{session_id, path, kind}, session_id == $sid, \
                (kind == \"read\" or kind == \"edit\")",
            p,
            ScriptMutability::Immutable,
        )?;
        Ok(rows.rows.iter().map(|r| cell_str(r, 0)).collect())
    }

    /// V14 Phase D2: rate at which a read-advisor reminder (`mem_event` kind
    /// `"remind"` — written by `GraphService::should_read` alongside the
    /// process-wide Activity event) is followed by a REAL `"read"` event for
    /// the SAME file in the SAME session — i.e. the agent re-read the file in
    /// full anyway, despite the reminder. A high rate signals the reminder
    /// wasn't sufficient, so `read_advisor_min_lines` should rise (those
    /// files are evidently needed whole — let them pass instead of getting
    /// reminded). Returns `(rate, sample count)`; `None` when no reminder has
    /// ever fired for this project (the read advisor is off, or never
    /// qualified a file).
    pub fn advisor_reread_rate(&self) -> AppResult<Option<(f64, u64)>> {
        let reminds = self.run(
            "?[session_id, path, ts_ms] := *mem_event{session_id, kind, path, ts_ms}, kind == \"remind\"",
            BTreeMap::new(),
            ScriptMutability::Immutable,
        )?;
        if reminds.rows.is_empty() {
            return Ok(None);
        }
        let reads = self.run(
            "?[session_id, path, ts_ms] := *mem_event{session_id, kind, path, ts_ms}, kind == \"read\"",
            BTreeMap::new(),
            ScriptMutability::Immutable,
        )?;
        let mut reads_by_key: HashMap<(String, String), Vec<i64>> = HashMap::new();
        for r in &reads.rows {
            reads_by_key
                .entry((cell_str(r, 0), cell_str(r, 1)))
                .or_default()
                .push(cell_i64(r, 2));
        }
        let mut total = 0u64;
        let mut reread = 0u64;
        for r in &reminds.rows {
            total += 1;
            let key = (cell_str(r, 0), cell_str(r, 1));
            let remind_ts = cell_i64(r, 2);
            if reads_by_key.get(&key).is_some_and(|ts| ts.iter().any(|&t| t > remind_ts)) {
                reread += 1;
            }
        }
        Ok(Some((reread as f64 / total as f64, total)))
    }

    /// V14 Phase D: per-tool ranking for the Usage section's "top consumers"
    /// table: `(tool, chars, calls)` descending by chars. Distinct from
    /// [`Self::usage_per_tool`] (chars-only, feeds `SessionUsageRow.tool_chars`)
    /// because the table also wants a call count.
    pub fn usage_tool_ranking(&self, session_id: &str) -> AppResult<Vec<(String, u64, u64)>> {
        let mut p = BTreeMap::new();
        p.insert("sid".to_string(), DataValue::Str(session_id.into()));
        // `seq` stays in the projection for the same set-semantics reason as
        // `usage_per_tool` — two identical (tool, chars) tool-result rows
        // must not collapse into one.
        let rows = self.run(
            "?[seq, tool, chars] := *usage_stat{session_id, seq, kind, tool, chars}, \
                session_id == $sid, kind == \"tool_result\"",
            p,
            ScriptMutability::Immutable,
        )?;
        let mut sums: HashMap<String, (u64, u64)> = HashMap::new();
        for r in &rows.rows {
            let tool = cell_str_opt(r, 1).unwrap_or_else(|| "unknown".to_string());
            let entry = sums.entry(tool).or_insert((0, 0));
            entry.0 += cell_i64(r, 2).max(0) as u64;
            entry.1 += 1;
        }
        let mut out: Vec<(String, u64, u64)> =
            sums.into_iter().map(|(tool, (chars, calls))| (tool, chars, calls)).collect();
        out.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        Ok(out)
    }

    /// The ranked working set for `session_id`: files aggregated from its events,
    /// scored `frequency × kind_weight` with recency as the tiebreak, newest and
    /// most-edited first. Bounded to `max` entries.
    pub fn mem_working_set(&self, session_id: &str, max: usize) -> AppResult<Vec<WorkingSetEntry>> {
        let mut p = BTreeMap::new();
        p.insert("sid".to_string(), DataValue::Str(session_id.into()));
        let rows = self.run(
            "?[path, kind, symbol, ts_ms, seq] := \
                *mem_event{session_id, seq, kind, path, symbol, ts_ms}, session_id == $sid",
            p,
            ScriptMutability::Immutable,
        )?;

        struct Agg {
            touches: u32,
            last_kind: String,
            last_ms: i64,
            last_seq: i64,
            symbols: Vec<(i64, String)>,
        }
        let mut map: HashMap<String, Agg> = HashMap::new();
        for r in &rows.rows {
            let path = cell_str(r, 0);
            if path.is_empty() {
                continue;
            }
            let kind = cell_str(r, 1);
            let symbol = cell_str(r, 2);
            let ts = cell_i64(r, 3);
            let seq = cell_i64(r, 4);
            let e = map.entry(path).or_insert(Agg {
                touches: 0,
                last_kind: String::new(),
                last_ms: 0,
                last_seq: -1,
                symbols: Vec::new(),
            });
            e.touches += 1;
            if seq > e.last_seq {
                e.last_seq = seq;
                e.last_kind = kind;
                e.last_ms = ts;
            }
            if !symbol.is_empty() {
                e.symbols.push((seq, symbol));
            }
        }

        let mut entries: Vec<(f64, WorkingSetEntry)> = map
            .into_iter()
            .map(|(path, a)| {
                // Distinct symbols, most recent (highest seq) first, bounded.
                let mut syms = a.symbols;
                syms.sort_by(|x, y| y.0.cmp(&x.0));
                let mut seen = HashSet::new();
                let top: Vec<String> = syms
                    .into_iter()
                    .filter(|(_, s)| seen.insert(s.clone()))
                    .map(|(_, s)| s)
                    .take(5)
                    .collect();
                let score = a.touches as f64 * kind_weight(&a.last_kind);
                (
                    score,
                    WorkingSetEntry {
                        path,
                        touches: a.touches,
                        last_kind: a.last_kind,
                        last_ms: a.last_ms,
                        top_symbols: top,
                    },
                )
            })
            .collect();
        // Score desc, then recency desc as the tiebreak.
        entries.sort_by(|a, b| {
            b.0.partial_cmp(&a.0)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then(b.1.last_ms.cmp(&a.1.last_ms))
        });
        Ok(entries.into_iter().take(max).map(|(_, e)| e).collect())
    }

    /// Record a note (a decision/fact) for a session.
    pub fn mem_add_note(
        &self,
        note_id: &str,
        session_id: &str,
        text: &str,
        ts_ms: i64,
        pinned: bool,
    ) -> AppResult<()> {
        let mut p = BTreeMap::new();
        p.insert("nid".to_string(), DataValue::Str(note_id.into()));
        p.insert("sid".to_string(), DataValue::Str(session_id.into()));
        p.insert("text".to_string(), DataValue::Str(text.into()));
        p.insert("ts".to_string(), DataValue::Num(Num::Int(ts_ms)));
        p.insert("pin".to_string(), DataValue::Bool(pinned));
        self.run_mut(
            "?[note_id, session_id, text, ts_ms, pinned] <- [[$nid, $sid, $text, $ts, $pin]]\n\
             :put mem_note {note_id => session_id, text, ts_ms, pinned}",
            p,
        )?;
        Ok(())
    }

    /// Set/clear the pinned flag on a note.
    pub fn mem_set_note_pinned(&self, note_id: &str, pinned: bool) -> AppResult<()> {
        let mut p = BTreeMap::new();
        p.insert("nid".to_string(), DataValue::Str(note_id.into()));
        p.insert("pin".to_string(), DataValue::Bool(pinned));
        // Read-modify-write to keep the other columns intact.
        let rows = self.run(
            "?[session_id, text, ts_ms] := *mem_note{note_id, session_id, text, ts_ms}, note_id == $nid",
            p.clone(),
            ScriptMutability::Immutable,
        )?;
        let Some(r) = rows.rows.first() else { return Ok(()) };
        p.insert("sid".to_string(), DataValue::Str(cell_str(r, 0).as_str().into()));
        p.insert("text".to_string(), DataValue::Str(cell_str(r, 1).as_str().into()));
        p.insert("ts".to_string(), DataValue::Num(Num::Int(cell_i64(r, 2))));
        self.run_mut(
            "?[note_id, session_id, text, ts_ms, pinned] <- [[$nid, $sid, $text, $ts, $pin]]\n\
             :put mem_note {note_id => session_id, text, ts_ms, pinned}",
            p,
        )?;
        Ok(())
    }

    /// A session's notes plus every pinned note in the project, pinned first
    /// then newest.
    pub fn mem_notes(&self, session_id: &str) -> AppResult<Vec<MemNote>> {
        let mut p = BTreeMap::new();
        p.insert("sid".to_string(), DataValue::Str(session_id.into()));
        let rows = self.run(
            "?[note_id, session_id, text, ts_ms, pinned] := \
                *mem_note{note_id, session_id, text, ts_ms, pinned}, \
                (session_id == $sid or pinned == true)",
            p,
            ScriptMutability::Immutable,
        )?;
        let mut notes: Vec<MemNote> = rows
            .rows
            .iter()
            .map(|r| MemNote {
                note_id: cell_str(r, 0),
                session_id: cell_str(r, 1),
                text: cell_str(r, 2),
                ts_ms: cell_i64(r, 3),
                pinned: cell_bool(r, 4),
            })
            .collect();
        notes.sort_by(|a, b| b.pinned.cmp(&a.pinned).then(b.ts_ms.cmp(&a.ts_ms)));
        Ok(notes)
    }

    /// All known sessions with their event counts, newest activity first.
    pub fn mem_sessions(&self) -> AppResult<Vec<SessionInfo>> {
        let srows = self.run(
            "?[session_id, agent, started_ms, last_ms] := \
                *session{session_id, agent, started_ms, last_ms}\n:order -last_ms",
            BTreeMap::new(),
            ScriptMutability::Immutable,
        )?;
        let crows = self.run(
            "?[session_id, count(seq)] := *mem_event{session_id, seq}",
            BTreeMap::new(),
            ScriptMutability::Immutable,
        )?;
        let mut counts: HashMap<String, u32> = HashMap::new();
        for r in &crows.rows {
            counts.insert(cell_str(r, 0), cell_i64(r, 1) as u32);
        }
        Ok(srows
            .rows
            .iter()
            .map(|r| {
                let sid = cell_str(r, 0);
                let events = counts.get(&sid).copied().unwrap_or(0);
                SessionInfo {
                    session_id: sid,
                    agent: cell_str(r, 1),
                    started_ms: cell_i64(r, 2),
                    last_ms: cell_i64(r, 3),
                    events,
                }
            })
            .collect())
    }

    /// Clear memory: one session (events + notes + session row) when `session_id`
    /// is `Some`, or the whole project's memory when `None`.
    pub fn mem_clear(&self, session_id: Option<&str>) -> AppResult<()> {
        self.with_write_txn(|tx| {
            match session_id {
                Some(sid) => {
                    let mut p = BTreeMap::new();
                    p.insert("sid".to_string(), DataValue::Str(sid.into()));
                    tx_run(tx, "?[session_id, seq] := *mem_event{session_id, seq}, session_id == $sid\n:rm mem_event {session_id, seq}", p.clone())?;
                    tx_run(tx, "?[session_id, seq] := *usage_stat{session_id, seq}, session_id == $sid\n:rm usage_stat {session_id, seq}", p.clone())?;
                    tx_run(tx, "?[note_id] := *mem_note{note_id, session_id}, session_id == $sid\n:rm mem_note {note_id}", p.clone())?;
                    // F5: drop the distilled flag too, else a cleared session
                    // stays marked distilled and its later work is never distilled.
                    tx_run(tx, "?[session_id] := *session_distilled{session_id}, session_id == $sid\n:rm session_distilled {session_id}", p.clone())?;
                    tx_run(tx, "?[session_id] := *session{session_id}, session_id == $sid\n:rm session {session_id}", p)?;
                }
                None => {
                    tx_run(tx, "?[session_id, seq] := *mem_event{session_id, seq}\n:rm mem_event {session_id, seq}", BTreeMap::new())?;
                    tx_run(tx, "?[session_id, seq] := *usage_stat{session_id, seq}\n:rm usage_stat {session_id, seq}", BTreeMap::new())?;
                    tx_run(tx, "?[note_id] := *mem_note{note_id}\n:rm mem_note {note_id}", BTreeMap::new())?;
                    tx_run(tx, "?[session_id] := *session_distilled{session_id}\n:rm session_distilled {session_id}", BTreeMap::new())?;
                    tx_run(tx, "?[session_id] := *session{session_id}\n:rm session {session_id}", BTreeMap::new())?;
                }
            }
            Ok(())
        })
    }

    // ── V12 Phase E: project facts (memory distillation) ─────────────────

    /// Insert a new project fact (distiller output or a manual add), then
    /// enforce the [`MAX_LIVE_PROJECT_FACTS`] cap: if the live (non-archived)
    /// count is now over the cap, archive the oldest UNPINNED live fact.
    /// Never archives a pinned fact — if every live fact (including the one
    /// just inserted) is pinned, the cap is simply exceeded. All in one write
    /// transaction so a concurrent insert can't race the cap check.
    pub fn add_project_fact(
        &self,
        fact_id: &str,
        text: &str,
        source_session: &str,
        ts_ms: i64,
        pinned: bool,
    ) -> AppResult<()> {
        let fact_id = fact_id.to_string();
        let text = text.to_string();
        let source_session = source_session.to_string();
        self.with_write_txn(move |tx| {
            let row = DataValue::List(vec![
                DataValue::Str(fact_id.as_str().into()),
                DataValue::Str(text.as_str().into()),
                DataValue::Str(source_session.as_str().into()),
                DataValue::Num(Num::Int(ts_ms)),
                DataValue::Bool(pinned),
                DataValue::Bool(false),
            ]);
            tx_put(
                tx,
                "?[fact_id, text, source_session, ts_ms, pinned, archived] <- $rows\n\
                 :put project_fact {fact_id => text, source_session, ts_ms, pinned, archived}",
                vec![row],
            )?;

            let rows = tx_run(
                tx,
                "?[fact_id, ts_ms, pinned] := *project_fact{fact_id, ts_ms, pinned, archived}, archived == false",
                BTreeMap::new(),
            )?;
            let live: Vec<(String, i64, bool)> = rows
                .rows
                .iter()
                .map(|r| (cell_str(r, 0), cell_i64(r, 1), cell_bool(r, 2)))
                .collect();
            if let Some(to_archive) = fact_to_archive_for_cap(&live, MAX_LIVE_PROJECT_FACTS) {
                archive_fact_in_tx(tx, &to_archive)?;
            }
            Ok(())
        })
    }

    /// Live (non-archived) project facts unless `include_archived`, pinned
    /// first then newest, bounded to `max`.
    pub fn list_project_facts(&self, include_archived: bool, max: usize) -> AppResult<Vec<ProjectFact>> {
        let script = if include_archived {
            "?[fact_id, text, source_session, ts_ms, pinned, archived] := \
                *project_fact{fact_id, text, source_session, ts_ms, pinned, archived}"
        } else {
            "?[fact_id, text, source_session, ts_ms, pinned, archived] := \
                *project_fact{fact_id, text, source_session, ts_ms, pinned, archived}, archived == false"
        };
        let rows = self.run(script, BTreeMap::new(), ScriptMutability::Immutable)?;
        let mut facts: Vec<ProjectFact> = rows
            .rows
            .iter()
            .map(|r| ProjectFact {
                fact_id: cell_str(r, 0),
                text: cell_str(r, 1),
                source_session: cell_str(r, 2),
                ts_ms: cell_i64(r, 3),
                pinned: cell_bool(r, 4),
                archived: cell_bool(r, 5),
            })
            .collect();
        facts.sort_by(|a, b| b.pinned.cmp(&a.pinned).then(b.ts_ms.cmp(&a.ts_ms)));
        facts.truncate(max);
        Ok(facts)
    }

    /// Pin/unpin a fact (read-modify-write to keep the other columns intact).
    pub fn set_fact_pinned(&self, fact_id: &str, pinned: bool) -> AppResult<()> {
        self.rewrite_fact(fact_id, |f| f.pinned = pinned)
    }

    /// Archive/unarchive a fact.
    pub fn set_fact_archived(&self, fact_id: &str, archived: bool) -> AppResult<()> {
        self.rewrite_fact(fact_id, |f| f.archived = archived)
    }

    /// Read-modify-write one fact's row, applying `mutate` to the in-memory
    /// copy. A no-op (not an error) when `fact_id` doesn't exist — mirrors
    /// [`Self::mem_set_note_pinned`]'s tolerant-missing-id posture (a stale
    /// UI row shouldn't surface an error toast).
    fn rewrite_fact(&self, fact_id: &str, mutate: impl FnOnce(&mut ProjectFact)) -> AppResult<()> {
        let mut p = BTreeMap::new();
        p.insert("fid".to_string(), DataValue::Str(fact_id.into()));
        let rows = self.run(
            "?[text, source_session, ts_ms, pinned, archived] := \
                *project_fact{fact_id, text, source_session, ts_ms, pinned, archived}, fact_id == $fid",
            p,
            ScriptMutability::Immutable,
        )?;
        let Some(r) = rows.rows.first() else { return Ok(()) };
        let mut fact = ProjectFact {
            fact_id: fact_id.to_string(),
            text: cell_str(r, 0),
            source_session: cell_str(r, 1),
            ts_ms: cell_i64(r, 2),
            pinned: cell_bool(r, 3),
            archived: cell_bool(r, 4),
        };
        mutate(&mut fact);
        let row = DataValue::List(vec![
            DataValue::Str(fact.fact_id.as_str().into()),
            DataValue::Str(fact.text.as_str().into()),
            DataValue::Str(fact.source_session.as_str().into()),
            DataValue::Num(Num::Int(fact.ts_ms)),
            DataValue::Bool(fact.pinned),
            DataValue::Bool(fact.archived),
        ]);
        self.put(
            "?[fact_id, text, source_session, ts_ms, pinned, archived] <- $rows\n\
             :put project_fact {fact_id => text, source_session, ts_ms, pinned, archived}",
            vec![row],
        )
    }

    /// Permanently delete a fact (vs. archive, which keeps the row).
    pub fn delete_fact(&self, fact_id: &str) -> AppResult<()> {
        let mut p = BTreeMap::new();
        p.insert("fid".to_string(), DataValue::Str(fact_id.into()));
        self.run_mut(
            "?[fact_id] := *project_fact{fact_id}, fact_id == $fid\n:rm project_fact {fact_id}",
            p,
        )?;
        Ok(())
    }

    /// Mark `session_id` as having run the distiller (successfully or not —
    /// the distiller marks this even on a validation failure, per its "never
    /// retry-loop a bad output" posture).
    pub fn mark_session_distilled(&self, session_id: &str, ts_ms: i64) -> AppResult<()> {
        let mut p = BTreeMap::new();
        p.insert("sid".to_string(), DataValue::Str(session_id.into()));
        p.insert("ts".to_string(), DataValue::Num(Num::Int(ts_ms)));
        self.run_mut(
            "?[session_id, distilled, ts_ms] <- [[$sid, true, $ts]]\n\
             :put session_distilled {session_id => distilled, ts_ms}",
            p,
        )?;
        Ok(())
    }

    /// Whether `session_id` has already run the distiller. `false` for a
    /// session with no `session_distilled` row (never attempted) — same
    /// honest-default posture as `is_test`/`visibility`.
    pub fn is_session_distilled(&self, session_id: &str) -> AppResult<bool> {
        let mut p = BTreeMap::new();
        p.insert("sid".to_string(), DataValue::Str(session_id.into()));
        let rows = self.run(
            "?[distilled] := *session_distilled{session_id, distilled}, session_id == $sid",
            p,
            ScriptMutability::Immutable,
        )?;
        Ok(rows.rows.first().map(|r| cell_bool(r, 0)).unwrap_or(false))
    }

    /// Sessions whose `last_ms` is older than `cutoff_ms` and that haven't
    /// been distilled yet — the candidate set for the idle-sweep trigger.
    /// Sessions are capped at [`MAX_SESSIONS_PER_ROOT`] so both scans here
    /// are small; no join is expressed in Datalog (simpler to read, cheap at
    /// this scale) — the distilled-id set is built once and checked in Rust.
    pub fn sessions_idle_undistilled(&self, cutoff_ms: i64) -> AppResult<Vec<String>> {
        let mut p = BTreeMap::new();
        p.insert("cutoff".to_string(), DataValue::Num(Num::Int(cutoff_ms)));
        let idle_rows = self.run(
            "?[session_id] := *session{session_id, last_ms}, last_ms < $cutoff",
            p,
            ScriptMutability::Immutable,
        )?;
        let idle: Vec<String> = idle_rows.rows.iter().map(|r| cell_str(r, 0)).collect();
        if idle.is_empty() {
            return Ok(idle);
        }
        let d_rows = self.run(
            "?[session_id, distilled] := *session_distilled{session_id, distilled}",
            BTreeMap::new(),
            ScriptMutability::Immutable,
        )?;
        let distilled: HashSet<String> = d_rows
            .rows
            .iter()
            .filter(|r| cell_bool(r, 1))
            .map(|r| cell_str(r, 0))
            .collect();
        Ok(idle.into_iter().filter(|s| !distilled.contains(s)).collect())
    }

    // ── V12 Phase D: git churn (`commit_touch`) ──────────────────────────

    /// Upsert churn rows collected by `graph::gitmeta::collect`/`collect_for`.
    /// A full-pass `collect()` result and an incremental `collect_for()`
    /// result are both just `:put` upserts here — same shape, same call —
    /// the difference (precise vs. approximate `touches_90d`) lives entirely
    /// in the collector, per its own doc comment.
    pub fn put_commit_touches(&self, rows: &[crate::graph::gitmeta::FileChurn]) -> AppResult<()> {
        if rows.is_empty() {
            return Ok(());
        }
        let data: Vec<DataValue> = rows
            .iter()
            .map(|c| {
                DataValue::List(vec![
                    DataValue::Str(c.file.as_str().into()),
                    DataValue::Num(Num::Int(c.last_ts)),
                    DataValue::Str(c.last_subject.as_str().into()),
                    DataValue::Num(Num::Int(c.touches_90d as i64)),
                ])
            })
            .collect();
        self.put(
            "?[file, last_ts, last_subject, touches_90d] <- $rows\n\
             :put commit_touch {file => last_ts, last_subject, touches_90d}",
            data,
        )
    }

    /// The stored churn row for one `file`, or `None` if it has no git history
    /// (never committed, or the project isn't a git repo — [`super::gitmeta`]
    /// degrades to collecting nothing in that case, so the relation simply has
    /// no row for it). Feeds the digest trailer (`graph::context::file_digest`)
    /// and the ranking churn boost.
    pub fn commit_touch(&self, file: &str) -> AppResult<Option<(i64, String, u32)>> {
        if !self.existing_relations()?.contains("commit_touch") {
            return Ok(None);
        }
        let mut p = BTreeMap::new();
        p.insert("file".to_string(), DataValue::Str(file.into()));
        let rows = self.run(
            "?[last_ts, last_subject, touches_90d] := \
                *commit_touch{file, last_ts, last_subject, touches_90d}, file == $file",
            p,
            ScriptMutability::Immutable,
        )?;
        Ok(rows
            .rows
            .first()
            .map(|r| (cell_i64(r, 0), cell_str(r, 1), cell_i64(r, 2) as u32)))
    }

    /// Churn-ranked files touched within the last `days`, optionally filtered
    /// to a `path_prefix`, most-touched first (ties broken by most-recent) —
    /// the engine behind `graph_recent_changes`. `commit_touch` is expected to
    /// be small (one row per ever-touched file, not per commit) but is
    /// unbounded in principle over a project's lifetime; the query itself is
    /// bounded (`:order -last_ts :limit $max`, so it never scans/returns the
    /// whole relation) rather than fetching everything and bounding only in
    /// Rust. The `days`/`path_prefix` filtering and the final touches-first
    /// sort still happen in Rust, over just the `max` newest-touched rows.
    pub fn recent_changes(
        &self,
        days: u32,
        path_prefix: Option<&str>,
        max: usize,
    ) -> AppResult<Vec<crate::graph::gitmeta::FileChurn>> {
        if !self.existing_relations()?.contains("commit_touch") {
            return Ok(Vec::new());
        }
        let mut p = BTreeMap::new();
        p.insert("max".to_string(), int(max.max(1).min(u32::MAX as usize) as u32));
        let rows = self.run(
            "?[file, last_ts, last_subject, touches_90d] := \
                *commit_touch{file, last_ts, last_subject, touches_90d}\n\
             :order -last_ts\n\
             :limit $max",
            p,
            ScriptMutability::Immutable,
        )?;
        let cutoff = crate::graph::gitmeta::now_ts() - (days as i64) * 86_400;
        let mut out: Vec<crate::graph::gitmeta::FileChurn> = rows
            .rows
            .iter()
            .filter_map(|r| {
                let last_ts = cell_i64(r, 1);
                if last_ts < cutoff {
                    return None;
                }
                let file = cell_str(r, 0);
                if let Some(prefix) = path_prefix {
                    if !prefix.is_empty() && !file.starts_with(prefix) {
                        return None;
                    }
                }
                Some(crate::graph::gitmeta::FileChurn {
                    file,
                    last_ts,
                    last_subject: cell_str(r, 2),
                    touches_90d: cell_i64(r, 3) as u32,
                })
            })
            .collect();
        out.sort_by(|a, b| {
            b.touches_90d
                .cmp(&a.touches_90d)
                .then_with(|| b.last_ts.cmp(&a.last_ts))
                .then_with(|| a.file.cmp(&b.file))
        });
        out.truncate(max);
        Ok(out)
    }

    // ── V12 Phase F: small key/value store (`meta`) ──────────────────────
    //
    // Currently used only for the analyses-auto trigger's last-emitted counts
    // (`analyses_counts`, see `GraphService::run_analyses_trigger`) so a
    // rebuild only emits `graph-analyses` when the numbers actually changed —
    // generic rather than a bespoke relation since a future feature needing
    // one small persisted value can reuse it instead of adding another
    // `:create`.

    /// Read one `meta` key. `None` if unset (or the relation doesn't exist
    /// yet — defensive, `ensure_memory_relations` always creates it on open).
    pub fn get_meta(&self, key: &str) -> AppResult<Option<String>> {
        if !self.existing_relations()?.contains("meta") {
            return Ok(None);
        }
        let mut p = BTreeMap::new();
        p.insert("key".to_string(), DataValue::Str(key.into()));
        let rows = self.run(
            "?[value] := *meta{key: k, value}, k == $key",
            p,
            ScriptMutability::Immutable,
        )?;
        Ok(rows.rows.first().map(|r| cell_str(r, 0)))
    }

    /// Upsert one `meta` key.
    pub fn put_meta(&self, key: &str, value: &str) -> AppResult<()> {
        self.put(
            "?[key, value] <- $rows\n:put meta {key => value}",
            vec![DataValue::List(vec![
                DataValue::Str(key.into()),
                DataValue::Str(value.into()),
            ])],
        )
    }
}

/// Kind weight for working-set scoring: an edit outranks a query outranks a read.
fn kind_weight(kind: &str) -> f64 {
    match kind {
        "edit" => 3.0,
        "query" => 2.0,
        _ => 1.0,
    }
}

/// Evict sessions beyond [`MAX_SESSIONS_PER_ROOT`] (oldest `last_ms` first),
/// cascading each evicted session's events, usage rows (V14 Phase C), and
/// **unpinned** notes; its pinned notes and their rows survive project-wide.
fn prune_sessions_in_tx(tx: &MultiTransaction) -> AppResult<()> {
    let rows = tx_run(
        tx,
        "?[session_id, last_ms] := *session{session_id, last_ms}\n:order -last_ms",
        BTreeMap::new(),
    )?;
    let ids: Vec<String> = rows.rows.iter().map(|r| cell_str(r, 0)).collect();
    if ids.len() <= MAX_SESSIONS_PER_ROOT {
        return Ok(());
    }
    for sid in &ids[MAX_SESSIONS_PER_ROOT..] {
        let mut p = BTreeMap::new();
        p.insert("sid".to_string(), DataValue::Str(sid.as_str().into()));
        tx_run(tx, "?[session_id, seq] := *mem_event{session_id, seq}, session_id == $sid\n:rm mem_event {session_id, seq}", p.clone())?;
        tx_run(tx, "?[session_id, seq] := *usage_stat{session_id, seq}, session_id == $sid\n:rm usage_stat {session_id, seq}", p.clone())?;
        tx_run(tx, "?[note_id] := *mem_note{note_id, session_id, pinned}, session_id == $sid, pinned == false\n:rm mem_note {note_id}", p.clone())?;
        // F5: also drop the distilled-flag row. Without this it leaks one row per
        // evicted session forever, and — because a Claude `session_id` is the
        // transcript UUID (stable across `--resume`/`--continue`) — a resumed
        // session that was evicted would hit `is_session_distilled == true` and
        // the idle sweep would skip distilling all its NEW work.
        tx_run(tx, "?[session_id] := *session_distilled{session_id}, session_id == $sid\n:rm session_distilled {session_id}", p.clone())?;
        tx_run(tx, "?[session_id] := *session{session_id}, session_id == $sid\n:rm session {session_id}", p)?;
    }
    Ok(())
}

/// V12 Phase E: pure cap-enforcement decision for [`GraphIndex::add_project_fact`].
/// Given the current LIVE facts as `(fact_id, ts_ms, pinned)` and a live-count
/// `cap`, returns the fact_id to archive to bring the set back to the cap, or
/// `None` when already within it. Picks the oldest UNPINNED fact (ascending
/// `ts_ms`); if every live fact is pinned, returns `None` — the cap is simply
/// exceeded rather than archiving a pinned fact. DB-free so it's directly
/// unit-testable.
fn fact_to_archive_for_cap(live: &[(String, i64, bool)], cap: usize) -> Option<String> {
    if live.len() <= cap {
        return None;
    }
    live.iter()
        .filter(|(_, _, pinned)| !*pinned)
        .min_by_key(|(_, ts, _)| *ts)
        .map(|(id, _, _)| id.clone())
}

/// Read-modify-write one `project_fact` row's `archived` flag to `true`
/// inside an open transaction. Used by [`GraphIndex::add_project_fact`]'s cap
/// enforcement; a no-op if the id is somehow already gone (best-effort, same
/// tolerant posture as the read-modify-write helpers above).
fn archive_fact_in_tx(tx: &MultiTransaction, fact_id: &str) -> AppResult<()> {
    let mut p = BTreeMap::new();
    p.insert("fid".to_string(), DataValue::Str(fact_id.into()));
    let rows = tx_run(
        tx,
        "?[text, source_session, ts_ms, pinned] := \
            *project_fact{fact_id, text, source_session, ts_ms, pinned}, fact_id == $fid",
        p,
    )?;
    let Some(r) = rows.rows.first() else { return Ok(()) };
    let row = DataValue::List(vec![
        DataValue::Str(fact_id.into()),
        DataValue::Str(cell_str(r, 0).as_str().into()),
        DataValue::Str(cell_str(r, 1).as_str().into()),
        DataValue::Num(Num::Int(cell_i64(r, 2))),
        DataValue::Bool(cell_bool(r, 3)),
        DataValue::Bool(true),
    ]);
    tx_put(
        tx,
        "?[fact_id, text, source_session, ts_ms, pinned, archived] <- $rows\n\
         :put project_fact {fact_id => text, source_session, ts_ms, pinned, archived}",
        vec![row],
    )
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
/// refs, doc-chunks, code-chunks, the `file` row, edges keyed by the file
/// id-prefix or `src == file`, plus inbound call edges to names this file
/// uniquely defined). The transaction makes this atomic with the re-insert in
/// [`GraphIndex::index_file_graph`].
fn remove_file_in_tx(tx: &MultiTransaction, file: &str) -> AppResult<()> {
    let prefix = format!("{file}#");
    let mut p = BTreeMap::new();
    p.insert("file".to_string(), DataValue::Str(file.into()));

    // Names this file DEFINES, captured before we delete its symbols. After
    // deletion, any of these names with no remaining definition anywhere is
    // "dangling": its inbound call edges (owned by other files) would otherwise
    // survive and make `callers()` report ghosts of a symbol that no longer
    // exists. External/unresolved call targets (stdlib names that never had a
    // symbol) are NOT in this set, so they're left untouched.
    let defined = tx_run(tx, "?[name] := *symbol{file, name}, file == $file", p.clone())?;
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

    // Drop inbound call edges to names this file uniquely defined.
    for name in defined_names {
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

/// A nullable `String?` column: `None` for a stored `Null`/missing cell
/// (unlike [`cell_str`], which folds that case into `""`) — needed wherever
/// absence must stay distinguishable from an empty string (`usage_stat`'s
/// `model`/`msg_id`/`tool` columns).
fn cell_str_opt(row: &[DataValue], i: usize) -> Option<String> {
    match row.get(i) {
        None | Some(DataValue::Null) => None,
        Some(v) => Some(dv_string(v)),
    }
}

fn dv_f64(v: &DataValue) -> f64 {
    match v {
        DataValue::Num(Num::Float(f)) => *f,
        DataValue::Num(Num::Int(i)) => *i as f64,
        _ => 0.0,
    }
}

fn cell_f64(row: &[DataValue], i: usize) -> f64 {
    row.get(i).map(dv_f64).unwrap_or(0.0)
}

/// FNV-1a hash of a chunk's text — used to detect when a chunk's content
/// changed so its stored embedding is re-computed. Delegates to the shared
/// [`fnv1a_hex`] so it can never drift from the builder's file-content hash.
fn text_hash(s: &str) -> String {
    super::model::fnv1a_hex(s)
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
fn with_row_confidence(mut hit: SymbolHit, r: &[DataValue], conf_col: usize, ambiguous: bool) -> SymbolHit {
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
        visibility: if r.len() > 6 { cell_str(r, 6) } else { "unknown".to_string() },
        end_line: if r.len() > 7 { cell_i64(r, 7) as u32 } else { start_line },
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
fn resolve_import(lang: Lang, from_file: &str, module: &str, known: &HashSet<String>) -> Option<String> {
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
    for cand in [format!("{stem}.py"), format!("{stem}/__init__.py")] {
        if known.contains(&cand) {
            return Some(cand);
        }
    }
    None
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::{parse_file, Lang};

    const SRC: &str = r#"
/// Adds two numbers.
pub fn add(a: i32, b: i32) -> i32 { helper(a) + b }
fn helper(x: i32) -> i32 { x * 2 }
pub struct Point { x: i32 }
"#;

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
        let at = idx.symbol_at("src/geo.rs", hits[0].start_line).expect("symbol_at");
        assert_eq!(at.as_ref().map(|s| s.name.as_str()), Some("add"));
        // The blank first line encloses no definition.
        assert!(idx.symbol_at("src/geo.rs", 1).expect("symbol_at blank").is_none());
        // callers_count: add calls helper → helper has ≥1 caller; add has none.
        assert!(idx.callers_count("helper").expect("cc helper") >= 1);
        assert_eq!(idx.callers_count("add").expect("cc add"), 0);
        // stored_file_hash returns the indexed content hash.
        assert_eq!(idx.stored_file_hash("src/geo.rs").expect("hash"), Some(fg.hash.clone()));

        let stats = idx.stats().expect("stats");
        assert_eq!(stats.files, 1);
        assert!(stats.symbols >= 3);
        assert!(stats.edges >= 1);

        drop(idx);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn digest_cache_roundtrip_and_orphan_prune() {
        let dir = std::env::temp_dir().join(format!("ckg-digest-{}", uuid::Uuid::new_v4()));
        let idx = GraphIndex::open(&dir, ".ckg").expect("open");
        idx.index_file_graph(&parse_file("src/a.rs", "pub fn f() {}\n", Lang::Rust))
            .expect("index");

        // Miss, then hit; a different hash is a miss (stale content).
        assert!(idx.get_digest("src/a.rs", "h1").unwrap().is_none());
        idx.put_digest("src/a.rs", "h1", "a three line digest", 100).unwrap();
        assert_eq!(idx.get_digest("src/a.rs", "h1").unwrap().as_deref(), Some("a three line digest"));
        assert!(idx.get_digest("src/a.rs", "h2").unwrap().is_none());
        assert_eq!(idx.digest_count().unwrap(), 1);

        // F11: re-digesting the SAME file under a new hash supersedes the old
        // row rather than leaking it — the count stays 1 and the stale hash is
        // no longer a hit.
        idx.put_digest("src/a.rs", "h2", "updated digest", 200).unwrap();
        assert_eq!(idx.digest_count().unwrap(), 1, "one digest per file, not per edit");
        assert!(idx.get_digest("src/a.rs", "h1").unwrap().is_none(), "old hash superseded");
        assert_eq!(idx.get_digest("src/a.rs", "h2").unwrap().as_deref(), Some("updated digest"));

        // A digest for a file no longer indexed is pruned; the live one stays.
        idx.put_digest("gone.rs", "hx", "orphan", 100).unwrap();
        assert_eq!(idx.digest_count().unwrap(), 2);
        assert_eq!(idx.prune_orphan_digests().unwrap(), 1);
        assert_eq!(idx.digest_count().unwrap(), 1);
        assert!(idx.get_digest("gone.rs", "hx").unwrap().is_none());

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
        idx.index_file_graph(&parse_file("src/dup.rs", "pub fn run() {}\npub fn run() {}\n", Lang::Rust))
            .expect("index dup");
        idx.index_file_graph(&parse_file("src/caller.rs", "pub fn c() { run() }\n", Lang::Rust))
            .expect("index caller");

        let central = idx.file_centrality(10).expect("centrality");
        let dup = central.iter().find(|(f, _)| f == "src/dup.rs").map(|(_, c)| *c).unwrap_or(0);
        assert_eq!(dup, 1, "one call edge counts once despite two same-named defs");

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
        assert!(outline.windows(2).all(|w| w[0].start_line <= w[1].start_line));

        // transitive: add -> helper (forward), and helper <- add (backward).
        let reach = idx.transitive("add", true).expect("transitive");
        assert!(reach.contains(&"helper".to_string()));
        let back = idx.transitive("helper", false).expect("transitive back");
        assert!(back.contains(&"add".to_string()), "add transitively calls helper");

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

        let hits = idx.dependents_transitive(&["a".to_string()], 3, 100).expect("dependents");
        assert_eq!(hits.len(), 2, "{hits:?}");
        let b = hits.iter().find(|h| h.symbol.name == "b").expect("b present");
        let c = hits.iter().find(|h| h.symbol.name == "c").expect("c present");
        assert_eq!(b.depth, 1);
        assert_eq!(c.depth, 2);
        assert!(b.approx && c.approx, "every hit is approximate by construction");
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
        let capped = idx.dependents_transitive(&["a".to_string()], 1, 100).expect("dependents");
        assert_eq!(capped.len(), 1);
        assert_eq!(capped[0].symbol.name, "b");

        // max=1 truncates even though depth would reach both.
        let truncated = idx.dependents_transitive(&["a".to_string()], 6, 1).expect("dependents");
        assert_eq!(truncated.len(), 1);

        // A depth passed as 0 (or absurdly high) is clamped into 1..=6, not
        // rejected — 0 still finds the direct caller.
        let clamped = idx.dependents_transitive(&["a".to_string()], 0, 100).expect("dependents");
        assert_eq!(clamped.len(), 1);
        assert_eq!(clamped[0].symbol.name, "b");

        // Empty roots is an empty result, not a query error.
        assert!(idx.dependents_transitive(&[], 3, 100).expect("dependents").is_empty());

        // A root that's also a caller of another root doesn't get reported as
        // its own dependent.
        let both_roots = idx
            .dependents_transitive(&["a".to_string(), "b".to_string()], 3, 100)
            .expect("dependents");
        assert!(!both_roots.iter().any(|h| h.symbol.name == "a" || h.symbol.name == "b"));
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
        idx.index_file_graph(&parse_file("src/chain.rs", src, Lang::Rust)).expect("index");

        let hits = idx.tests_for(&["one".to_string()], 3, 100).expect("tests_for");
        assert_eq!(hits.len(), 1, "{hits:?}");
        assert_eq!(hits[0].name, "test_it");
        assert!(hits[0].is_test);
        assert!(!hits.iter().any(|s| s.name == "plain_caller"), "non-test caller excluded: {hits:?}");
        assert!(!hits.iter().any(|s| s.name == "two"), "the intermediate non-test hop is excluded: {hits:?}");

        // Depth 1 only reaches `two` (not a test), so no tests found yet.
        let shallow = idx.tests_for(&["one".to_string()], 1, 100).expect("tests_for");
        assert!(shallow.is_empty(), "{shallow:?}");

        drop(idx);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn semantic_vector_store_roundtrip() {
        // Exercises the CozoDB HNSW path end-to-end with deterministic vectors
        // (no embedding endpoint needed).
        let dir = std::env::temp_dir().join(format!("ckg-vec-{}", uuid::Uuid::new_v4()));
        let idx = GraphIndex::open(&dir, ".ckg").expect("open");

        // Two doc chunks via a tiny markdown file.
        let md = "# Cats\n\nFelines purr.\n\n# Engines\n\nMotors combust fuel.\n";
        idx.index_file_graph(&parse_file("docs/a.md", md, Lang::Markdown))
            .expect("index md");

        let epoch = "test-epoch";
        let reset = idx.ensure_vector_store(3, "fake-model", epoch).expect("ensure");
        assert!(!reset, "fresh store isn't a reset");

        // Every chunk needs a vector initially.
        let need = idx.chunks_needing_vectors(epoch, 100).expect("need");
        assert_eq!(need.len(), 2);

        // Assign deterministic 3-d vectors: "cats" near [1,0,0], "engines" near [0,1,0].
        let vecs: Vec<(String, String, Vec<f32>)> = need
            .iter()
            .map(|(id, hash, text)| {
                let v = if text.to_lowercase().contains("felines") {
                    vec![1.0, 0.0, 0.0]
                } else {
                    vec![0.0, 1.0, 0.0]
                };
                (id.clone(), hash.clone(), v)
            })
            .collect();
        idx.put_doc_vectors(epoch, &vecs).expect("put vecs");

        // Coverage is now full; nothing left to embed.
        assert_eq!(idx.embedding_coverage(epoch).expect("cov"), (2, 2));
        assert!(idx.chunks_needing_vectors(epoch, 100).expect("need2").is_empty());

        // A query near [1,0,0] returns the "Cats" chunk first.
        let hits = idx
            .semantic_doc_search(&[0.9, 0.1, 0.0], epoch, 2, 200)
            .expect("search");
        assert!(!hits.is_empty());
        assert_eq!(hits[0].0.anchor, "cats");

        drop(idx);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn clear_vectors_drops_hnsw_indexed_store() {
        // Regression: `clear_vectors` (Rebuild embeddings) and a dim-change must
        // drop the HNSW index before `::remove`-ing `doc_vec` — CozoDB rejects
        // removing a relation that still has an index attached.
        let dir = std::env::temp_dir().join(format!("ckg-clr-{}", uuid::Uuid::new_v4()));
        let idx = GraphIndex::open(&dir, ".ckg").expect("open");

        idx.index_file_graph(&parse_file("docs/a.md", "# Cats\n\nFelines purr.\n", Lang::Markdown))
            .expect("index");

        let epoch = "e1";
        idx.ensure_vector_store(3, "m", epoch).expect("ensure");
        let need = idx.chunks_needing_vectors(epoch, 100).expect("need");
        let vecs: Vec<(String, String, Vec<f32>)> = need
            .iter()
            .map(|(id, h, _)| (id.clone(), h.clone(), vec![1.0, 0.0, 0.0]))
            .collect();
        idx.put_doc_vectors(epoch, &vecs).expect("put");

        // The HNSW index is attached now — clearing must not error.
        idx.clear_vectors().expect("clear_vectors with index attached");
        assert!(!idx.existing_relations().unwrap().contains("doc_vec"));

        // A dim change re-exercises the same drop path inside ensure_vector_store.
        idx.ensure_vector_store(3, "m", epoch).expect("recreate");
        let reset = idx.ensure_vector_store(4, "m", "e2").expect("dim change");
        assert!(reset, "a dim change is a reset");

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
        idx.index_file_graph(&parse_file("src/b.rs", "pub fn bar() { baz() }\n", Lang::Rust))
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
    fn prune_orphan_vectors_drops_deleted_chunks() {
        let dir = std::env::temp_dir().join(format!("ckg-prune-{}", uuid::Uuid::new_v4()));
        let idx = GraphIndex::open(&dir, ".ckg").expect("open");
        idx.index_file_graph(&parse_file(
            "docs/a.md",
            "# Cats\n\nFelines.\n\n# Dogs\n\nCanines.\n",
            Lang::Markdown,
        ))
        .expect("index md");

        let epoch = "e";
        idx.ensure_vector_store(3, "m", epoch).expect("ensure");
        let need = idx.chunks_needing_vectors(epoch, 100).expect("need");
        assert_eq!(need.len(), 2);
        let vecs: Vec<(String, String, Vec<f32>)> = need
            .iter()
            .map(|(id, h, _)| (id.clone(), h.clone(), vec![1.0, 0.0, 0.0]))
            .collect();
        idx.put_doc_vectors(epoch, &vecs).expect("put");
        assert_eq!(idx.embedding_coverage(epoch).unwrap(), (2, 2));

        // Delete the file's chunks: doc_chunk rows go, doc_vec rows orphan.
        // Coverage would now read "embedded > total" (the false-100% bug).
        idx.remove_file("docs/a.md").expect("remove");
        assert_eq!(idx.embedding_coverage(epoch).unwrap(), (2, 0));

        // Pruning drops exactly the orphans and restores accurate coverage.
        assert_eq!(idx.prune_orphan_vectors().unwrap(), 2);
        assert_eq!(idx.embedding_coverage(epoch).unwrap(), (0, 0));

        drop(idx);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn prune_keeps_still_valid_vectors() {
        // Pruning must NOT force a re-embed of chunks that still exist.
        let dir = std::env::temp_dir().join(format!("ckg-keep-{}", uuid::Uuid::new_v4()));
        let idx = GraphIndex::open(&dir, ".ckg").expect("open");
        idx.index_file_graph(&parse_file("docs/a.md", "# Cats\n\nFelines.\n", Lang::Markdown))
            .expect("index md");
        let epoch = "e";
        idx.ensure_vector_store(3, "m", epoch).expect("ensure");
        let need = idx.chunks_needing_vectors(epoch, 100).expect("need");
        let vecs: Vec<(String, String, Vec<f32>)> = need
            .iter()
            .map(|(id, h, _)| (id.clone(), h.clone(), vec![1.0, 0.0, 0.0]))
            .collect();
        idx.put_doc_vectors(epoch, &vecs).expect("put");
        assert_eq!(idx.prune_orphan_vectors().unwrap(), 0, "nothing orphaned");
        assert!(idx.chunks_needing_vectors(epoch, 100).unwrap().is_empty());

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
        assert!(!ids.iter().any(|i| i.contains("N@")), "const not chunked: {ids:?}");
        assert!(!ids.iter().any(|i| i.contains("short@")), "1-line fn not chunked: {ids:?}");
        let chunk = fg
            .code_chunks
            .iter()
            .find(|c| c.id.contains("long_fn@"))
            .expect("long_fn chunked");
        assert_eq!(chunk.file, "src/a.rs");
        assert!(chunk.text.contains("let c = a + b"), "body present: {}", chunk.text);
    }

    #[test]
    fn semantic_code_vector_store_roundtrip() {
        // Exercises the code_vec HNSW path end-to-end with deterministic
        // vectors (no embedding endpoint needed) — the code twin of
        // `semantic_vector_store_roundtrip`.
        let dir = std::env::temp_dir().join(format!("ckg-codevec-{}", uuid::Uuid::new_v4()));
        let idx = GraphIndex::open(&dir, ".ckg").expect("open");

        let src = "pub fn add_numbers(a: i32, b: i32) -> i32 {\n    let sum = a + b;\n    sum\n}\n\
                   pub fn multiply_numbers(a: i32, b: i32) -> i32 {\n    let prod = a * b;\n    prod\n}\n";
        idx.index_file_graph(&parse_file("src/math.rs", src, Lang::Rust))
            .expect("index");

        let epoch = "code-epoch";
        let reset = idx.ensure_code_vector_store(3, "fake-model", epoch).expect("ensure");
        assert!(!reset, "fresh store isn't a reset");

        let need = idx.pending_code_chunks(epoch, 100).expect("need");
        assert_eq!(need.len(), 2);

        // Deterministic 3-d vectors: "add_numbers" near [1,0,0], the other near [0,1,0].
        let vecs: Vec<(String, String, Vec<f32>)> = need
            .iter()
            .map(|(id, hash, text)| {
                let v = if text.contains("add_numbers") {
                    vec![1.0, 0.0, 0.0]
                } else {
                    vec![0.0, 1.0, 0.0]
                };
                (id.clone(), hash.clone(), v)
            })
            .collect();
        idx.put_code_vectors(epoch, &vecs).expect("put vecs");

        assert_eq!(idx.code_embedding_coverage(epoch).expect("cov"), (2, 2));
        assert!(idx.pending_code_chunks(epoch, 100).expect("need2").is_empty());

        let hits = idx
            .semantic_code_search(&[0.9, 0.1, 0.0], epoch, 2)
            .expect("search");
        assert!(!hits.is_empty());
        assert_eq!(hits[0].0.name, "add_numbers");

        drop(idx);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn prune_orphan_code_vectors_drops_deleted_chunks() {
        let dir = std::env::temp_dir().join(format!("ckg-codeprune-{}", uuid::Uuid::new_v4()));
        let idx = GraphIndex::open(&dir, ".ckg").expect("open");
        let src = "pub fn add_numbers(a: i32, b: i32) -> i32 {\n    let sum = a + b;\n    sum\n}\n";
        idx.index_file_graph(&parse_file("src/math.rs", src, Lang::Rust))
            .expect("index");

        let epoch = "e";
        idx.ensure_code_vector_store(3, "m", epoch).expect("ensure");
        let need = idx.pending_code_chunks(epoch, 100).expect("need");
        assert_eq!(need.len(), 1);
        let vecs: Vec<(String, String, Vec<f32>)> = need
            .iter()
            .map(|(id, h, _)| (id.clone(), h.clone(), vec![1.0, 0.0, 0.0]))
            .collect();
        idx.put_code_vectors(epoch, &vecs).expect("put");
        assert_eq!(idx.code_embedding_coverage(epoch).unwrap(), (1, 1));

        // Delete the file's chunks: code_chunk rows go, code_vec rows orphan.
        idx.remove_file("src/math.rs").expect("remove");
        assert_eq!(idx.code_embedding_coverage(epoch).unwrap(), (1, 0));

        assert_eq!(idx.prune_orphan_code_vectors().unwrap(), 1);
        assert_eq!(idx.code_embedding_coverage(epoch).unwrap(), (0, 0));

        drop(idx);
        let _ = std::fs::remove_dir_all(&dir);
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
            assert!(!peek.find_symbol("f").unwrap().is_empty(), "open_existing must not wipe");
        }

        // The writable path migrates: resets (empties) + flags exactly once.
        let idx2 = GraphIndex::open(&dir, ".ckg").expect("reopen");
        assert!(idx2.take_schema_reset(), "migration flags a rebuild");
        assert!(!idx2.take_schema_reset(), "flag is one-shot");
        assert!(idx2.find_symbol("f").unwrap().is_empty(), "stale rows were reset");

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
        assert!(names.contains(&"unused_pub"), "unused public fn is a candidate");
        assert!(!names.contains(&"used_pub"), "a called fn is not dead");
        assert!(!names.contains(&"priv_fn"), "a private fn is never a dead export");
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
        idx.index_file_graph(&parse_file("src/a.ts", "import { x } from './b';\nexport const y = 1;\n", Lang::TypeScript))
            .expect("a");
        idx.index_file_graph(&parse_file("src/b.ts", "import { y } from './a';\nexport const x = 1;\n", Lang::TypeScript))
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
        idx.index_file_graph(&parse_file("src/a.ts", "import { x } from './b';\n", Lang::TypeScript))
            .expect("a");
        idx.index_file_graph(&parse_file("src/b.ts", "export const x = 1;\n", Lang::TypeScript))
            .expect("b");
        assert!(idx.import_cycles(50).expect("cycles").is_empty());
        // An unresolvable/external import must not crash.
        idx.index_file_graph(&parse_file("src/c.ts", "import fs from 'node:fs';\n", Lang::TypeScript))
            .expect("c");
        assert!(idx.import_cycles(50).expect("cycles2").is_empty());

        drop(idx);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn memory_events_notes_and_ranking() {
        let dir = std::env::temp_dir().join(format!("ckg-mem-{}", uuid::Uuid::new_v4()));
        let idx = GraphIndex::open(&dir, ".ckg").expect("open");

        // Session s1: read a.rs (t=100), then edit b.rs twice (t=200,300).
        idx.record_mem_event("s1", "claude", "read", "a.rs", None, None, 100, None).unwrap();
        idx.record_mem_event("s1", "claude", "edit", "b.rs", Some("foo"), Some(3), 200, None).unwrap();
        idx.record_mem_event("s1", "claude", "edit", "b.rs", Some("bar"), Some(9), 300, None).unwrap();

        assert_eq!(idx.mem_current_session().unwrap().as_deref(), Some("s1"));

        let ws = idx.mem_working_set("s1", 10).unwrap();
        assert_eq!(ws.len(), 2);
        // b.rs (2 edits, weight 3) outranks a.rs (1 read, weight 1).
        assert_eq!(ws[0].path, "b.rs");
        assert_eq!(ws[0].touches, 2);
        assert_eq!(ws[0].last_kind, "edit");
        // Most-recent symbol first, deduped.
        assert_eq!(ws[0].top_symbols, vec!["bar".to_string(), "foo".to_string()]);
        assert_eq!(ws[1].path, "a.rs");

        // A later session s2 becomes current.
        idx.record_mem_event("s2", "opencode", "read", "c.rs", None, None, 400, None).unwrap();
        assert_eq!(idx.mem_current_session().unwrap().as_deref(), Some("s2"));

        // Notes: a pinned note is visible from any session; unpinned only its own.
        let n1 = "note-1";
        idx.mem_add_note(n1, "s1", "use FNV hashing", 250, true).unwrap();
        idx.mem_add_note("note-2", "s1", "s1-only detail", 260, false).unwrap();
        let s2_notes = idx.mem_notes("s2").unwrap();
        assert!(s2_notes.iter().any(|n| n.note_id == n1), "pinned note crosses sessions");
        assert!(!s2_notes.iter().any(|n| n.note_id == "note-2"), "unpinned note stays in its session");

        let sessions = idx.mem_sessions().unwrap();
        assert_eq!(sessions.len(), 2);
        assert_eq!(sessions[0].session_id, "s2"); // newest first
        assert!(sessions.iter().find(|s| s.session_id == "s1").unwrap().events >= 3);

        drop(idx);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn memory_current_session_scopes_by_agent() {
        let dir = std::env::temp_dir().join(format!("ckg-agent-{}", uuid::Uuid::new_v4()));
        let idx = GraphIndex::open(&dir, ".ckg").expect("open");
        // A Claude session (older) and an OpenCode session (more recent) on the
        // same project.
        idx.record_mem_event("c1", "claude", "read", "a.rs", None, None, 100, None).unwrap();
        idx.record_mem_event("o1", "opencode", "read", "b.rs", None, None, 200, None).unwrap();

        // Unscoped picks the globally most recent (OpenCode's).
        assert_eq!(idx.mem_current_session().unwrap().as_deref(), Some("o1"));
        // Agent-scoped resolves each agent's own session — no cross-talk, even
        // though the OpenCode session is newer.
        assert_eq!(idx.mem_current_session_for(Some("claude")).unwrap().as_deref(), Some("c1"));
        assert_eq!(idx.mem_current_session_for(Some("opencode")).unwrap().as_deref(), Some("o1"));
        // An agent with no sessions yet resolves to None.
        assert_eq!(idx.mem_current_session_for(Some("nobody")).unwrap(), None);

        drop(idx);
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ── V14 Phase C: usage_stat store ──────────────────────────────────────

    #[test]
    fn usage_turn_upserts_by_msg_id_instead_of_duplicating() {
        let dir = std::env::temp_dir().join(format!("ckg-usage-upsert-{}", uuid::Uuid::new_v4()));
        let idx = GraphIndex::open(&dir, ".ckg").expect("open");

        // A partial line (usage not yet firmed up) ...
        idx.record_usage_event(
            "s1",
            "claude",
            &UsageEvent::Turn {
                msg_id: "m1".to_string(),
                model: None,
                in_tok: 0,
                out_tok: 0,
                cache_read: 0,
                cache_make: 0,
            },
            100,
        )
        .unwrap();
        // ... then the SAME message id with the real numbers.
        idx.record_usage_event(
            "s1",
            "claude",
            &UsageEvent::Turn {
                msg_id: "m1".to_string(),
                model: Some("claude-x".to_string()),
                in_tok: 120,
                out_tok: 30,
                cache_read: 40,
                cache_make: 5,
            },
            110,
        )
        .unwrap();

        let series = idx.usage_turn_series("s1").unwrap();
        assert_eq!(series.len(), 1, "same msg_id must upsert in place, not duplicate");
        assert_eq!(series[0].msg_id, "m1");
        assert_eq!(series[0].model.as_deref(), Some("claude-x"));
        assert_eq!(series[0].in_tok, 120);
        assert_eq!(series[0].out_tok, 30);
        assert_eq!(series[0].cache_read, 40);
        assert_eq!(series[0].cache_make, 5);

        let totals = idx.usage_session_totals("s1").unwrap();
        assert_eq!(totals.in_tok, 120, "totals reflect the upserted (last) value, not both writes summed");

        drop(idx);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn usage_per_tool_and_turn_series_join_tool_results_to_the_following_turn() {
        let dir = std::env::temp_dir().join(format!("ckg-usage-join-{}", uuid::Uuid::new_v4()));
        let idx = GraphIndex::open(&dir, ".ckg").expect("open");

        // Turn 1 (the tool-calling message) ...
        idx.record_usage_event(
            "s1",
            "claude",
            &UsageEvent::Turn {
                msg_id: "t1".to_string(),
                model: Some("m".to_string()),
                in_tok: 10,
                out_tok: 5,
                cache_read: 0,
                cache_make: 0,
            },
            100,
        )
        .unwrap();
        // ... then its tool result arrives (chars attributed to the NEXT turn) ...
        idx.record_usage_event(
            "s1",
            "claude",
            &UsageEvent::ToolResult { tool: Some("Read".to_string()), chars: 500 },
            110,
        )
        .unwrap();
        idx.record_usage_event(
            "s1",
            "claude",
            &UsageEvent::ToolResult { tool: Some("Read".to_string()), chars: 300 },
            111,
        )
        .unwrap();
        // ... then turn 2 (which "saw" that tool output as its input context).
        idx.record_usage_event(
            "s1",
            "claude",
            &UsageEvent::Turn {
                msg_id: "t2".to_string(),
                model: Some("m".to_string()),
                in_tok: 800,
                out_tok: 20,
                cache_read: 0,
                cache_make: 0,
            },
            120,
        )
        .unwrap();

        let per_tool = idx.usage_per_tool("s1").unwrap();
        assert_eq!(per_tool, vec![("Read".to_string(), 800)]);

        let series = idx.usage_turn_series("s1").unwrap();
        assert_eq!(series.len(), 2);
        assert_eq!(series[0].msg_id, "t1");
        assert_eq!(series[0].tool_chars, 0, "no tool results before the first turn");
        assert_eq!(series[1].msg_id, "t2");
        assert_eq!(series[1].tool_chars, 800, "both Read results attributed to the turn that followed them");

        drop(idx);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn usage_per_tool_buckets_unjoined_results_as_unknown() {
        let dir = std::env::temp_dir().join(format!("ckg-usage-unknown-{}", uuid::Uuid::new_v4()));
        let idx = GraphIndex::open(&dir, ".ckg").expect("open");
        idx.record_usage_event("s1", "claude", &UsageEvent::ToolResult { tool: None, chars: 42 }, 100)
            .unwrap();
        assert_eq!(idx.usage_per_tool("s1").unwrap(), vec![("unknown".to_string(), 42)]);
        drop(idx);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn usage_all_sessions_reports_totals_cache_ratio_and_est_only() {
        let dir = std::env::temp_dir().join(format!("ckg-usage-allsess-{}", uuid::Uuid::new_v4()));
        let idx = GraphIndex::open(&dir, ".ckg").expect("open");

        idx.record_usage_event(
            "c1",
            "claude",
            &UsageEvent::Turn {
                msg_id: "m1".to_string(),
                model: None,
                in_tok: 100,
                out_tok: 10,
                cache_read: 50,
                cache_make: 0,
            },
            100,
        )
        .unwrap();
        idx.record_usage_event(
            "o1",
            "opencode",
            &UsageEvent::ToolResult { tool: Some("edit".to_string()), chars: 20 },
            200,
        )
        .unwrap();

        let rows = idx.usage_all_sessions().unwrap();
        let claude = rows.iter().find(|r| r.session_id == "c1").expect("c1 present");
        assert!(!claude.est_only, "claude sessions carry exact usage");
        assert_eq!(claude.totals.in_tok, 100);
        // cache_read / (cache_read + in_tok) = 50 / 150.
        assert!((claude.cache_hit_ratio - (50.0 / 150.0)).abs() < 1e-9);

        let opencode = rows.iter().find(|r| r.session_id == "o1").expect("o1 present");
        assert!(opencode.est_only, "opencode sessions have no exact usage — always est-only");
        assert_eq!(opencode.totals.in_tok, 0, "a tool_result-only session has zero token totals");
        assert_eq!(opencode.tool_chars, 20);
        assert_eq!(opencode.cache_hit_ratio, 0.0, "no denominator ⇒ 0.0, not NaN");

        drop(idx);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn usage_ring_prunes_beyond_the_per_session_cap() {
        let dir = std::env::temp_dir().join(format!("ckg-usage-ring-{}", uuid::Uuid::new_v4()));
        let idx = GraphIndex::open(&dir, ".ckg").expect("open");

        let total = MAX_USAGE_PER_SESSION + 5;
        for i in 0..total {
            idx.record_usage_event(
                "s1",
                "claude",
                &UsageEvent::ToolResult { tool: Some("Bash".to_string()), chars: 1 },
                100 + i,
            )
            .unwrap();
        }
        let per_tool = idx.usage_per_tool("s1").unwrap();
        let (_, chars) = per_tool.into_iter().find(|(t, _)| t == "Bash").expect("Bash present");
        assert_eq!(
            chars, MAX_USAGE_PER_SESSION as u64,
            "the ring keeps exactly the cap's worth of rows (1 char each), not `total`"
        );

        drop(idx);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn usage_stat_is_evicted_with_its_session() {
        let dir = std::env::temp_dir().join(format!("ckg-usage-evict-{}", uuid::Uuid::new_v4()));
        let idx = GraphIndex::open(&dir, ".ckg").expect("open");

        // One usage row for a session that will be evicted (session s0), then
        // MAX_SESSIONS_PER_ROOT newer sessions push it out.
        idx.record_usage_event(
            "s0",
            "claude",
            &UsageEvent::ToolResult { tool: Some("Read".to_string()), chars: 99 },
            0,
        )
        .unwrap();
        for i in 0..MAX_SESSIONS_PER_ROOT {
            let sid = format!("s{}", i + 1);
            idx.record_usage_event(
                &sid,
                "claude",
                &UsageEvent::ToolResult { tool: Some("Read".to_string()), chars: 1 },
                1000 + i as i64,
            )
            .unwrap();
        }

        assert!(idx.usage_per_tool("s0").unwrap().is_empty(), "s0's usage rows were cascaded away");
        assert!(
            !idx.mem_sessions().unwrap().iter().any(|s| s.session_id == "s0"),
            "s0 itself was evicted"
        );

        drop(idx);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn mem_clear_also_drops_usage_stat() {
        let dir = std::env::temp_dir().join(format!("ckg-usage-clear-{}", uuid::Uuid::new_v4()));
        let idx = GraphIndex::open(&dir, ".ckg").expect("open");
        idx.record_usage_event(
            "s1",
            "claude",
            &UsageEvent::ToolResult { tool: Some("Read".to_string()), chars: 10 },
            100,
        )
        .unwrap();
        idx.mem_clear(Some("s1")).unwrap();
        assert!(idx.usage_per_tool("s1").unwrap().is_empty());
        drop(idx);
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ── V14 Phase D/D2: Usage section + advisor signal queries ─────────────

    #[test]
    fn usage_tool_ranking_reports_chars_and_call_counts() {
        let dir = std::env::temp_dir().join(format!("ckg-toolrank-{}", uuid::Uuid::new_v4()));
        let idx = GraphIndex::open(&dir, ".ckg").expect("open");
        for chars in [100, 200] {
            idx.record_usage_event(
                "s1",
                "claude",
                &UsageEvent::ToolResult { tool: Some("Read".to_string()), chars },
                100,
            )
            .unwrap();
        }
        idx.record_usage_event(
            "s1",
            "claude",
            &UsageEvent::ToolResult { tool: Some("Bash".to_string()), chars: 50 },
            100,
        )
        .unwrap();
        let ranking = idx.usage_tool_ranking("s1").unwrap();
        assert_eq!(ranking, vec![("Read".to_string(), 300, 2), ("Bash".to_string(), 50, 1)]);
        drop(idx);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn session_agent_reports_the_upserted_tag() {
        let dir = std::env::temp_dir().join(format!("ckg-sessagent-{}", uuid::Uuid::new_v4()));
        let idx = GraphIndex::open(&dir, ".ckg").expect("open");
        assert_eq!(idx.session_agent("s1").unwrap(), None, "unknown session");
        idx.record_mem_event("s1", "opencode", "read", "a.rs", None, None, 100, None).unwrap();
        assert_eq!(idx.session_agent("s1").unwrap(), Some("opencode".to_string()));
        drop(idx);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn mem_touched_paths_covers_read_and_edit_only() {
        let dir = std::env::temp_dir().join(format!("ckg-touched-{}", uuid::Uuid::new_v4()));
        let idx = GraphIndex::open(&dir, ".ckg").expect("open");
        idx.record_mem_event("s1", "claude", "read", "a.rs", None, None, 100, None).unwrap();
        idx.record_mem_event("s1", "claude", "edit", "b.rs", None, None, 200, None).unwrap();
        idx.record_mem_event("s1", "claude", "query", "c.rs", None, None, 300, None).unwrap();
        idx.record_mem_event("s1", "claude", "remind", "d.rs", None, None, 400, None).unwrap();
        let touched = idx.mem_touched_paths("s1").unwrap();
        assert_eq!(touched, ["a.rs".to_string(), "b.rs".to_string()].into_iter().collect());
        drop(idx);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn advisor_reread_rate_is_none_without_any_reminder() {
        let dir = std::env::temp_dir().join(format!("ckg-reread-none-{}", uuid::Uuid::new_v4()));
        let idx = GraphIndex::open(&dir, ".ckg").expect("open");
        assert_eq!(idx.advisor_reread_rate().unwrap(), None);
        drop(idx);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn advisor_reread_rate_counts_reads_strictly_after_the_reminder() {
        let dir = std::env::temp_dir().join(format!("ckg-reread-{}", uuid::Uuid::new_v4()));
        let idx = GraphIndex::open(&dir, ".ckg").expect("open");
        // s1/a.rs: reminded, then genuinely re-read afterward -> counts.
        idx.record_mem_event("s1", "claude", "remind", "a.rs", None, None, 100, None).unwrap();
        idx.record_mem_event("s1", "claude", "read", "a.rs", None, None, 200, None).unwrap();
        // s1/b.rs: reminded, only read BEFORE (stale/irrelevant) -> doesn't count.
        idx.record_mem_event("s1", "claude", "read", "b.rs", None, None, 50, None).unwrap();
        idx.record_mem_event("s1", "claude", "remind", "b.rs", None, None, 100, None).unwrap();
        // s2/c.rs: reminded, never read again -> doesn't count.
        idx.record_mem_event("s2", "claude", "remind", "c.rs", None, None, 100, None).unwrap();

        let (rate, samples) = idx.advisor_reread_rate().unwrap().unwrap();
        assert_eq!(samples, 3);
        assert!((rate - (1.0 / 3.0)).abs() < 1e-9, "rate={rate}");
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

    // ── V12 Phase D: commit_touch store ───────────────────────────────────

    #[test]
    fn commit_touch_roundtrip_and_incremental_overwrite() {
        let dir = std::env::temp_dir().join(format!("ckg-churn-{}", uuid::Uuid::new_v4()));
        let idx = GraphIndex::open(&dir, ".ckg").expect("open");

        // No row yet.
        assert!(idx.commit_touch("src/a.rs").unwrap().is_none());

        // A full-pass style upsert.
        idx.put_commit_touches(&[crate::graph::gitmeta::FileChurn {
            file: "src/a.rs".to_string(),
            last_ts: 1_000,
            last_subject: "init: a".to_string(),
            touches_90d: 3,
        }])
        .expect("put");
        let (ts, subject, touches) = idx.commit_touch("src/a.rs").unwrap().expect("row present");
        assert_eq!((ts, subject.as_str(), touches), (1_000, "init: a", 3));

        // An incremental (collect_for-shaped) upsert overwrites the row —
        // same key, new values win, nothing lingers from the old row.
        idx.put_commit_touches(&[crate::graph::gitmeta::FileChurn {
            file: "src/a.rs".to_string(),
            last_ts: 2_000,
            last_subject: "fix: a".to_string(),
            touches_90d: 1,
        }])
        .expect("put incremental");
        let (ts2, subject2, touches2) = idx.commit_touch("src/a.rs").unwrap().expect("row present");
        assert_eq!((ts2, subject2.as_str(), touches2), (2_000, "fix: a", 1));

        drop(idx);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn recent_changes_orders_by_touches_then_recency_and_filters() {
        let dir = std::env::temp_dir().join(format!("ckg-recent-{}", uuid::Uuid::new_v4()));
        let idx = GraphIndex::open(&dir, ".ckg").expect("open");
        let now = crate::graph::gitmeta::now_ts();
        let day = 86_400;

        idx.put_commit_touches(&[
            // Most touches, but older than the highest-touch tie below.
            crate::graph::gitmeta::FileChurn {
                file: "src/hot.rs".to_string(),
                last_ts: now - 2 * day,
                last_subject: "hot file".to_string(),
                touches_90d: 5,
            },
            // Fewer touches — should rank below hot.rs despite being newer.
            crate::graph::gitmeta::FileChurn {
                file: "src/warm.rs".to_string(),
                last_ts: now - day,
                last_subject: "warm file".to_string(),
                touches_90d: 2,
            },
            // Outside the `days` window entirely — excluded regardless of
            // touch count.
            crate::graph::gitmeta::FileChurn {
                file: "src/stale.rs".to_string(),
                last_ts: now - 400 * day,
                last_subject: "ancient".to_string(),
                touches_90d: 99,
            },
            // Matches the prefix filter test below.
            crate::graph::gitmeta::FileChurn {
                file: "docs/readme.md".to_string(),
                last_ts: now - day,
                last_subject: "docs touch".to_string(),
                touches_90d: 5,
            },
        ])
        .expect("put");

        // Default window (30d): stale.rs excluded; hot.rs (5 touches) ranks
        // above both 5-touch docs/readme.md (tie-broken by recency: readme is
        // newer) and warm.rs (2 touches).
        let rows = idx.recent_changes(30, None, 10).expect("recent_changes");
        let files: Vec<&str> = rows.iter().map(|c| c.file.as_str()).collect();
        assert!(!files.contains(&"src/stale.rs"), "{files:?}");
        assert_eq!(files[0], "docs/readme.md", "5 touches, more recent than hot.rs: {files:?}");
        assert_eq!(files[1], "src/hot.rs", "5 touches: {files:?}");
        assert_eq!(files[2], "src/warm.rs", "2 touches ranks last: {files:?}");

        // A tight window excludes everything older than it, even a
        // heavily-touched file.
        let tight = idx.recent_changes(0, None, 10).expect("recent_changes");
        assert!(tight.is_empty(), "{tight:?}");

        // path_prefix filters to one subtree.
        let docs_only = idx.recent_changes(30, Some("docs/"), 10).expect("recent_changes");
        assert_eq!(docs_only.len(), 1);
        assert_eq!(docs_only[0].file, "docs/readme.md");

        // max caps the result count.
        let capped = idx.recent_changes(30, None, 1).expect("recent_changes");
        assert_eq!(capped.len(), 1);

        drop(idx);
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ── V12 Phase E: project facts (memory distillation) ─────────────────

    #[test]
    fn fact_to_archive_for_cap_picks_oldest_unpinned_never_pinned() {
        // Below cap: nothing to archive.
        let below = vec![("a".to_string(), 1, false), ("b".to_string(), 2, true)];
        assert_eq!(fact_to_archive_for_cap(&below, 5), None);

        // Over cap: the oldest UNPINNED fact wins even though "old-pinned" is
        // older still.
        let over = vec![
            ("old-pinned".to_string(), 1, true),
            ("oldest-unpinned".to_string(), 2, false),
            ("newer-unpinned".to_string(), 3, false),
        ];
        assert_eq!(fact_to_archive_for_cap(&over, 2), Some("oldest-unpinned".to_string()));

        // Every live fact pinned: nothing safe to archive, cap simply exceeded.
        let all_pinned = vec![("p1".to_string(), 1, true), ("p2".to_string(), 2, true)];
        assert_eq!(fact_to_archive_for_cap(&all_pinned, 1), None);
    }

    #[test]
    fn project_fact_cap_archives_oldest_unpinned_never_pinned() {
        let dir = std::env::temp_dir().join(format!("ckg-factcap-{}", uuid::Uuid::new_v4()));
        let idx = GraphIndex::open(&dir, ".ckg").expect("open");

        // Seed a pinned fact plus (cap - 1) unpinned facts directly (bulk
        // write, bypassing the cap check) so the store starts already at the
        // cap. Bypassing `add_project_fact` for the seed keeps this test fast
        // (one bulk transaction instead of `MAX_LIVE_PROJECT_FACTS` of them).
        let mut rows: Vec<DataValue> = vec![DataValue::List(vec![
            DataValue::Str("pinned-1".into()),
            DataValue::Str("pinned fact".into()),
            DataValue::Str("s1".into()),
            DataValue::Num(Num::Int(1)),
            DataValue::Bool(true),
            DataValue::Bool(false),
        ])];
        for i in 0..MAX_LIVE_PROJECT_FACTS - 1 {
            rows.push(DataValue::List(vec![
                DataValue::Str(format!("f{i}").into()),
                DataValue::Str(format!("fact {i}").into()),
                DataValue::Str("s1".into()),
                DataValue::Num(Num::Int((i + 2) as i64)),
                DataValue::Bool(false),
                DataValue::Bool(false),
            ]));
        }
        idx.put(
            "?[fact_id, text, source_session, ts_ms, pinned, archived] <- $rows\n\
             :put project_fact {fact_id => text, source_session, ts_ms, pinned, archived}",
            rows,
        )
        .expect("seed facts");

        // One more insert pushes the live count over the cap by one — the
        // oldest unpinned fact ("f0", ts=2) must be archived, never "pinned-1"
        // (ts=1, older still, but pinned).
        idx.add_project_fact("new-1", "the newest fact", "s2", 1000, false)
            .expect("insert over cap");

        let live = idx.list_project_facts(false, 1000).expect("list live");
        assert_eq!(live.len(), MAX_LIVE_PROJECT_FACTS);
        assert!(live.iter().any(|f| f.fact_id == "pinned-1"), "pinned fact must survive the cap");
        assert!(live.iter().any(|f| f.fact_id == "new-1"));
        assert!(!live.iter().any(|f| f.fact_id == "f0"), "oldest unpinned fact should be archived");

        let all = idx.list_project_facts(true, 1000).expect("list all");
        let f0 = all.iter().find(|f| f.fact_id == "f0").expect("f0 still exists, archived");
        assert!(f0.archived);

        drop(idx);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn list_project_facts_orders_pinned_first_then_newest() {
        let dir = std::env::temp_dir().join(format!("ckg-factlist-{}", uuid::Uuid::new_v4()));
        let idx = GraphIndex::open(&dir, ".ckg").expect("open");

        idx.add_project_fact("old-unpinned", "old unpinned", "s1", 100, false).unwrap();
        idx.add_project_fact("new-unpinned", "new unpinned", "s1", 300, false).unwrap();
        idx.add_project_fact("old-pinned", "old pinned", "s1", 50, true).unwrap();

        let facts = idx.list_project_facts(false, 10).unwrap();
        let ids: Vec<&str> = facts.iter().map(|f| f.fact_id.as_str()).collect();
        // Pinned first (even though it's the oldest by ts_ms), then the
        // unpinned facts newest-first.
        assert_eq!(ids, vec!["old-pinned", "new-unpinned", "old-unpinned"]);

        drop(idx);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn fact_pin_unpin_archive_delete_roundtrip() {
        let dir = std::env::temp_dir().join(format!("ckg-factcrud-{}", uuid::Uuid::new_v4()));
        let idx = GraphIndex::open(&dir, ".ckg").expect("open");

        idx.add_project_fact("f1", "some fact", "s1", 100, false).unwrap();
        assert!(!idx.list_project_facts(false, 10).unwrap()[0].pinned);

        idx.set_fact_pinned("f1", true).unwrap();
        assert!(idx.list_project_facts(false, 10).unwrap()[0].pinned);

        idx.set_fact_pinned("f1", false).unwrap();
        assert!(!idx.list_project_facts(false, 10).unwrap()[0].pinned);

        idx.set_fact_archived("f1", true).unwrap();
        assert!(idx.list_project_facts(false, 10).unwrap().is_empty(), "archived facts are excluded by default");
        let all = idx.list_project_facts(true, 10).unwrap();
        assert_eq!(all.len(), 1);
        assert!(all[0].archived);

        idx.delete_fact("f1").unwrap();
        assert!(idx.list_project_facts(true, 10).unwrap().is_empty());

        // Deleting/updating an unknown id is a tolerant no-op, not an error.
        assert!(idx.set_fact_pinned("nope", true).is_ok());
        assert!(idx.delete_fact("nope").is_ok());

        drop(idx);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn session_distilled_flag_roundtrip_and_idle_sweep_candidates() {
        let dir = std::env::temp_dir().join(format!("ckg-distflag-{}", uuid::Uuid::new_v4()));
        let idx = GraphIndex::open(&dir, ".ckg").expect("open");

        idx.record_mem_event("s1", "claude", "read", "a.rs", None, None, 100, None).unwrap();
        idx.record_mem_event("s2", "claude", "read", "b.rs", None, None, 200, None).unwrap();

        // Neither session has been distilled yet.
        assert!(!idx.is_session_distilled("s1").unwrap());
        assert!(!idx.is_session_distilled("s2").unwrap());

        // Both are "idle" relative to a cutoff after their last activity.
        let idle = idx.sessions_idle_undistilled(1_000).unwrap();
        assert_eq!(idle.len(), 2);
        assert!(idle.contains(&"s1".to_string()));
        assert!(idle.contains(&"s2".to_string()));

        // Mark s1 distilled — it drops out of the idle-undistilled candidate
        // set; s2 stays.
        idx.mark_session_distilled("s1", 150).unwrap();
        assert!(idx.is_session_distilled("s1").unwrap());
        assert!(!idx.is_session_distilled("s2").unwrap());
        let idle_after = idx.sessions_idle_undistilled(1_000).unwrap();
        assert_eq!(idle_after, vec!["s2".to_string()]);

        // A cutoff before either session's last activity excludes both (not
        // idle yet).
        assert!(idx.sessions_idle_undistilled(50).unwrap().is_empty());

        drop(idx);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn session_distilled_flag_dropped_on_clear_and_eviction() {
        // F5: the distilled flag must not outlive its session — neither a
        // mem_clear nor an eviction may leave an orphan row (which would wrongly
        // suppress distillation if the same session id recurs, e.g. a resumed
        // Claude transcript UUID).
        let dir = std::env::temp_dir().join(format!("ckg-distdrop-{}", uuid::Uuid::new_v4()));
        let idx = GraphIndex::open(&dir, ".ckg").expect("open");

        // mem_clear(Some) drops only the target's flag.
        idx.record_mem_event("s1", "claude", "read", "a.rs", None, None, 100, None).unwrap();
        idx.record_mem_event("s2", "claude", "read", "b.rs", None, None, 200, None).unwrap();
        idx.mark_session_distilled("s1", 150).unwrap();
        idx.mark_session_distilled("s2", 250).unwrap();
        idx.mem_clear(Some("s1")).unwrap();
        assert!(!idx.is_session_distilled("s1").unwrap(), "s1 flag cleared");
        assert!(idx.is_session_distilled("s2").unwrap(), "s2 flag untouched");

        // mem_clear(None) drops the rest.
        idx.mem_clear(None).unwrap();
        assert!(!idx.is_session_distilled("s2").unwrap(), "whole-project clear drops s2 flag");

        // Eviction cascades the flag too: mark s0 distilled, then push it past
        // the cap with newer sessions.
        idx.record_mem_event("s0", "claude", "read", "a.rs", None, None, 0, None).unwrap();
        idx.mark_session_distilled("s0", 1).unwrap();
        for i in 0..MAX_SESSIONS_PER_ROOT {
            let sid = format!("n{}", i + 1);
            idx.record_mem_event(&sid, "claude", "read", "a.rs", None, None, 1000 + i as i64, None)
                .unwrap();
        }
        assert!(
            !idx.is_session_distilled("s0").unwrap(),
            "evicted session's distilled flag was cascaded away"
        );

        drop(idx);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
