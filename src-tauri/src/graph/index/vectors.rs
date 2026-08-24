//! V11 Phase G — the **semantic search** stores: the doc and code vector
//! relations, their HNSW indexes, their epoch/dim singletons, and the k-NN
//! queries over them. The V11 Phase F local-model **digest cache** rides along,
//! because it is the other thing the embedding pipeline keeps beside the graph
//! and it is pruned by the same file-lifecycle rules.
//!
//! # The two stores are one shape twice (V42 R13)
//!
//! `doc_chunk`/`doc_vec`/`embed_meta` and `code_chunk`/`code_vec`/
//! `code_embed_meta` are the same store over prose and over symbol bodies. They
//! keep SEPARATE meta singletons on purpose: a dim change has to be detected
//! independently per store, and sharing one singleton would have the second
//! `ensure_*` call see the first call's just-written new dim and wrongly
//! conclude nothing changed.
//!
//! What was NOT worth having twice is the code. The lifecycle half — ensure,
//! clear, drop, prune orphans, read the stored dim and epoch — was two
//! hand-copied sets of near-identical bodies whose only differences were the
//! three relation names. They are now one set, parameterised by [`VecStore`],
//! with the public per-store methods kept as the thin wrappers that name which
//! store they mean. The queries that genuinely differ between prose and code
//! (the puts, the pending-chunk selections, the two k-NN searches, the two
//! coverage counts) stay written out, because they are not the same query.
//!
//! The vector store is created lazily because its column type bakes in the
//! dimension (`<F32; N>`), which is only known once an embedder is configured
//! (or auto-probed). Vectors are keyed by `(chunk_id, epoch)` so a model change
//! (same dim) can keep old vectors alongside new; a dim change forces a
//! drop+recreate. Each vector also stores its chunk's content hash, so a
//! re-indexed chunk whose text changed is detected as needing a fresh
//! embedding.

use std::collections::BTreeMap;

use cozo::{DataValue, Num, ScriptMutability};

use crate::error::AppResult;

use super::{cell_i64, cell_str, dv_i64, int, truncate_chars, DocHit, GraphIndex, SymbolHit};

/// The three relation names that are all that distinguishes the doc vector
/// store from the code one.
///
/// `Copy` and `&'static str` throughout: these are two compile-time constants
/// ([`DOC_STORE`], [`CODE_STORE`]), never anything a caller composes, so no
/// lifetime or ownership question arises at a call site.
#[derive(Clone, Copy)]
struct VecStore {
    /// The vector relation — `(chunk_id, epoch) => hash, vec`.
    vecs: &'static str,
    /// The chunk relation its rows point at. A vector whose chunk is gone is
    /// the orphan the prune removes.
    chunks: &'static str,
    /// The store's own `model`/`dim`/`epoch` singleton.
    meta: &'static str,
}

/// The prose store: markdown + doc-comment chunks.
const DOC_STORE: VecStore = VecStore {
    vecs: "doc_vec",
    chunks: "doc_chunk",
    meta: "embed_meta",
};

/// The code store: symbol-body chunks. Its own singleton, not a share of
/// [`DOC_STORE`]'s — see the module docs.
const CODE_STORE: VecStore = VecStore {
    vecs: "code_vec",
    chunks: "code_chunk",
    meta: "code_embed_meta",
};

impl GraphIndex {
    // ── The store lifecycle, once (V42 R13) ───────────────────────────────
    //
    // Six operations that differ between the doc store and the code store only
    // in which three relations they name. Each takes a [`VecStore`]; the public
    // per-store methods below are the wrappers that say which one they mean.

