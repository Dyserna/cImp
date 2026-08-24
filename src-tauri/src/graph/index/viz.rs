//! V15 Feature 4 — the **Graph View** queries: the bounded top-degree snapshot,
//! one file's ego subgraph, and the per-file presence the Workbench jump button
//! reads.
//!
//! All three are views of one FILE-level rollup ([`GraphIndex::viz_rollup`]),
//! which is the reason they belong together and apart from the rest of the
//! store: the rollup is where the caps live, and every cap here exists to keep
//! a hairball off the screen rather than to bound a query. Nothing is
//! persisted — the rollup is recomputed per call from `symbol`/`calls`/
//! `imports`.
//!
//! A submodule (V42 R13) for the mechanical reason: five methods, four caps and
//! two helpers that no other part of the store touches. The row types the
//! queries return (`VizNode`, `VizEdge`, `VizGraph`, `VizFileStatus`) stay in
//! the parent, where `graph::service` already names them; the private rollup
//! tuple alias came along, since only this module can spell it.

use std::collections::{HashMap, HashSet};

use crate::error::AppResult;

use super::{
    cell_str, resolve_import, Confidence, GraphIndex, Lang, VizEdge, VizFileStatus, VizGraph,
    VizNode,
};

/// The uncut file-level rollup behind every Graph View query: nodes keyed by
/// file path, the deduplicated edge list, and that list's index-aligned
/// rolled-up weights (see [`GraphIndex::viz_rollup`]).
type VizRollup = (HashMap<String, VizNode>, Vec<VizEdge>, Vec<u64>);

/// Most definitions one call edge may fan out to in the viz snapshot (a call
/// edge stores the callee NAME; common names resolve to dozens of defs).
const VIZ_CALL_FANOUT_MAX: usize = 4;
/// Hard cap on edges returned by a `viz_ego` query — a hub file can touch
/// hundreds of files, and the injected ego shares the Graph View's per-frame
/// budget with the rest of the rendered snapshot.
const VIZ_EGO_EDGES_MAX: usize = 200;
/// Edge budget per node for the viz snapshot's hard edge cap.
const VIZ_EDGES_PER_NODE: usize = 4;
/// Per-node drawn-neighbor quota: an edge survives only while one of its
/// endpoints still has quota (strongest edges kept first), so dense file
/// graphs stay readable instead of becoming a hairball.
const VIZ_NEIGHBORS_PER_NODE: usize = 3;

/// Confidence ordering for viz edge ranking (strongest first).
fn viz_conf_rank(c: &str) -> u8 {
    match c {
        "extracted" => 0,
        "inferred" => 1,
        _ => 2,
    }
}

/// The longest subsystem name (a directory prefix) that prefixes `file`, or
/// empty when the file falls outside every named subsystem.
fn viz_subsystem_of(sub_names: &[String], file: &str) -> String {
    sub_names
        .iter()
        .filter(|n| file.starts_with(n.as_str()))
        .max_by_key(|n| n.len())
        .cloned()
        .unwrap_or_default()
}

