//! V15 Feature 2 — the **architecture overview**: god nodes, subsystems, and
//! the edges that cross between them.
//!
//! Pure topology, computed on demand from a handful of scans of the warm index
//! — no LLM, no embeddings, and nothing persisted: the report is derived from
//! `symbol`/`calls`/`imports` every time it is asked for, so it can never go
//! stale against the graph it describes.
//!
//! A submodule (V42 R13) because it is one 300-line algorithm with one entry
//! point and one private helper, which is the shape that reads better beside
//! its own module docs than buried in the middle of the store. The report types
//! themselves stay in the parent, where every other row type the store returns
//! is declared. `impl GraphIndex` continues here, exactly as
//! [`super::notes`] does it.

use std::collections::{BTreeMap, HashMap, HashSet};

use cozo::ScriptMutability;

use crate::error::AppResult;

use super::{
    cell_i64, cell_str, resolve_import, ArchReport, GodNode, GraphIndex, Lang, Subsystem,
    SurprisingEdge,
};

impl GraphIndex {
    /// V15 Feature 2: the architecture overview — god nodes (highest-degree
    /// hubs), subsystems (file communities via deterministic label propagation),
    /// and surprising edges (edges crossing subsystem boundaries). Pure topology,
    /// computed on demand from a handful of scans of the warm index; no LLM, no
    /// embeddings. `max_communities`/`min_size` bound the subsystem report;
    /// `max_rows` bounds god nodes and surprising edges.
    pub fn architecture(
        &self,
        max_communities: usize,
        min_size: usize,
        max_rows: usize,
    ) -> AppResult<ArchReport> {
        // Symbol tables.
        let sym_rows = self.run(
            "?[id, name, kind, file, start_line] := *symbol{id, name, kind, file, start_line}",
            BTreeMap::new(),
            ScriptMutability::Immutable,
        )?;
        let mut sym_file: HashMap<String, String> = HashMap::new(); // id → file
        let mut name_files: HashMap<String, Vec<String>> = HashMap::new(); // name → files
                                                                           // name → (id, kind, file, start_line) of its FIRST definition, ordered
                                                                           // like `find_symbol` (file, start_line, id) — the god-node loop below
                                                                           // resolves representatives from this already-loaded table instead of
                                                                           // issuing one `find_symbol` DB query per candidate name.
        let mut first_def: HashMap<String, (String, String, String, i64)> = HashMap::new();
        for r in &sym_rows.rows {
            let id = cell_str(r, 0);
            let name = cell_str(r, 1);
            let kind = cell_str(r, 2);
            let file = cell_str(r, 3);
            let start_line = cell_i64(r, 4);
            sym_file.insert(id.clone(), file.clone());
            match first_def.entry(name.clone()) {
                std::collections::hash_map::Entry::Occupied(mut e) => {
                    let (cid, _, cfile, cline) = e.get();
                    if (file.as_str(), start_line, id.as_str())
                        < (cfile.as_str(), *cline, cid.as_str())
                    {
                        e.insert((id, kind, file.clone(), start_line));
                    }
                }
                std::collections::hash_map::Entry::Vacant(e) => {
                    e.insert((id, kind, file.clone(), start_line));
                }
            }
            name_files.entry(name).or_default().push(file);
        }

        // File langs (for import resolution).
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
            known_files.insert(path);
        }

