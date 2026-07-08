//! The embedded CozoDB store for one project's graph. `GraphIndex` owns a
//! SQLite-backed `DbInstance` at `<root>/<db_subdir>/graph.db`, ensures the
//! schema, writes [`FileGraph`]s (delete-then-insert per file so a re-index is
//! idempotent and isolated), and answers the first queries.
//!
//! The query API broadens in Phase B; this stage proves the round trip
//! (parse → store → `find_symbol`).

use std::collections::{BTreeMap, HashSet};
use std::path::Path;

use cozo::{DataValue, DbInstance, MultiTransaction, Num, ScriptMutability};

use crate::error::{AppError, AppResult};

use super::model::{FileGraph, Lang};
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
}

/// One reference (use site) returned by `references`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RefHit {
    pub name: String,
    pub file: String,
    pub line: u32,
    pub col: u32,
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
}

impl GraphIndex {
    /// Open (creating if needed) the graph store for `root`, ensuring the
    /// schema. `db_subdir` is the per-project home (default `.cimp`).
    pub fn open(root: &Path, db_subdir: &str) -> AppResult<GraphIndex> {
        let dir = root.join(db_subdir);
        std::fs::create_dir_all(&dir).map_err(AppError::Io)?;
        let db_path = dir.join("graph.db");
        let db = DbInstance::new("sqlite", db_path.to_string_lossy().as_ref(), Default::default())
            .map_err(|e| AppError::Graph(format!("open {}: {e}", db_path.display())))?;
        let index = GraphIndex { db };
        index.ensure_schema()?;
        index.migrate_schema()?;
        Ok(index)
    }