impl GraphIndex {
    /// V15 Feature 4: a bounded subgraph for the Graph View tab — FILE-level
    /// only. Symbol nodes made even medium projects too dense to render or
    /// read (thousands of nodes, most of them `contains` leaves), so
    /// symbol→symbol call edges are rolled up to edges between their
    /// containing files and `contains` edges (file→symbol by construction)
    /// are dropped entirely; intra-file calls self-collapse and vanish.
    /// Nodes are the top `max_nodes` highest-degree files carrying a
    /// subsystem label (color) and degree (size); edges carry kind (color)
    /// and the best confidence seen for the pair (dash). Offline, read-only.
    ///
    /// Edges are bounded too — the frontend pays per edge per frame (spring
    /// force + canvas stroke), so an uncapped edge list froze the whole
    /// webview on big projects:
    /// - a call to a many-definition name fans out to at most
    ///   [`VIZ_CALL_FANOUT_MAX`] candidate files (a call edge stores the
    ///   callee NAME; hyper-common names like `new` resolve to dozens of
    ///   definitions, and drawing caller × every-candidate is quadratic
    ///   noise, not signal);
    /// - duplicate (src, dst, kind) pairs collapse into one WEIGHTED edge
    ///   (weight = how many rolled-up call sites/imports it stands for),
    ///   keeping the highest confidence seen;
    /// - each node keeps at most [`VIZ_NEIGHBORS_PER_NODE`] drawn edges
    ///   (strongest first; an edge survives while either endpoint still has
    ///   quota), and the final list is capped at
    ///   `max_nodes * VIZ_EDGES_PER_NODE`.
    pub fn viz_snapshot(&self, max_nodes: usize) -> AppResult<VizGraph> {
        let max_nodes = max_nodes.max(1);
        let (meta, edges, weights) = self.viz_rollup()?;
        let sub_names = self.viz_subsystem_names();

        // Keep the top `max_nodes` by degree (ties by id for determinism).
        let mut nodes: Vec<VizNode> = meta.into_values().filter(|n| n.degree > 0).collect();
        nodes.sort_by(|a, b| b.degree.cmp(&a.degree).then_with(|| a.id.cmp(&b.id)));
        nodes.truncate(max_nodes);
        for n in &mut nodes {
            n.subsystem = viz_subsystem_of(&sub_names, &n.file);
        }
        let kept: HashSet<String> = nodes.iter().map(|n| n.id.clone()).collect();
        let mut weighted: Vec<(VizEdge, u64)> = edges
            .into_iter()
            .zip(weights)
            .filter(|(e, _)| kept.contains(&e.src) && kept.contains(&e.dst))
            .collect();

        // Drawn-edge cap, strongest first: order by rolled-up weight, then
        // confidence, then a deterministic key; each node gets at most
        // VIZ_NEIGHBORS_PER_NODE drawn incident edges (an edge draws while
        // EITHER endpoint still has quota, so a hub's strongest spokes stay
        // even after the hub itself is saturated), all under the global
        // max_nodes * VIZ_EDGES_PER_NODE bound. Edges over quota are KEPT
        // with `drawn: false` — the frontend's connections panel and
        // selection highlight need the full set; only ambient rendering and
        // the spring sim are bounded by the flag. Node degrees stay as
        // computed above.
        weighted.sort_by(|a, b| {
            b.1.cmp(&a.1)
                .then_with(|| viz_conf_rank(&a.0.confidence).cmp(&viz_conf_rank(&b.0.confidence)))
                .then_with(|| (&a.0.src, &a.0.dst, &a.0.kind).cmp(&(&b.0.src, &b.0.dst, &b.0.kind)))
        });
        let max_edges = max_nodes.saturating_mul(VIZ_EDGES_PER_NODE);
        let mut used: HashMap<String, usize> = HashMap::new();
        let mut drawn_count = 0usize;
        let mut final_edges: Vec<VizEdge> = Vec::with_capacity(weighted.len());
        for (mut e, _) in weighted {
            let su = used.get(&e.src).copied().unwrap_or(0);
            let du = used.get(&e.dst).copied().unwrap_or(0);
            if drawn_count < max_edges
                && (su < VIZ_NEIGHBORS_PER_NODE || du < VIZ_NEIGHBORS_PER_NODE)
            {
                e.drawn = true;
                drawn_count += 1;
                *used.entry(e.src.clone()).or_default() += 1;
                *used.entry(e.dst.clone()).or_default() += 1;
            }
            final_edges.push(e);
        }

        Ok(VizGraph {
            nodes,
            edges: final_edges,
        })
    }

    /// Workbench ⌖ support: per-file Graph View presence for a batch of
    /// repo-relative paths. `indexed` = the file exists in the graph at all;
    /// `degree` = its rolled-up file-level call/import degree (0 ⇒ the file
    /// can never appear in the snapshot, so there is nothing to jump to).
    /// One rollup pass covers the whole batch.
    pub fn viz_file_status(&self, paths: &[String]) -> AppResult<Vec<VizFileStatus>> {
        let (meta, _, _) = self.viz_rollup()?;
        Ok(paths
            .iter()
            .map(|p| match meta.get(&format!("file:{p}")) {
                Some(n) => VizFileStatus {
                    path: p.clone(),
                    indexed: true,
                    degree: n.degree,
                },
                None => VizFileStatus {
                    path: p.clone(),
                    indexed: false,
                    degree: 0,
                },
            })
            .collect())
    }