        // Undirected file-level adjacency + a representative edge kind per pair,
        // built from call edges (caller file ↔ callee file) and resolved imports.
        let mut adj: HashMap<String, HashSet<String>> = HashMap::new();
        let mut pair_kind: HashMap<(String, String), &'static str> = HashMap::new();
        let link = |a: &str,
                    b: &str,
                    kind: &'static str,
                    adj: &mut HashMap<String, HashSet<String>>,
                    pair_kind: &mut HashMap<(String, String), &'static str>| {
            if a == b {
                return;
            }
            adj.entry(a.to_string()).or_default().insert(b.to_string());
            adj.entry(b.to_string()).or_default().insert(a.to_string());
            let key = if a < b {
                (a.to_string(), b.to_string())
            } else {
                (b.to_string(), a.to_string())
            };
            // Prefer to remember an import link over a call link when both exist.
            pair_kind
                .entry(key)
                .and_modify(|k| {
                    if kind == "import" {
                        *k = "import";
                    }
                })
                .or_insert(kind);
        };

        // Call edges → caller file ↔ each callee-name's file(s). Also inbound
        // call counts per callee name (feeds god nodes).
        let call_rows = self.run(
            r#"?[src, dst] := *edge{kind: k, src, dst}, k == "call""#,
            BTreeMap::new(),
            ScriptMutability::Immutable,
        )?;
        let mut inbound_calls: HashMap<String, u64> = HashMap::new();
        for r in &call_rows.rows {
            let src = cell_str(r, 0);
            let dst = cell_str(r, 1);
            *inbound_calls.entry(dst.clone()).or_default() += 1;
            let Some(caller_file) = sym_file.get(&src) else {
                continue;
            };
            if let Some(files) = name_files.get(&dst) {
                for cf in files {
                    link(caller_file, cf, "call", &mut adj, &mut pair_kind);
                }
            }
        }

        // Import edges → file ↔ resolved-target file.
        let import_rows = self.run(
            r#"?[src, dst] := *edge{kind: k, src, dst}, k == "import""#,
            BTreeMap::new(),
            ScriptMutability::Immutable,
        )?;
        for r in &import_rows.rows {
            let from_file = cell_str(r, 0);
            let module = cell_str(r, 1);
            let lang = Lang::from_tag(file_lang.get(&from_file).map(|s| s.as_str()).unwrap_or(""));
            if let Some(target) = resolve_import(lang, &from_file, &module, &known_files) {
                link(&from_file, &target, "import", &mut adj, &mut pair_kind);
            }
        }

        // ── Label propagation (deterministic, id-sorted, bounded) ──
        let mut files: Vec<String> = adj.keys().cloned().collect();
        files.sort();
        let mut label: HashMap<String, String> =
            files.iter().map(|f| (f.clone(), f.clone())).collect();
        const MAX_ITERS: usize = 20;
        for _ in 0..MAX_ITERS {
            let mut changed = false;
            for f in &files {
                let Some(nbrs) = adj.get(f) else { continue };
                if nbrs.is_empty() {
                    continue;
                }
                let mut counts: HashMap<&str, usize> = HashMap::new();
                for n in nbrs {
                    if let Some(l) = label.get(n) {
                        *counts.entry(l.as_str()).or_default() += 1;
                    }
                }
                // Most frequent neighbor label; ties → lexicographically smallest
                // label, so the pass is deterministic run to run.
                if let Some(best) = counts
                    .iter()
                    .max_by(|a, b| a.1.cmp(b.1).then_with(|| b.0.cmp(a.0)))
                    .map(|(l, _)| l.to_string())
                {
                    if label.get(f) != Some(&best) {
                        label.insert(f.clone(), best);
                        changed = true;
                    }
                }
            }
            if !changed {
                break;
            }
        }

        // Group into communities.
        let mut groups: HashMap<String, Vec<String>> = HashMap::new();
        for f in &files {
            if let Some(l) = label.get(f) {
                groups.entry(l.clone()).or_default().push(f.clone());
            }
        }
        // File centrality → hub selection + a score map.
        let centrality: HashMap<String, u64> = self
            .file_centrality(usize::MAX)
            .unwrap_or_default()
            .into_iter()
            .collect();

        let mut communities: Vec<Vec<String>> = groups
            .into_values()
            .filter(|g| g.len() >= min_size.max(1))
            .collect();
        // Biggest first; tie-break by first (sorted) member for determinism.
        for g in &mut communities {
            g.sort();
        }
        communities.sort_by(|a, b| b.len().cmp(&a.len()).then_with(|| a[0].cmp(&b[0])));
        communities.truncate(max_communities.max(1));

        // Name each community + map file → community name (for surprising edges).
        let mut file_comm: HashMap<String, String> = HashMap::new();
        let mut subsystems: Vec<Subsystem> = Vec::new();
        for members in &communities {
            let name = community_name(members);
            let hub = members
                .iter()
                .max_by(|a, b| {
                    centrality
                        .get(*a)
                        .copied()
                        .unwrap_or(0)
                        .cmp(&centrality.get(*b).copied().unwrap_or(0))
                        .then_with(|| b.cmp(a))
                })
                .cloned()
                .unwrap_or_default();
            for f in members {
                file_comm.insert(f.clone(), name.clone());
            }
            subsystems.push(Subsystem {
                name,
                size: members.len(),
                files: members.iter().take(6).cloned().collect(),
                hub,
            });
        }

        // Surprising edges: file-pairs whose endpoints are in different reported
        // communities, ranked by how rare cross-links are between that community
        // pair (fewer crossings = more surprising).
        let mut cross_count: HashMap<(String, String), usize> = HashMap::new();
        let mut candidates: Vec<((String, String), &'static str, String, String)> = Vec::new();
        for ((a, b), kind) in &pair_kind {
            let (Some(ca), Some(cb)) = (file_comm.get(a), file_comm.get(b)) else {
                continue;
            };
            if ca == cb {
                continue;
            }
            let cpair = if ca < cb {
                (ca.clone(), cb.clone())
            } else {
                (cb.clone(), ca.clone())
            };
            *cross_count.entry(cpair).or_default() += 1;
            candidates.push(((a.clone(), b.clone()), kind, ca.clone(), cb.clone()));
        }
        candidates.sort_by(|x, y| {
            let cx = {
                let k = if x.2 < x.3 {
                    (x.2.clone(), x.3.clone())
                } else {
                    (x.3.clone(), x.2.clone())
                };
                cross_count.get(&k).copied().unwrap_or(0)
            };
            let cy = {
                let k = if y.2 < y.3 {
                    (y.2.clone(), y.3.clone())
                } else {
                    (y.3.clone(), y.2.clone())
                };
                cross_count.get(&k).copied().unwrap_or(0)
            };
            cx.cmp(&cy).then_with(|| x.0.cmp(&y.0))
        });
        let surprising: Vec<SurprisingEdge> = candidates
            .into_iter()
            .take(max_rows)
            .map(|((from, to), kind, cf, ct)| SurprisingEdge {
                from,
                to,
                kind: kind.to_string(),
                from_subsystem: cf,
                to_subsystem: ct,
            })
            .collect();

        // God nodes: top symbols by inbound call count + top files by centrality,
        // merged and ranked by degree.
        let mut god: Vec<GodNode> = Vec::new();
        let mut sym_deg: Vec<(String, u64)> = inbound_calls.into_iter().collect();
        sym_deg.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        for (name, deg) in sym_deg.iter().take(max_rows) {
            // Represent by the first definition of the name; a callee name
            // with no definition (external/stdlib) is skipped, exactly like
            // the `find_symbol`-was-empty case this replaces.
            if let Some((id, kind, file, _)) = first_def.get(name) {
                god.push(GodNode {
                    id: id.clone(),
                    label: name.clone(),
                    file: file.clone(),
                    kind: kind.clone(),
                    degree: *deg,
                });
            }
        }
        for (file, deg) in self.file_centrality(max_rows)? {
            god.push(GodNode {
                id: format!("file:{file}"),
                label: file.clone(),
                file,
                kind: "file".to_string(),
                degree: deg,
            });
        }
        god.sort_by(|a, b| b.degree.cmp(&a.degree).then_with(|| a.label.cmp(&b.label)));
        god.truncate(max_rows);

        Ok(ArchReport {
            god_nodes: god,
            subsystems,
            surprising,
        })
    }
}

