//! The embedded CozoDB store for one project's graph. `GraphIndex` owns a
//! SQLite-backed `DbInstance` at `<root>/<db_subdir>/graph.db`, ensures the
//! schema, writes [`FileGraph`]s (delete-then-insert per file so a re-index is
//! idempotent and isolated), and answers the first queries.
//!
//! The query API broadens in Phase B; this stage proves the round trip
//! (parse → store → `find_symbol`).

use std::collections::{BTreeMap, HashSet};
use std::path::Path;

use cozo::{DataValue, DbInstance, Num, ScriptMutability};

use crate::error::{AppError, AppResult};

use super::model::FileGraph;
use super::schema::RELATIONS;

/// One symbol returned by a lookup query.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SymbolHit {
    pub id: String,
    pub name: String,
    pub kind: String,
    pub file: String,
    pub start_line: u32,
    pub signature: String,
}

/// Per-index counts for the status surface.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct GraphStats {
    pub files: u64,
    pub symbols: u64,
    pub edges: u64,
}

pub struct GraphIndex {
    db: DbInstance,
}

impl GraphIndex {
    /// Open (creating if needed) the graph store for `root`, ensuring the
    /// schema. `db_subdir` is the per-project home (default `.ccimp`).
    pub fn open(root: &Path, db_subdir: &str) -> AppResult<GraphIndex> {
        let dir = root.join(db_subdir);
        std::fs::create_dir_all(&dir).map_err(AppError::Io)?;
        let db_path = dir.join("graph.db");
        let db = DbInstance::new("sqlite", db_path.to_string_lossy().as_ref(), Default::default())
            .map_err(|e| AppError::Graph(format!("open {}: {e}", db_path.display())))?;
        let index = GraphIndex { db };
        index.ensure_schema()?;
        Ok(index)
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

    fn existing_relations(&self) -> AppResult<HashSet<String>> {
        let rows = self.run(
            "::relations",
            BTreeMap::new(),
            ScriptMutability::Immutable,
        )?;
        let name_col = rows
            .headers
            .iter()
            .position(|h| h == "name")
            .unwrap_or(0);
        Ok(rows
            .rows
            .iter()
            .filter_map(|r| r.get(name_col))
            .map(dv_string)
            .collect())
    }

    /// Write one file's extracted graph, replacing any prior rows for that
    /// path. Symbols/refs/doc-chunks are keyed by the file; edges are matched
    /// by the file-embedded id prefix (`<file>#…`) or, for imports, `src == file`.
    pub fn index_file_graph(&self, fg: &FileGraph) -> AppResult<()> {
        let file = fg.path.clone();
        let prefix = format!("{file}#");

        // --- delete prior rows for this file ---
        let mut p = BTreeMap::new();
        p.insert("file".to_string(), DataValue::Str(file.as_str().into()));
        self.run_mut(
            "?[id] := *symbol{id, file}, file == $file\n:rm symbol {id}",
            p.clone(),
        )?;
        self.run_mut(
            "?[file, line, col, name] := *ref{file, line, col, name}, file == $file\n:rm ref {file, line, col, name}",
            p.clone(),
        )?;
        self.run_mut(
            "?[id] := *doc_chunk{id, source_path}, source_path == $file\n:rm doc_chunk {id}",
            p.clone(),
        )?;
        let mut pe = p.clone();
        pe.insert("prefix".to_string(), DataValue::Str(prefix.as_str().into()));
        self.run_mut(
            "?[kind, src, dst] := *edge{kind, src, dst}, (starts_with(src, $prefix) or src == $file)\n:rm edge {kind, src, dst}",
            pe,
        )?;

        // --- insert fresh rows ---
        let file_rows = vec![DataValue::List(vec![
            DataValue::Str(file.as_str().into()),
            DataValue::Str(fg.lang_tag.as_str().into()),
            DataValue::Str(fg.hash.as_str().into()),
        ])];
        self.put("?[path, lang, hash] <- $rows\n:put file {path => lang, hash}", file_rows)?;

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
                ])
            })
            .collect();
        self.put(
            "?[id, name, kind, file, start_line, end_line, signature, doc] <- $rows\n\
             :put symbol {id => name, kind, file, start_line, end_line, signature, doc}",
            symbol_rows,
        )?;

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
        self.put(
            "?[file, line, col, name, resolved_id] <- $rows\n:put ref {file, line, col, name => resolved_id}",
            ref_rows,
        )?;

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
        self.put(
            "?[id, source_path, anchor, text] <- $rows\n:put doc_chunk {id => source_path, anchor, text}",
            doc_rows,
        )?;

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
        self.put(
            "?[kind, src, dst] <- $rows\n:put edge {kind, src, dst}",
            edge_rows,
        )?;

        Ok(())
    }

    /// Find definitions by exact name.
    pub fn find_symbol(&self, name: &str) -> AppResult<Vec<SymbolHit>> {
        let mut p = BTreeMap::new();
        p.insert("name".to_string(), DataValue::Str(name.into()));
        let rows = self.run(
            "?[id, name, kind, file, start_line, signature] := \
                *symbol{id, name, kind, file, start_line, signature}, name == $name",
            p,
            ScriptMutability::Immutable,
        )?;
        Ok(rows
            .rows
            .iter()
            .map(|r| SymbolHit {
                id: cell_str(r, 0),
                name: cell_str(r, 1),
                kind: cell_str(r, 2),
                file: cell_str(r, 3),
                start_line: cell_i64(r, 4) as u32,
                signature: cell_str(r, 5),
            })
            .collect())
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
        })
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
}