    /// Workbench ⌖ support: the 1-hop FILE ego of `path`, computed on the
    /// FULL rollup — i.e. regardless of the snapshot's top-N-by-degree cut —
    /// so a jump to a low-degree file can temporarily inject it (plus every
    /// file it calls/imports, either direction) into the rendered graph.
    /// Incident edges come strongest-first, capped at [`VIZ_EGO_EDGES_MAX`],
    /// all marked `drawn`. Empty when the file isn't indexed; a lone node
    /// when it has no connections.
    pub fn viz_ego(&self, path: &str) -> AppResult<VizGraph> {
        let id = format!("file:{path}");
        let (mut meta, edges, weights) = self.viz_rollup()?;
        if !meta.contains_key(&id) {
            return Ok(VizGraph::default());
        }
        let mut incident: Vec<(VizEdge, u64)> = edges
            .into_iter()
            .zip(weights)
            .filter(|(e, _)| e.src == id || e.dst == id)
            .collect();
        // Same strongest-first order as the snapshot's drawn-edge cap.
        incident.sort_by(|a, b| {
            b.1.cmp(&a.1)
                .then_with(|| viz_conf_rank(&a.0.confidence).cmp(&viz_conf_rank(&b.0.confidence)))
                .then_with(|| (&a.0.src, &a.0.dst, &a.0.kind).cmp(&(&b.0.src, &b.0.dst, &b.0.kind)))
        });
        incident.truncate(VIZ_EGO_EDGES_MAX);

        // Target first, then neighbors in edge-strength order.
        let mut ids: Vec<String> = vec![id.clone()];
        let mut seen: HashSet<String> = HashSet::from([id]);
        for (e, _) in &incident {
            for end in [&e.src, &e.dst] {
                if seen.insert(end.clone()) {
                    ids.push(end.clone());
                }
            }
        }
        let sub_names = self.viz_subsystem_names();
        let nodes: Vec<VizNode> = ids
            .into_iter()
            .filter_map(|nid| meta.remove(&nid))
            .map(|mut n| {
                n.subsystem = viz_subsystem_of(&sub_names, &n.file);
                n
            })
            .collect();
        let edges = incident
            .into_iter()
            .map(|(mut e, _)| {
                e.drawn = true;
                e
            })
            .collect();
        Ok(VizGraph { nodes, edges })
    }

    /// Subsystem names (directory prefixes) from the architecture pass — the
    /// viz node color buckets. Cheap reuse of the pass's named buckets —
    /// `max_rows = 0` because only `subsystems` is consumed, so the god-node
    /// and surprising-edge computations (including the file-centrality scan)
    /// are skipped entirely.
    fn viz_subsystem_names(&self) -> Vec<String> {
        self.architecture(64, 1, 0)
            .unwrap_or_default()
            .subsystems
            .into_iter()
            .map(|s| s.name)
            .collect()
    }