/// Derive a subsystem name (V15 Feature 2) from its member files: the longest
/// common path-segment DIRECTORY prefix (e.g. `src/graph/`), falling back to the
/// shortest member path when the files share no common directory.
fn community_name(files: &[String]) -> String {
    if files.is_empty() {
        return "misc".to_string();
    }
    let split: Vec<Vec<&str>> = files.iter().map(|f| f.split('/').collect()).collect();
    let min_len = split.iter().map(|s| s.len()).min().unwrap_or(0);
    let mut prefix: Vec<&str> = Vec::new();
    // Stop before the last segment of the shortest path (that's a filename, not
    // a directory), so the name is always a real containing directory.
    for i in 0..min_len.saturating_sub(1) {
        let seg = split[0][i];
        if split.iter().all(|s| s[i] == seg) {
            prefix.push(seg);
        } else {
            break;
        }
    }
    if !prefix.is_empty() {
        return format!("{}/", prefix.join("/"));
    }
    files
        .iter()
        .min_by_key(|f| f.len())
        .cloned()
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::{parse_file, Lang};

    #[test]
    fn architecture_groups_files_by_directory_prefix() {
        let dir = std::env::temp_dir().join(format!("ckg-arch-{}", uuid::Uuid::new_v4()));
        let idx = GraphIndex::open(&dir, ".ckg").expect("open");
        // Two cohesive directories, each internally coupled, with no cross edges.
        idx.index_file_graph(&parse_file(
            "src/graph/a.rs",
            "pub fn ga() { gb(); }\n",
            Lang::Rust,
        ))
        .unwrap();
        idx.index_file_graph(&parse_file(
            "src/graph/b.rs",
            "pub fn gb() { ga(); }\n",
            Lang::Rust,
        ))
        .unwrap();
        idx.index_file_graph(&parse_file(
            "src/ui/x.rs",
            "pub fn ux() { uy(); }\n",
            Lang::Rust,
        ))
        .unwrap();
        idx.index_file_graph(&parse_file(
            "src/ui/y.rs",
            "pub fn uy() { ux(); }\n",
            Lang::Rust,
        ))
        .unwrap();

        let report = idx.architecture(12, 2, 50).expect("arch");
        assert!(!report.god_nodes.is_empty(), "expected hubs");
        let names: Vec<&str> = report.subsystems.iter().map(|s| s.name.as_str()).collect();
        assert!(
            names.iter().any(|n| n.contains("src/graph")),
            "communities: {names:?}"
        );
        assert!(
            names.iter().any(|n| n.contains("src/ui")),
            "communities: {names:?}"
        );
        // Determinism: a second run yields the identical report.
        assert_eq!(idx.architecture(12, 2, 50).expect("arch2"), report);
        drop(idx);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