    /// Ensure `s`'s vector relation + HNSW index exist sized for `dim`, and
    /// stamp `model`/`dim`/`epoch` into its meta singleton. If a store exists
    /// at a DIFFERENT dim it is dropped and recreated, and `true` is returned
    /// so the caller knows the old vectors are gone and a full re-embed is due.
    fn ensure_store(&self, s: VecStore, dim: usize, model: &str, epoch: &str) -> AppResult<bool> {
        self.ensure_meta(s)?;
        let existing_dim = self.stored_meta_dim(s)?;
        let mut reset = false;
        let have_vecs = self.existing_relations()?.contains(s.vecs);

        if !have_vecs || existing_dim != Some(dim) {
            if have_vecs {
                // A dim change: drop the store (its HNSW index must go first —
                // CozoDB refuses to remove a relation with indices attached).
                self.drop_store(s)?;
                reset = true;
            }
            self.exec(
                &format!("?[chunk_id, epoch, hash, vec] <- []\n:create {} {{chunk_id: String, epoch: String => hash: String, vec: <F32; {dim}>}}", s.vecs),
            )?;
            self.exec(
                &format!(
                    "::hnsw create {}:vec_idx {{dim: {dim}, m: 16, dtype: F32, fields: [vec], distance: Cosine, ef_construction: 50}}",
                    s.vecs
                ),
            )?;
        }

        // Upsert the singleton meta row.
        let mut p = BTreeMap::new();
        p.insert("model".to_string(), DataValue::Str(model.into()));
        p.insert("dim".to_string(), int(dim as u32));
        p.insert("epoch".to_string(), DataValue::Str(epoch.into()));
        self.run_mut(
            &format!(
                "?[id, model, dim, epoch] <- [['1', $model, $dim, $epoch]]\n:put {} {{id => model, dim, epoch}}",
                s.meta
            ),
            p,
        )?;
        Ok(reset)
    }

    /// Drop `s`'s vector store so the next backfill re-embeds everything from
    /// scratch. No-op if there is no store.
    fn clear_store(&self, s: VecStore) -> AppResult<()> {
        if self.existing_relations()?.contains(s.vecs) {
            self.drop_store(s)?;
        }
        Ok(())
    }

    /// Remove `s`'s vector relation, dropping its HNSW index first. CozoDB
    /// refuses to `::remove` a relation that still has an index attached, so
    /// the index must go first. The index drop is best-effort — ignored if it
    /// is absent (a partially-created store), leaving the relation removal to
    /// surface any real error.
    fn drop_store(&self, s: VecStore) -> AppResult<()> {
        let _ = self.exec(&format!("::index drop {}:vec_idx", s.vecs));
        self.exec(&format!("::remove {}", s.vecs))?;
        Ok(())
    }

    /// Delete `s`'s vectors whose chunk no longer exists. Keeps every
    /// still-valid vector, so it never forces a needless re-embed. No-op when
    /// there is no vector store. Returns the number of orphans removed.
    fn prune_orphans(&self, s: VecStore) -> AppResult<u64> {
        if !self.existing_relations()?.contains(s.vecs) {
            return Ok(0);
        }
        // Count first — a `:rm` returns a status row, not the deleted rows.
        let n = {
            let rows = self.query(
                &format!(
                    "?[count(chunk_id)] := *{}{{chunk_id, epoch}}, not *{}{{id: chunk_id}}",
                    s.vecs, s.chunks
                ),
            )?;
            rows.rows
                .first()
                .and_then(|r| r.first())
                .map(dv_i64)
                .unwrap_or(0) as u64
        };
        if n > 0 {
            self.exec(
                &format!(
                    "?[chunk_id, epoch] := *{}{{chunk_id, epoch}}, not *{}{{id: chunk_id}}\n\
                     :rm {} {{chunk_id, epoch}}",
                    s.vecs, s.chunks, s.vecs
                ),
            )?;
        }
        Ok(n)
    }

    /// Ensure `s`'s `model`/`dim`/`epoch` singleton relation exists.
    fn ensure_meta(&self, s: VecStore) -> AppResult<()> {
        if !self.existing_relations()?.contains(s.meta) {
            self.exec(
                &format!("?[id, model, dim, epoch] <- []\n:create {} {{id: String => model: String, dim: Int, epoch: String}}", s.meta),
            )?;
        }
        Ok(())
    }

    /// The dimension `s`'s vectors were last written at, or `None` if the store
    /// has never been sized.
    fn stored_meta_dim(&self, s: VecStore) -> AppResult<Option<usize>> {
        if !self.existing_relations()?.contains(s.meta) {
            return Ok(None);
        }
        let rows = self.query(&format!("?[dim] := *{}{{dim}}", s.meta))?;
        Ok(rows.rows.first().map(|r| cell_i64(r, 0) as usize))
    }