    /// The shared FILE-level rollup behind the Graph View queries
    /// (`viz_snapshot` / `viz_file_status` / `viz_ego`): EVERY indexed file
    /// as a `VizNode` (degree = unique rolled-up call/import edges touching
    /// it, subsystem left empty) plus the deduplicated edge list with its
    /// index-aligned rolled-up weights. No top-N cut and no drawn-edge cap —
    /// each caller applies its own bounds.
    fn viz_rollup(&self) -> AppResult<VizRollup> {
        // Symbol table — not nodes anymore, just the lookups that resolve a
        // call edge (symbol-id src, callee-NAME dst) to its file endpoints.
        let sym_rows = self.query("?[id, name, file] := *symbol{id, name, file}")?;
        let mut sym_file: HashMap<String, String> = HashMap::new();
        let mut name_to_files: HashMap<String, Vec<String>> = HashMap::new();
        for r in &sym_rows.rows {
            let id = cell_str(r, 0);
            let name = cell_str(r, 1);
            let file = cell_str(r, 2);
            name_to_files.entry(name).or_default().push(file.clone());
            sym_file.insert(id, file);
        }
        let file_rows = self.query("?[path, lang] := *file{path, lang}")?;
        let mut meta: HashMap<String, VizNode> = HashMap::new();
        let mut file_lang: HashMap<String, String> = HashMap::new();
        let mut known_files: HashSet<String> = HashSet::new();
        for r in &file_rows.rows {
            let path = cell_str(r, 0);
            file_lang.insert(path.clone(), cell_str(r, 1));
            let id = format!("file:{path}");
            meta.insert(
                id.clone(),
                VizNode {
                    id,
                    label: path.clone(),
                    file: path.clone(),
                    kind: "file".to_string(),
                    degree: 0,
                    subsystem: String::new(),
                },
            );
            known_files.insert(path);
        }

        let multi = self.multi_candidate_names()?;
        let mut edges: Vec<VizEdge> = Vec::new();
        // Rolled-up weight per edge, index-aligned with `edges`: how many
        // call sites / imports the collapsed (src, dst, kind) pair stands
        // for. Drives the strongest-first drawn-edge cap below.
        let mut weights: Vec<u64> = Vec::new();
        // (src, dst, kind) → index into `edges`: rolled-up duplicates (many
        // symbol pairs between the same two files) collapse into one edge
        // that keeps the best confidence seen.
        let mut edge_ix: HashMap<(String, String, &'static str), usize> = HashMap::new();
        let push_edge = |edges: &mut Vec<VizEdge>,
                         weights: &mut Vec<u64>,
                         edge_ix: &mut HashMap<(String, String, &'static str), usize>,
                         meta: &mut HashMap<String, VizNode>,
                         a: &str,
                         b: &str,
                         kind: &'static str,
                         conf: Confidence| {
            if a == b || !meta.contains_key(a) || !meta.contains_key(b) {
                return;
            }
            if let Some(&i) = edge_ix.get(&(a.to_string(), b.to_string(), kind)) {
                weights[i] += 1;
                if viz_conf_rank(conf.tag()) < viz_conf_rank(&edges[i].confidence) {
                    edges[i].confidence = conf.tag().to_string();
                }
                return;
            }
            edge_ix.insert((a.to_string(), b.to_string(), kind), edges.len());
            if let Some(n) = meta.get_mut(a) {
                n.degree += 1;
            }
            if let Some(n) = meta.get_mut(b) {
                n.degree += 1;
            }
            edges.push(VizEdge {
                src: a.to_string(),
                dst: b.to_string(),
                kind: kind.to_string(),
                confidence: conf.tag().to_string(),
                drawn: false,
            });
            weights.push(1);
        };

        // Call edges (name-resolved), rolled up to file→file.
        let call_rows = self.query(
            r#"?[src, dst, conf] := *edge{kind: k, src, dst, confidence: conf}, k == "call""#,
        )?;
        for r in &call_rows.rows {
            let src = cell_str(r, 0);
            let dst = cell_str(r, 1);
            let Some(from_file) = sym_file.get(&src) else {
                continue;
            };
            let conf = if multi.contains(&dst) {
                Confidence::Ambiguous
            } else {
                Confidence::from_tag(&cell_str(r, 2))
            };
            if let Some(files) = name_to_files.get(&dst) {
                let mut files: Vec<&String> = files.iter().collect();
                files.sort(); // deterministic pick when the fan-out is capped
                files.dedup();
                let from_id = format!("file:{from_file}");
                for callee_file in files.into_iter().take(VIZ_CALL_FANOUT_MAX) {
                    push_edge(
                        &mut edges,
                        &mut weights,
                        &mut edge_ix,
                        &mut meta,
                        &from_id,
                        &format!("file:{callee_file}"),
                        "call",
                        conf,
                    );
                }
            }
        }
        // Import edges (resolved file→file).
        let import_rows = self.query(
            r#"?[src, dst, conf] := *edge{kind: k, src, dst, confidence: conf}, k == "import""#,
        )?;
        for r in &import_rows.rows {
            let from_file = cell_str(r, 0);
            let module = cell_str(r, 1);
            let lang = Lang::from_tag(file_lang.get(&from_file).map(|s| s.as_str()).unwrap_or(""));
            if let Some(target) = resolve_import(lang, &from_file, &module, &known_files) {
                let conf = Confidence::from_tag(&cell_str(r, 2));
                push_edge(
                    &mut edges,
                    &mut weights,
                    &mut edge_ix,
                    &mut meta,
                    &format!("file:{from_file}"),
                    &format!("file:{target}"),
                    "import",
                    conf,
                );
            }
        }

        Ok((meta, edges, weights))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::{parse_file, Lang};

    /// Regression for the Graph View freeze: a call edge stores the callee
    /// NAME, and the viz snapshot used to fan out to EVERY definition of that
    /// name — hyper-common names (`new`: 33 defs in this repo) multiplied one
    /// call site into dozens of drawn edges, with no overall edge cap, and the
    /// frontend's per-edge-per-frame cost pinned the webview thread.
    /// Also guards the file-level contract: files are the only nodes, calls
    /// roll up to file→file, and `contains` edges are gone.
    #[test]
    fn viz_snapshot_caps_call_fanout_and_total_edges() {
        let dir = std::env::temp_dir().join(format!("ckg-viz-{}", uuid::Uuid::new_v4()));
        let idx = GraphIndex::open(&dir, ".ckg").expect("open");
        for i in 0..9 {
            idx.index_file_graph(&parse_file(
                &format!("src/d{i}.rs"),
                "pub fn dup() {}\n",
                Lang::Rust,
            ))
            .unwrap();
        }
        idx.index_file_graph(&parse_file(
            "src/main.rs",
            "pub fn caller() { dup(); }\n",
            Lang::Rust,
        ))
        .unwrap();

        let g = idx.viz_snapshot(100).expect("viz");
        // File-level graph: no symbol nodes, no contains edges, and the call
        // edges connect file:… ids.
        assert!(
            g.nodes.iter().all(|n| n.kind == "file"),
            "nodes: {:?}",
            g.nodes
        );
        assert!(
            g.edges.iter().all(|e| e.kind != "contains"),
            "edges: {:?}",
            g.edges
        );
        let calls: Vec<_> = g.edges.iter().filter(|e| e.kind == "call").collect();
        assert!(calls
            .iter()
            .all(|e| e.src.starts_with("file:") && e.dst.starts_with("file:")));
        assert_eq!(
            calls.len(),
            VIZ_CALL_FANOUT_MAX,
            "one call site × 9 same-named defs (in 9 files) is capped, not fanned out: {calls:?}"
        );
        // A multi-candidate callee name renders as ambiguous (dotted).
        assert!(calls.iter().all(|e| e.confidence == "ambiguous"));
        // The overall bound the frontend's frame budget relies on applies to
        // DRAWN edges — over-quota edges ride along with drawn=false for the
        // connections panel.
        let drawn = g.edges.iter().filter(|e| e.drawn).count();
        assert!(drawn <= g.nodes.len() * VIZ_EDGES_PER_NODE);

        drop(idx);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The Workbench ⌖ support queries work on the FULL rollup, not the
    /// snapshot's top-N-by-degree cut: `viz_file_status` reports per-file
    /// presence + degree (0-degree and unindexed files disable the jump
    /// button), and `viz_ego` returns a file the cut dropped plus its 1-hop
    /// file neighborhood so the frontend can inject it temporarily.
    #[test]
    fn viz_file_status_and_ego_ignore_the_top_n_cut() {
        let dir = std::env::temp_dir().join(format!("ckg-viz-ego-{}", uuid::Uuid::new_v4()));
        let idx = GraphIndex::open(&dir, ".ckg").expect("open");
        // hub is called from three files (degree 3); leaf has exactly one
        // edge (its call to hub); lone is indexed but fully disconnected.
        idx.index_file_graph(&parse_file("src/hub.rs", "pub fn hub() {}\n", Lang::Rust))
            .unwrap();
        idx.index_file_graph(&parse_file(
            "src/s1.rs",
            "pub fn s1() { hub(); }\n",
            Lang::Rust,
        ))
        .unwrap();
        idx.index_file_graph(&parse_file(
            "src/s2.rs",
            "pub fn s2() { hub(); }\n",
            Lang::Rust,
        ))
        .unwrap();
        idx.index_file_graph(&parse_file(
            "src/leaf.rs",
            "pub fn leaf() { hub(); }\n",
            Lang::Rust,
        ))
        .unwrap();
        idx.index_file_graph(&parse_file("src/lone.rs", "pub fn lone() {}\n", Lang::Rust))
            .unwrap();

        // A max_nodes=1 snapshot keeps only the hub — leaf falls off the cut.
        let snap = idx.viz_snapshot(1).expect("snapshot");
        assert_eq!(snap.nodes.len(), 1);
        assert_eq!(snap.nodes[0].id, "file:src/hub.rs");

        let status = idx
            .viz_file_status(&[
                "src/leaf.rs".into(),
                "src/lone.rs".into(),
                "src/nope.rs".into(),
            ])
            .expect("status");
        assert_eq!(status.len(), 3);
        assert!(
            status[0].indexed && status[0].degree >= 1,
            "leaf: {:?}",
            status[0]
        );
        assert!(
            status[1].indexed && status[1].degree == 0,
            "lone: {:?}",
            status[1]
        );
        assert!(
            !status[2].indexed && status[2].degree == 0,
            "nope: {:?}",
            status[2]
        );

        // Ego of the dropped file: itself first, its neighbor, one drawn edge.
        let ego = idx.viz_ego("src/leaf.rs").expect("ego");
        let ids: Vec<&str> = ego.nodes.iter().map(|n| n.id.as_str()).collect();
        assert_eq!(ids, vec!["file:src/leaf.rs", "file:src/hub.rs"]);
        assert_eq!(ego.edges.len(), 1);
        assert!(ego.edges[0].drawn);
        assert_eq!(ego.edges[0].src, "file:src/leaf.rs");
        assert_eq!(ego.edges[0].dst, "file:src/hub.rs");
        // A disconnected file egos to a lone node; an unindexed path to nothing.
        let lone = idx.viz_ego("src/lone.rs").expect("ego lone");
        assert_eq!(lone.nodes.len(), 1);
        assert!(lone.edges.is_empty());
        assert!(idx
            .viz_ego("src/nope.rs")
            .expect("ego nope")
            .nodes
            .is_empty());

        drop(idx);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