    /// Open an **existing** graph store, erroring if it hasn't been built yet.
    /// Used by read-only consumers (the MCP child) that must not create an
    /// empty db for an unindexed project.
    pub fn open_existing(root: &Path, db_subdir: &str) -> AppResult<GraphIndex> {
        let db_path = root.join(db_subdir).join("graph.db");
        if !db_path.exists() {
            return Err(AppError::GraphNotReady(format!(
                "no code graph at {} — enable the graph and index this project in cImp",
                db_path.display()
            )));
        }
        Self::open(root, db_subdir)
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

    /// Detect a stale on-disk schema generation and, if found, drop+recreate the
    /// derived relations at the current [`GRAPH_SCHEMA_VERSION`] shape. CozoDB
    /// has no cheap `ALTER` and every graph row is re-derivable from source, so
    /// a `reset()` *is* the migration — a re-index (spawned by the service on
    /// launch/watch) repopulates the emptied relations with the new columns. The
    /// version lives in a dedicated `schema_meta` singleton that is **not** part
    /// of [`RELATIONS`], so `reset()` preserves it. A fresh store starts at
    /// `None` and is stamped on first open (one cheap recreate).
    fn migrate_schema(&self) -> AppResult<()> {
        self.ensure_schema_meta()?;
        if self.stored_schema_version()? != Some(GRAPH_SCHEMA_VERSION) {
            self.reset()?;
            self.write_schema_version(GRAPH_SCHEMA_VERSION)?;
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

    /// Delete every row belonging to `file` (symbols, refs, doc-chunks, the
    /// `file` row, and edges keyed by the file-embedded id prefix `<file>#…` or
    /// `src == file` for imports). Used both by the per-file replace in
    /// [`index_file_graph`] and by the watcher when a file is deleted.
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

        let edge_rows = fg
            .edges
            .iter()
            .map(|e| {
                DataValue::List(vec![
                    DataValue::Str(e.kind.tag().into()),
                    DataValue::Str(e.src.as_str().into()),
                    DataValue::Str(e.dst.as_str().into()),
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
                "?[id, name, kind, file, start_line, end_line, signature, doc, visibility] <- $rows\n\
                 :put symbol {id => name, kind, file, start_line, end_line, signature, doc, visibility}",
                symbol_rows,
            )?;
            tx_put(
                tx,
                "?[file, line, col, name, resolved_id] <- $rows\n:put ref {file, line, col, name => resolved_id}",
                ref_rows,
            )?;
            tx_put(
                tx,
                "?[id, source_path, anchor, text] <- $rows\n:put doc_chunk {id => source_path, anchor, text}",
                doc_rows,
            )?;
            tx_put(
                tx,
                "?[kind, src, dst] <- $rows\n:put edge {kind, src, dst}",
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
            "?[id, name, kind, file, start_line, signature, visibility] := \
                *symbol{id, name, kind, file, start_line, signature, visibility}, name == $name",
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
            r#"?[sid, sname, skind, file, start_line, signature, visibility] :=
                *edge{kind: ek, src: sid, dst: dn}, ek == "call", dn == $name,
                *symbol{id: sid, name: sname, kind: skind, file, start_line, signature, visibility}
            :limit 500"#,
            p,
            ScriptMutability::Immutable,
        )?;
        Ok(rows_to_symbols(&rows))
    }

    /// Symbols called by any symbol named `name` (callees, resolved by name).
    pub fn callees(&self, name: &str) -> AppResult<Vec<SymbolHit>> {
        let mut p = BTreeMap::new();
        p.insert("name".to_string(), DataValue::Str(name.into()));
        let rows = self.run(
            r#"?[id2, nm, skind, file, start_line, signature, visibility] :=
                *symbol{id: cid, name: cn}, cn == $name,
                *edge{kind: ek, src: cid, dst: dn}, ek == "call",
                *symbol{id: id2, name: nm, kind: skind, file, start_line, signature, visibility}, nm == dn
            :limit 500"#,
            p,
            ScriptMutability::Immutable,
        )?;
        Ok(rows_to_symbols(&rows))
    }

    /// All reference (use) sites of `name`.
    pub fn references(&self, name: &str) -> AppResult<Vec<RefHit>> {
        let mut p = BTreeMap::new();
        p.insert("name".to_string(), DataValue::Str(name.into()));
        let rows = self.run(
            "?[name, file, line, col] := *ref{name, file, line, col}, name == $name\n:limit 1000",
            p,
            ScriptMutability::Immutable,
        )?;
        Ok(rows
            .rows
            .iter()
            .map(|r| RefHit {
                name: cell_str(r, 0),
                file: cell_str(r, 1),
                line: cell_i64(r, 2) as u32,
                col: cell_i64(r, 3) as u32,
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
    /// (`main`, `test_*`, common trait/convention method names). These are
    /// *candidates* — a symbol reached only through dynamic dispatch, an external
    /// consumer, a macro, or reflection has no static edge and will appear here
    /// as a false positive. The caller/UI must state that caveat.
    pub fn dead_exports(&self, max: usize) -> AppResult<Vec<SymbolHit>> {
        let mut p = BTreeMap::new();
        p.insert("max".to_string(), int(max as u32));
        let rows = self.run(
            r#"call_dst[dst] := *edge{kind: k, src, dst}, k == "call"
?[id, name, kind, file, start_line, signature, visibility] :=
    *symbol{id, name, kind, file, start_line, signature, visibility},
    visibility == "public",
    not *ref{name: name},
    not call_dst[name],
    not call_dst[id]
:limit $max"#,
            p,
            ScriptMutability::Immutable,
        )?;
        let mut syms = rows_to_symbols(&rows);
        syms.retain(|s| !is_entrypoint_name(&s.name));
        syms.sort_by(|a, b| a.file.cmp(&b.file).then(a.start_line.cmp(&b.start_line)));
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
            "?[id, name, kind, file, start_line, signature, visibility] := \
                *symbol{id, name, kind, file, start_line, signature, visibility}, file == $file",
            p,
            ScriptMutability::Immutable,
        )?;
        let mut syms = rows_to_symbols(&rows);
        syms.sort_by_key(|s| s.start_line);
        Ok(syms)
    }

    /// Transitive call-chain names. `forward = true` returns everything `name`
    /// transitively calls; `false` returns everything that transitively calls
    /// `name`. Recursive Datalog over a name-level call graph; terminates on
    /// cycles (set saturation).
    pub fn transitive(&self, name: &str, forward: bool) -> AppResult<Vec<String>> {
        let mut p = BTreeMap::new();
        p.insert("name".to_string(), DataValue::Str(name.into()));
        let head = if forward {
            "?[y] := reach[x, y], x == $name"
        } else {
            "?[x] := reach[x, y], y == $name"
        };
        let script = format!(
            r#"calls[cn, dn] := *symbol{{id: cid, name: cn}}, *edge{{kind: ek, src: cid, dst: dn}}, ek == "call"
reach[x, y] := calls[x, y]
reach[x, y] := reach[x, z], calls[z, y]
{head}
:limit 1000"#
        );
        let rows = self.run(&script, p, ScriptMutability::Immutable)?;
        Ok(rows.rows.iter().map(|r| cell_str(r, 0)).collect())
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
/// refs, doc-chunks, the `file` row, edges keyed by the file id-prefix or
/// `src == file`, plus inbound call edges to names this file uniquely defined).
/// The transaction makes this atomic with the re-insert in [`GraphIndex::index_file_graph`].
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
/// visibility]` to [`SymbolHit`]s. Queries that don't project `visibility`
/// (7th column absent) get `"unknown"`.
fn rows_to_symbols(rows: &cozo::NamedRows) -> Vec<SymbolHit> {
    rows.rows
        .iter()
        .map(|r| SymbolHit {
            id: cell_str(r, 0),
            name: cell_str(r, 1),
            kind: cell_str(r, 2),
            file: cell_str(r, 3),
            start_line: cell_i64(r, 4) as u32,
            signature: cell_str(r, 5),
            visibility: if r.len() > 6 { cell_str(r, 6) } else { "unknown".to_string() },
        })
        .collect()
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

        let stats = idx.stats().expect("stats");
        assert_eq!(stats.files, 1);
        assert!(stats.symbols >= 3);
        assert!(stats.edges >= 1);

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

        // transitive: add -> helper.
        let reach = idx.transitive("add", true).expect("transitive");
        assert!(reach.contains(&"helper".to_string()));

        // doc search finds the "Adds two numbers." chunk.
        let docs = idx.search_docs("numbers", 10, 200).expect("docs");
        assert!(docs.iter().any(|d| d.anchor == "add"));

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