    /// `s`'s current epoch fingerprint, or `None` if it has never been embedded.
    fn meta_epoch(&self, s: VecStore) -> AppResult<Option<String>> {
        if !self.existing_relations()?.contains(s.meta) {
            return Ok(None);
        }
        let rows = self.query(&format!("?[epoch] := *{}{{epoch}}", s.meta))?;
        Ok(rows.rows.first().map(|r| cell_str(r, 0)))
    }

    // ── The doc store ─────────────────────────────────────────────────────

    /// Ensure the `doc_vec` relation + HNSW index exist sized for `dim`, and
    /// stamp `model`/`dim`/`epoch` into the `embed_meta` singleton. If a store
    /// exists at a DIFFERENT dim, it's dropped and recreated (returns `true` so
    /// the caller knows the old vectors are gone and a full re-embed is due).
    pub fn ensure_vector_store(&self, dim: usize, model: &str, epoch: &str) -> AppResult<bool> {
        self.ensure_store(DOC_STORE, dim, model, epoch)
    }

    /// Drop the vector store (and its HNSW index) so the next backfill
    /// re-embeds everything from scratch. Used by "Rebuild embeddings" / a
    /// silent model swap behind the same name. No-op if there's no store.
    pub fn clear_vectors(&self) -> AppResult<()> {
        self.clear_store(DOC_STORE)
    }

    /// Delete vectors whose `doc_chunk` no longer exists — chunks dropped by a
    /// file delete/rename or replaced under a new anchor when a file changed.
    /// Without this, orphaned vectors keep being counted as "embedded" (so
    /// coverage can read >100% and suppress backfill) and linger in the HNSW
    /// index as dead candidates. Returns the number of orphans removed.
    pub fn prune_orphan_vectors(&self) -> AppResult<u64> {
        self.prune_orphans(DOC_STORE)
    }

    /// The current embedding epoch fingerprint, or `None` if never embedded.
    pub fn current_epoch(&self) -> AppResult<Option<String>> {
        self.meta_epoch(DOC_STORE)
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
        let rows = self.query("?[id, text] := *doc_chunk{id, text}")?;
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
            let rows = self.query("?[count(id)] := *doc_chunk{id}")?;
            rows.rows
                .first()
                .and_then(|r| r.first())
                .map(dv_i64)
                .unwrap_or(0) as u64
        };
        let embedded = if self.existing_relations()?.contains("doc_vec") {
            let mut p = BTreeMap::new();
            p.insert("epoch".to_string(), DataValue::Str(epoch.into()));
            let rows = self.run(
                "?[count(chunk_id)] := *doc_vec{chunk_id, epoch}, epoch == $epoch",
                p,
                ScriptMutability::Immutable,
            )?;
            rows.rows
                .first()
                .and_then(|r| r.first())
                .map(dv_i64)
                .unwrap_or(0) as u64
        } else {
            0
        };
        Ok((embedded, total))
    }
    // ── The code store ────────────────────────────────────────────────────
    //
    // The `code_chunk`/`code_vec` pair is the twin of `doc_chunk`/`doc_vec`
    // above, over symbol bodies instead of prose.

    /// Ensure the `code_vec` relation + HNSW index exist sized for `dim`, and
    /// stamp `model`/`dim`/`epoch` into the `code_embed_meta` singleton.
    /// Returns `true` on a dim change (old code vectors dropped — a full code
    /// re-embed is due).
    pub fn ensure_code_vector_store(
        &self,
        dim: usize,
        model: &str,
        epoch: &str,
    ) -> AppResult<bool> {
        self.ensure_store(CODE_STORE, dim, model, epoch)
    }

    /// Drop the `code_vec` store, forcing the next backfill to re-embed every
    /// code chunk from scratch. No-op if there's no store.
    pub fn clear_code_vectors(&self) -> AppResult<()> {
        self.clear_store(CODE_STORE)
    }

    /// Delete code vectors whose `code_chunk` no longer exists — chunks
    /// dropped by a file delete/rename or a symbol that shrank below the
    /// chunking threshold. Returns the number of orphans removed.
    pub fn prune_orphan_code_vectors(&self) -> AppResult<u64> {
        self.prune_orphans(CODE_STORE)
    }

    /// The current code-embedding epoch fingerprint, or `None` if never embedded.
    pub fn current_code_epoch(&self) -> AppResult<Option<String>> {
        self.meta_epoch(CODE_STORE)
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
        let rows = self.query("?[id, text] := *code_chunk{id, text}")?;
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
            let rows = self.query("?[count(id)] := *code_chunk{id}")?;
            rows.rows
                .first()
                .and_then(|r| r.first())
                .map(dv_i64)
                .unwrap_or(0) as u64
        };
        let embedded = if self.existing_relations()?.contains("code_vec") {
            let mut p = BTreeMap::new();
            p.insert("epoch".to_string(), DataValue::Str(epoch.into()));
            let rows = self.run(
                "?[count(chunk_id)] := *code_vec{chunk_id, epoch}, epoch == $epoch",
                p,
                ScriptMutability::Immutable,
            )?;
            rows.rows
                .first()
                .and_then(|r| r.first())
                .map(dv_i64)
                .unwrap_or(0) as u64
        } else {
            0
        };
        Ok((embedded, total))
    }

    // ── The local-model digest cache (V11 Phase F) ────────────────────────
    //
    // Not a vector store: a `(file, content_hash) => text` cache of what the
    // local model said about a file, so a re-index that did not change the file
    // does not pay for the digest again. It lives here because it is pruned by
    // the same file-lifecycle rule the vectors are — a row whose file is gone
    // is an orphan — and because the embedding pipeline is its only writer.
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
    pub fn put_digest(
        &self,
        file: &str,
        content_hash: &str,
        text: &str,
        ts_ms: i64,
    ) -> AppResult<()> {
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
        let rows = self.query("?[count(file)] := *digest{file, content_hash}")?;
        Ok(rows
            .rows
            .first()
            .and_then(|r| r.first())
            .map(dv_i64)
            .unwrap_or(0) as u64)
    }

    /// Drop cached digests whose file is no longer indexed (mirrors the doc_vec
    /// orphan sweep).
    pub fn prune_orphan_digests(&self) -> AppResult<u64> {
        if !self.existing_relations()?.contains("digest") {
            return Ok(0);
        }
        let n = {
            let rows = self.query(
                "?[count(file)] := *digest{file, content_hash}, not *file{path: file}",
            )?;
            rows.rows
                .first()
                .and_then(|r| r.first())
                .map(dv_i64)
                .unwrap_or(0) as u64
        };
        if n > 0 {
            self.exec(
                "?[file, content_hash] := *digest{file, content_hash}, not *file{path: file}\n\
                 :rm digest {file, content_hash}",
            )?;
        }
        Ok(n)
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
    crate::graph::model::fnv1a_hex(s)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::{parse_file, Lang};

    #[test]
    fn digest_cache_roundtrip_and_orphan_prune() {
        let dir = std::env::temp_dir().join(format!("ckg-digest-{}", uuid::Uuid::new_v4()));
        let idx = GraphIndex::open(&dir, ".ckg").expect("open");
        idx.index_file_graph(&parse_file("src/a.rs", "pub fn f() {}\n", Lang::Rust))
            .expect("index");

        // Miss, then hit; a different hash is a miss (stale content).
        assert!(idx.get_digest("src/a.rs", "h1").unwrap().is_none());
        idx.put_digest("src/a.rs", "h1", "a three line digest", 100)
            .unwrap();
        assert_eq!(
            idx.get_digest("src/a.rs", "h1").unwrap().as_deref(),
            Some("a three line digest")
        );
        assert!(idx.get_digest("src/a.rs", "h2").unwrap().is_none());
        assert_eq!(idx.digest_count().unwrap(), 1);

        // F11: re-digesting the SAME file under a new hash supersedes the old
        // row rather than leaking it — the count stays 1 and the stale hash is
        // no longer a hit.
        idx.put_digest("src/a.rs", "h2", "updated digest", 200)
            .unwrap();
        assert_eq!(
            idx.digest_count().unwrap(),
            1,
            "one digest per file, not per edit"
        );
        assert!(
            idx.get_digest("src/a.rs", "h1").unwrap().is_none(),
            "old hash superseded"
        );
        assert_eq!(
            idx.get_digest("src/a.rs", "h2").unwrap().as_deref(),
            Some("updated digest")
        );

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
        let reset = idx
            .ensure_vector_store(3, "fake-model", epoch)
            .expect("ensure");
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
        assert!(idx
            .chunks_needing_vectors(epoch, 100)
            .expect("need2")
            .is_empty());

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
    fn widened_selection_lets_the_backfill_skip_poison_chunks() {
        // The contract `embed_backfill`'s skip set depends on: this selector
        // takes an ARBITRARY limit, so asking for `batch + skipped.len()` rows
        // and filtering the skipped ids back out still yields a full batch of
        // fresh work — and yields NOTHING once only skipped rows remain, which
        // is what terminates the loop. Without the widening, the same rejected
        // rows would fill every batch and the backfill would spin forever.
        let dir = std::env::temp_dir().join(format!("ckg-skip-{}", uuid::Uuid::new_v4()));
        let idx = GraphIndex::open(&dir, ".ckg").expect("open");

        let md = "# One\n\nAlpha.\n\n# Two\n\nBravo.\n\n# Three\n\nCharlie.\n\n# Four\n\nDelta.\n";
        idx.index_file_graph(&parse_file("docs/a.md", md, Lang::Markdown))
            .expect("index md");

        let epoch = "skip-epoch";
        idx.ensure_vector_store(3, "m", epoch).expect("ensure");

        let batch = 2usize;
        let select = |skipped: &std::collections::HashSet<String>| -> Vec<String> {
            idx.chunks_needing_vectors(epoch, batch + skipped.len())
                .expect("need")
                .into_iter()
                .filter(|(id, _, _)| !skipped.contains(id))
                .map(|(id, _, _)| id)
                .collect()
        };

        let mut skipped: std::collections::HashSet<String> = std::collections::HashSet::new();
        let first = select(&skipped);
        assert_eq!(first.len(), 4.min(batch), "a plain batch: {first:?}");

        // The whole first batch turns out to be poison.
        for id in &first {
            skipped.insert(id.clone());
        }
        let second = select(&skipped);
        assert_eq!(second.len(), batch, "widening surfaces fresh rows: {second:?}");
        assert!(
            second.iter().all(|id| !skipped.contains(id)),
            "skipped ids must never come back: {second:?}"
        );

        // Embed the rest; only the skipped rows are left pending.
        let vecs: Vec<(String, String, Vec<f32>)> = idx
            .chunks_needing_vectors(epoch, 100)
            .expect("need all")
            .into_iter()
            .filter(|(id, _, _)| !skipped.contains(id))
            .map(|(id, hash, _)| (id, hash, vec![1.0, 0.0, 0.0]))
            .collect();
        idx.put_doc_vectors(epoch, &vecs).expect("put");

        // Nothing embeddable remains → the backfill loop breaks instead of
        // re-selecting the poison rows forever.
        assert!(select(&skipped).is_empty(), "loop must terminate");
        // …and the poison rows are genuinely still pending (not silently
        // marked done), so a later run with a fixed server retries them.
        assert_eq!(
            idx.chunks_needing_vectors(epoch, 100).expect("need").len(),
            skipped.len()
        );

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

        idx.index_file_graph(&parse_file(
            "docs/a.md",
            "# Cats\n\nFelines purr.\n",
            Lang::Markdown,
        ))
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
        idx.clear_vectors()
            .expect("clear_vectors with index attached");
        assert!(!idx.existing_relations().unwrap().contains("doc_vec"));

        // A dim change re-exercises the same drop path inside ensure_vector_store.
        idx.ensure_vector_store(3, "m", epoch).expect("recreate");
        let reset = idx.ensure_vector_store(4, "m", "e2").expect("dim change");
        assert!(reset, "a dim change is a reset");

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
        idx.index_file_graph(&parse_file(
            "docs/a.md",
            "# Cats\n\nFelines.\n",
            Lang::Markdown,
        ))
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
        let reset = idx
            .ensure_code_vector_store(3, "fake-model", epoch)
            .expect("ensure");
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
        assert!(idx
            .pending_code_chunks(epoch, 100)
            .expect("need2")
            .is_empty());

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
}
