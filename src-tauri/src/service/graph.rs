//! The code-graph use cases: build and ignore-list management, the Analyses
//! surface, the Graph View's bounded subgraphs, project memory and facts, and
//! the context-injection preview.
//!
//! ## What the A1 graph run found
//!
//! One handle and a lot of shaping. Every command in this module talks to
//! [`crate::graph::GraphService`] — the warm per-root index — and nothing else,
//! so unlike the settings save there is no web of collaborators to name. What
//! there IS, and what makes these worth moving, is the **wire shaping**: nine
//! of these commands exist to turn the index's internal reports into the flat
//! row structs the Code Intelligence and Graph View surfaces render
//! ([`ImpactResult`], [`PathResult`], [`ArchResult`], [`VizGraphResult`], …).
//! That mapping is the thing a UI depends on byte-for-byte, and the thing a
//! wire boundary should not be deciding.
//!
//! ## The handle is borrowed, not behind a trait — deliberately
//!
//! [`GraphIndexHost`](crate::service::sink::GraphIndexHost) and
//! [`ChecksLangStats`](crate::service::checks::ChecksLangStats) exist because
//! the *settings* and *checks* use cases reach into the graph; a trait is what
//! keeps another domain's capability out of their signature and their tests off
//! an unconstructible `GraphService`. Here the index is not another domain's
//! capability — it is this domain's own handle, the way `SettingsHandle` is the
//! settings service's. Wrapping it would produce a `GraphHost` with a method per
//! `GraphService` method, which is `GraphService` with extra steps and the exact
//! shape `GraphIndexHost`'s own doc comment refuses.
//!
//! That leaves these use cases un-runnable headlessly until `GraphService`
//! itself is — its `AppHandle` covers five `state::<T>()` reaches, which is
//! Phase A2's named cluster, not A1's. What A1 buys is that the shaping, the
//! argument parsing and the root fallback are testable and callable without a
//! WebView today, and that the day A2 lands, so is everything else here.
//!
//! ## What did NOT change
//!
//! [`GraphService::spawn_rebuild`](crate::graph::GraphService::spawn_rebuild) is
//! still called with [`RebuildOrigin::User`](crate::graph::RebuildOrigin) by
//! exactly the two paths a user drives (the Rebuild button, the Settings
//! language toggle) — that origin is what allows the V30 session-push
//! announcement, and widening it would announce every watcher tick.
//! [`CodeIntelService::set_language_enabled`] still refuses an unsupported tag
//! and still skips the whole mutate-and-rebuild when the desired state already
//! holds, because a redundant rebuild re-indexes and re-embeds the project for
//! nothing. And [`CodeIntelService::note_review`] still rejects an unknown
//! action rather than ignoring it: on a security control, a typo must not read
//! as "reviewed, nothing happened".

use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::error::{AppError, AppResult};
use crate::graph::GraphService;
use crate::service::project_root;
use crate::settings::SettingsHandle;

// ── wire shapes ──────────────────────────────────────────────────────────
//
// These live beside the use case that produces them (the `service::checks`
// precedent). Nothing else in the crate names them; the frontend does.

/// V10: one candidate dead export (unused public symbol) for the Analyses tab.
#[derive(serde::Serialize)]
pub struct DeadExportRow {
    pub name: String,
    pub kind: String,
    pub file: String,
    pub line: u32,
    pub signature: String,
}

/// V12 Phase B (Analyses): one symbol changed since `HEAD` (the working-tree
/// diff's root set).
#[derive(serde::Serialize)]
pub struct ChangedSymbolRow {
    pub name: String,
    pub kind: String,
    pub file: String,
    pub line: u32,
}

/// V12 Phase B (Analyses): one transitive dependent of a changed symbol.
#[derive(serde::Serialize)]
pub struct DependentRow {
    pub name: String,
    pub kind: String,
    pub file: String,
    pub line: u32,
    pub depth: u32,
    pub approx: bool,
    /// V15 Feature 3: weakest edge confidence along the discovery chain
    /// (`extracted`/`inferred`/`ambiguous`).
    pub confidence: String,
}

/// V12 Phase B (Analyses): the working-tree diff's blast radius — the changed
/// symbols, their transitive dependents, and any changed files the graph
/// doesn't index (docs/configs/etc.).
#[derive(serde::Serialize)]
pub struct ImpactResult {
    pub changed: Vec<ChangedSymbolRow>,
    pub dependents: Vec<DependentRow>,
    pub unindexed: Vec<String>,
}

/// One node on a traced path, serialized for the Code Intelligence tab.
#[derive(serde::Serialize)]
pub struct PathNodeRow {
    pub id: String,
    pub label: String,
    pub file: String,
    pub line: u32,
    pub kind: String,
    pub edge_to_next: Option<String>,
    pub confidence: Option<String>,
}

/// The result of a path trace. `found=false` means no path within the hop bound
/// (or an unresolvable endpoint).
#[derive(serde::Serialize)]
pub struct PathResult {
    pub found: bool,
    pub nodes: Vec<PathNodeRow>,
    pub hops: usize,
    pub equal_alternatives: u64,
}

#[derive(serde::Serialize)]
pub struct GodNodeRow {
    pub id: String,
    pub label: String,
    pub file: String,
    pub kind: String,
    pub degree: u64,
}

#[derive(serde::Serialize)]
pub struct SubsystemRow {
    pub name: String,
    pub size: usize,
    pub files: Vec<String>,
    pub hub: String,
}

#[derive(serde::Serialize)]
pub struct SurprisingRow {
    pub from: String,
    pub to: String,
    pub kind: String,
    pub from_subsystem: String,
    pub to_subsystem: String,
}

#[derive(serde::Serialize)]
pub struct ArchResult {
    pub god_nodes: Vec<GodNodeRow>,
    pub subsystems: Vec<SubsystemRow>,
    pub surprising: Vec<SurprisingRow>,
}

#[derive(serde::Serialize)]
pub struct VizNodeRow {
    pub id: String,
    pub label: String,
    pub file: String,
    pub kind: String,
    pub degree: u64,
    pub subsystem: String,
}

#[derive(serde::Serialize)]
pub struct VizEdgeRow {
    pub src: String,
    pub dst: String,
    pub kind: String,
    pub confidence: String,
    /// `false` = over the per-node drawn quota: listed/highlighted by the
    /// frontend but not rendered as an ambient line.
    pub drawn: bool,
}

#[derive(serde::Serialize)]
pub struct VizGraphResult {
    pub nodes: Vec<VizNodeRow>,
    pub edges: Vec<VizEdgeRow>,
}

/// Per-file Graph View presence (Workbench ⌖ button state).
#[derive(serde::Serialize)]
pub struct VizFileStatusRow {
    pub path: String,
    /// The file exists in the graph index at all.
    pub indexed: bool,
    /// Rolled-up file-level call/import degree (0 = nothing to jump to).
    pub degree: u64,
}

// ── argument shaping ─────────────────────────────────────────────────────

/// Turn a picked absolute path into the `graph.ignore` glob
/// [`CodeIntelService::ignore_pick`] returns.
///
/// Project-relative and anchored with a leading `/` when the pick lies under a
/// known graph root (longest root wins), with a trailing `/` for folders. A pick
/// outside every root falls back to the absolute path with forward slashes — it
/// won't match anything, but it lands visibly in the editor where the user can
/// correct it, rather than being silently dropped.
fn to_ignore_glob(path: &Path, is_dir: bool, roots: &[PathBuf]) -> String {
    // Longest matching root wins so a nested root maps to the shorter rel.
    let rel = roots
        .iter()
        .filter(|r| path.starts_with(r))
        .max_by_key(|r| r.components().count())
        .and_then(|r| path.strip_prefix(r).ok());
    let mut glob = match rel {
        // Leading `/` anchors to the project root: the user picked THIS
        // `docs/`, not every directory named `docs` at any depth.
        Some(rel) => format!("/{}", rel.to_string_lossy().replace('\\', "/")),
        None => path.to_string_lossy().replace('\\', "/"),
    };
    if is_dir && !glob.ends_with('/') {
        glob.push('/');
    }
    glob
}

/// The edge kinds a path trace may walk, from the frontend's optional tag list.
///
/// An absent, empty or wholly unrecognised list means "all three" rather than
/// "none": a filter the caller could not express must not silently answer
/// `found: false` for a path that exists.
fn parse_path_kinds(kinds: Option<Vec<String>>) -> Vec<crate::graph::EdgeKind> {
    use crate::graph::EdgeKind;
    let all = || vec![EdgeKind::Call, EdgeKind::Import, EdgeKind::Contains];
    let Some(ks) = kinds else { return all() };
    let mut out = Vec::new();
    for k in ks {
        match k.trim().to_ascii_lowercase().as_str() {
            "call" => out.push(EdgeKind::Call),
            "import" => out.push(EdgeKind::Import),
            "contains" => out.push(EdgeKind::Contains),
            _ => {}
        }
    }
    if out.is_empty() {
        all()
    } else {
        out
    }
}

/// The code-graph use cases, over one borrowed handle — same shape and
/// rationale as [`crate::service::tabs::TabService`].
///
/// Named `CodeIntelService` rather than `GraphService` because the handle it
/// borrows already has that name: [`crate::graph::GraphService`] is the warm
/// index, and this is the list of things the UI can ask of it — the same
/// distinction `service::tabs` draws against `tabs::registry`. `CodeIntel` is
/// also what the surface is called on screen.
pub struct CodeIntelService<'a> {
    index: &'a Arc<GraphService>,
}

impl<'a> CodeIntelService<'a> {
    pub fn new(index: &'a Arc<GraphService>) -> Self {
        Self { index }
    }

    /// V9-01: trigger a full rebuild of the project's code graph. Returns
    /// immediately — the build runs on a worker thread and reports progress via
    /// the `graph-status` event. A no-op when a build for that root is already
    /// in flight.
    ///
    /// The origin is `User`: this is the one graph path allowed to announce
    /// itself on the V30 session-push bus (and only if it also runs long enough
    /// to matter).
    pub fn rebuild(&self, root: Option<String>) -> AppResult<()> {
        let root = project_root(root)?;
        self.index
            .spawn_rebuild(root, crate::graph::RebuildOrigin::User);
        Ok(())
    }

    /// Open a native file/folder picker for the Settings "Ignore" editor and
    /// return a gitignore-style glob for the selection (see [`to_ignore_glob`]
    /// for the shape). `None` when the user cancels.
    pub async fn ignore_pick(&self, folder: bool) -> AppResult<Option<String>> {
        let start = std::env::current_dir().ok();
        // rfd's sync dialog blocks its thread (native message pump) — keep it
        // off the async runtime's core threads.
        let picked = tauri::async_runtime::spawn_blocking(move || {
            let mut d = rfd::FileDialog::new().set_title(if folder {
                "Choose a folder for the graph to ignore"
            } else {
                "Choose a file for the graph to ignore"
            });
            if let Some(s) = start {
                d = d.set_directory(s);
            }
            if folder {
                d.pick_folder()
            } else {
                d.pick_file()
            }
        })
        .await
        .map_err(|e| AppError::Settings(format!("picker task: {e}")))?;
        let Some(path) = picked else { return Ok(None) };

        let mut roots: Vec<PathBuf> = self
            .index
            .statuses()
            .iter()
            .map(|s| PathBuf::from(&s.root))
            .collect();
        // The launch dir is the primary project even before its first build.
        if let Ok(cwd) = std::env::current_dir() {
            roots.push(cwd);
        }
        Ok(Some(to_ignore_glob(&path, folder, &roots)))
    }

    /// V9-01 Phase G: force a full re-embed of the project's doc chunks (drops
    /// the vector store, then backfills). No-op when semantic search is off.
    pub fn rebuild_embeddings(&self, root: Option<String>) -> AppResult<()> {
        let root = project_root(root)?;
        self.index.spawn_rebuild_embeddings(root);
        Ok(())
    }

    /// V10 (Analyses): candidate unused public symbols — public/exported defs
    /// with no reference and no inbound call edge. Candidates only; the UI
    /// states the false-positive caveat.
    pub fn dead_exports(&self, root: Option<String>) -> AppResult<Vec<DeadExportRow>> {
        let root = project_root(root)?;
        Ok(self
            .index
            .dead_exports(&root)?
            .into_iter()
            .map(|s| DeadExportRow {
                name: s.name,
                kind: s.kind,
                file: s.file,
                line: s.start_line,
                signature: s.signature,
            })
            .collect())
    }

    /// V10 (Analyses): import cycles between files (each a loop of ≥ 2 files
    /// that transitively import one another).
    pub fn cycles(&self, root: Option<String>) -> AppResult<Vec<Vec<String>>> {
        let root = project_root(root)?;
        self.index.import_cycles(&root)
    }

    /// V12 Phase B (Analyses): "what does my current working-tree change
    /// affect?" — diff mode only (the `symbols`-scoped mode is MCP-tool only,
    /// where an agent supplies explicit roots). Errors with a "requires git"
    /// message when `root` isn't a git repository.
    pub fn impact(&self, root: Option<String>) -> AppResult<ImpactResult> {
        let root = project_root(root)?;
        let report = self.index.impact(&root)?;
        Ok(ImpactResult {
            changed: report
                .changed
                .into_iter()
                .map(|s| ChangedSymbolRow {
                    name: s.name,
                    kind: s.kind,
                    file: s.file,
                    line: s.start_line,
                })
                .collect(),
            dependents: report
                .dependents
                .into_iter()
                .map(|d| DependentRow {
                    name: d.symbol.name,
                    kind: d.symbol.kind,
                    file: d.symbol.file,
                    line: d.symbol.start_line,
                    depth: d.depth,
                    approx: d.approx,
                    confidence: d.confidence.tag().to_string(),
                })
                .collect(),
            unindexed: report.unindexed,
        })
    }

    /// V15 Feature 1 (Architecture): trace the shortest path between two
    /// entities through the call/import/containment graph.
    pub fn path(
        &self,
        root: Option<String>,
        from: &str,
        to: &str,
        kinds: Option<Vec<String>>,
        symmetric: Option<bool>,
    ) -> AppResult<PathResult> {
        let root = project_root(root)?;
        let kinds = parse_path_kinds(kinds);
        let hit = self.index.shortest_path(
            &root,
            from.trim(),
            to.trim(),
            &kinds,
            symmetric.unwrap_or(false),
        )?;
        Ok(match hit {
            Some(h) => PathResult {
                found: true,
                nodes: h
                    .nodes
                    .into_iter()
                    .map(|n| PathNodeRow {
                        id: n.id,
                        label: n.label,
                        file: n.file,
                        line: n.line,
                        kind: n.kind,
                        edge_to_next: n.edge_to_next,
                        confidence: n.confidence.map(|c| c.tag().to_string()),
                    })
                    .collect(),
                hops: h.hops,
                equal_alternatives: h.equal_alternatives,
            },
            None => PathResult {
                found: false,
                nodes: Vec::new(),
                hops: 0,
                equal_alternatives: 0,
            },
        })
    }

    /// V15 Feature 2 (Architecture): the system-shape overview — god nodes,
    /// subsystems, and surprising cross-subsystem edges.
    pub fn architecture(&self, root: Option<String>) -> AppResult<ArchResult> {
        let root = project_root(root)?;
        let r = self.index.architecture(&root)?;
        Ok(ArchResult {
            god_nodes: r
                .god_nodes
                .into_iter()
                .map(|g| GodNodeRow {
                    id: g.id,
                    label: g.label,
                    file: g.file,
                    kind: g.kind,
                    degree: g.degree,
                })
                .collect(),
            subsystems: r
                .subsystems
                .into_iter()
                .map(|s| SubsystemRow {
                    name: s.name,
                    size: s.size,
                    files: s.files,
                    hub: s.hub,
                })
                .collect(),
            surprising: r
                .surprising
                .into_iter()
                .map(|e| SurprisingRow {
                    from: e.from,
                    to: e.to,
                    kind: e.kind,
                    from_subsystem: e.from_subsystem,
                    to_subsystem: e.to_subsystem,
                })
                .collect(),
        })
    }

    /// V15 Feature 4 (Graph View): a bounded {nodes, edges} subgraph for the
    /// live visualization.
    pub fn viz_snapshot(&self, root: Option<String>) -> AppResult<VizGraphResult> {
        let root = project_root(root)?;
        Ok(viz_result(self.index.viz_snapshot(&root)?))
    }

    /// Workbench ⌖ support: per-file Graph View presence for a batch of
    /// repo-relative paths — the jump button disables for unindexed or
    /// connection-less files.
    pub fn viz_file_status(
        &self,
        root: Option<String>,
        paths: &[String],
    ) -> AppResult<Vec<VizFileStatusRow>> {
        let root = project_root(root)?;
        Ok(self
            .index
            .viz_file_status(&root, paths)?
            .into_iter()
            .map(|s| VizFileStatusRow {
                path: s.path,
                indexed: s.indexed,
                degree: s.degree,
            })
            .collect())
    }

    /// Workbench ⌖ support: the 1-hop FILE ego of `path` regardless of the
    /// snapshot's top-N-by-degree cut — the Graph View injects it temporarily
    /// when a jump targets a file the rendered snapshot dropped.
    pub fn viz_ego(&self, root: Option<String>, path: &str) -> AppResult<VizGraphResult> {
        let root = project_root(root)?;
        Ok(viz_result(self.index.viz_ego(&root, path)?))
    }

    /// V10 (Memory): the project's session/action memory — current session, its
    /// working set, notes (pinned + current-session), and the recent-sessions
    /// list.
    pub fn memory(&self, root: Option<String>) -> AppResult<crate::graph::MemorySnapshot> {
        let root = project_root(root)?;
        Ok(self.index.memory_snapshot(&root))
    }

    /// V10 (Memory): clear one session's memory (`session` = its id) or the
    /// whole project's memory (`session` omitted).
    pub fn memory_clear(&self, root: Option<String>, session: Option<String>) -> AppResult<()> {
        let root = project_root(root)?;
        let session = session.filter(|s| !s.trim().is_empty());
        self.index.mem_clear(&root, session.as_deref())
    }

    /// V10 (Memory): pin/unpin a note (pinned notes survive session eviction
    /// and show project-wide).
    pub fn note_set_pinned(
        &self,
        root: Option<String>,
        note_id: &str,
        pinned: bool,
    ) -> AppResult<()> {
        let root = project_root(root)?;
        self.index.mem_set_note_pinned(&root, note_id, pinned)
    }

    /// V32 Phase C2 (Memory): resolve one QUARANTINED note — `action` is
    /// `"promote"` (clear the taint; the note becomes ordinary memory, pinned
    /// state preserved) or `"discard"` (delete it).
    ///
    /// One method rather than two: the two actions are the two halves of one
    /// review decision, always rendered side by side, and an unknown `action`
    /// is REJECTED rather than silently ignored — a typo must not read as
    /// "reviewed, nothing happened" on a security control.
    pub fn note_review(&self, root: Option<String>, note_id: &str, action: &str) -> AppResult<()> {
        let root = project_root(root)?;
        match action {
            "promote" => self.index.mem_promote_note(&root, note_id),
            "discard" => self.index.mem_delete_note(&root, note_id),
            other => Err(AppError::Graph(format!(
                "unknown note review action `{other}` (expected \"promote\" or \"discard\")"
            ))),
        }
    }

    /// V12 Phase E (Memory): the project's durable facts (pinned first, then
    /// newest), excluding archived ones.
    pub fn facts(&self, root: Option<String>) -> AppResult<Vec<crate::graph::ProjectFact>> {
        let root = project_root(root)?;
        Ok(self.index.list_project_facts(&root, false, 200))
    }

    /// V12 Phase E (Memory): pin / unpin / archive / delete one project fact.
    /// An unknown action is rejected, for [`Self::note_review`]'s reason.
    pub fn fact_update(&self, root: Option<String>, id: &str, action: &str) -> AppResult<()> {
        let root = project_root(root)?;
        match action {
            "pin" => self.index.set_fact_pinned(&root, id, true),
            "unpin" => self.index.set_fact_pinned(&root, id, false),
            "archive" => self.index.set_fact_archived(&root, id, true),
            "delete" => self.index.delete_fact(&root, id),
            other => Err(AppError::Graph(format!(
                "unknown fact action: {other} (expected pin|unpin|archive|delete)"
            ))),
        }
    }

    /// V12 Phase E (Memory): manually add a project fact from the Facts UI's
    /// "add fact" input (recorded with `source_session = "manual"`).
    pub fn fact_add(&self, root: Option<String>, text: &str, pin: Option<bool>) -> AppResult<()> {
        let root = project_root(root)?;
        self.index
            .add_project_fact_manual(&root, text, pin.unwrap_or(false))
    }

    /// V10 (Context): preview what context injection WOULD prepend for
    /// `prompt`, bypassing the `context_injection` toggle (so the user can tune
    /// before enabling). No `session_id` — the preview isn't tied to a live
    /// session.
    pub fn context_preview(
        &self,
        prompt: &str,
        root: Option<String>,
    ) -> AppResult<crate::graph::RetrieveResult> {
        let root = project_root(root)?;
        Ok(self.index.retrieve_context(&root, prompt, None))
    }

    /// V9-02: the project's language census for the Code Graph tab's language
    /// buttons — every language present on disk with its file count and
    /// green/yellow/red classification. Walks the tree fresh each call, so the
    /// frontend calls it on tab open and after a rebuild, not on a poll.
    pub fn language_census(&self, root: Option<String>) -> AppResult<Vec<crate::graph::LangCensus>> {
        let root = project_root(root)?;
        Ok(self.index.language_census(&root))
    }

    /// V9-02: add or remove a language from the code graph's index set, then
    /// kick a full rebuild so the change takes effect — indexing new files (and
    /// embedding them when semantic search is on) or dropping the removed
    /// language's rows.
    ///
    /// Two refusals, both load-bearing: an unsupported tag is an error rather
    /// than a silently-stored string, and a toggle to the state that already
    /// holds returns without mutating or rebuilding — a redundant rebuild
    /// re-indexes and re-embeds the whole project for nothing.
    pub fn set_language_enabled(
        &self,
        settings: &SettingsHandle,
        lang: &str,
        enabled: bool,
        root: Option<String>,
    ) -> AppResult<()> {
        let tag = lang.trim().to_ascii_lowercase();
        if crate::graph::Lang::from_tag(&tag) == crate::graph::Lang::Other {
            return Err(AppError::Settings(format!(
                "unsupported graph language: {lang}"
            )));
        }
        let already = settings.current().graph.languages.iter().any(|l| l == &tag);
        if enabled == already {
            return Ok(());
        }
        settings.mutate(move |cur| {
            let langs = &mut cur.graph.languages;
            if enabled {
                langs.push(tag);
            } else {
                langs.retain(|l| l != &tag);
            }
        });
        let root = project_root(root)?;
        // A Settings language toggle is a user action, like Rebuild.
        self.index
            .spawn_rebuild(root, crate::graph::RebuildOrigin::User);
        Ok(())
    }
}

/// The shared `{nodes, edges}` mapping behind [`CodeIntelService::viz_snapshot`]
/// and [`CodeIntelService::viz_ego`]: the two answer the same wire shape from
/// the same internal one, and this is the one place those two commands were a
/// copy of each other.
fn viz_result(g: crate::graph::VizGraph) -> VizGraphResult {
    VizGraphResult {
        nodes: g
            .nodes
            .into_iter()
            .map(|n| VizNodeRow {
                id: n.id,
                label: n.label,
                file: n.file,
                kind: n.kind,
                degree: n.degree,
                subsystem: n.subsystem,
            })
            .collect(),
        edges: g
            .edges
            .into_iter()
            .map(|e| VizEdgeRow {
                src: e.src,
                dst: e.dst,
                kind: e.kind,
                confidence: e.confidence,
                drawn: e.drawn,
            })
            .collect(),
    }
}

/// V9-01: recent graph tool calls (cloud Claude + offload worker), newest
/// first. The store is process-wide across every indexed root; pass
/// `scoped: true` (with an optional `root`) to filter to one project's calls —
/// the Graph View pulse feed uses this so another project's activity can't light
/// up same-named nodes here.
///
/// The persistent activity store also holds offload runs; this use case keeps
/// its historical contract of graph calls only. The Tool Activity tab uses
/// [`crate::service::view::activity_since`] and sees everything.
///
/// Free rather than a [`CodeIntelService`] method: the activity store is a
/// process-global, so a service would be a handle it never touches. `since_ts`
/// trims the response to entries newer than the caller's high-water mark, so the
/// 1.5–2 s pollers aren't re-serializing hundreds of unchanged rows every tick.
/// All store calls run on the blocking pool: the first access loads the JSONL
/// mirror from disk, and mutations rewrite it — neither belongs on a tokio
/// worker thread.
pub async fn history(
    root: Option<String>,
    scoped: Option<bool>,
    since_ts: Option<u64>,
) -> AppResult<Vec<crate::activity::ActivityEntry>> {
    let key = if scoped.unwrap_or(false) {
        Some(crate::activity::root_key(&project_root(root)?))
    } else {
        None
    };
    crate::service::on_blocking_pool(move || {
        let mut calls: Vec<_> = crate::activity::snapshot_since(since_ts.unwrap_or(0))
            .into_iter()
            .filter(|c| c.kind == crate::activity::ActivityKind::Graph.as_str())
            .collect();
        if let Some(key) = key {
            // #104 item 5: NOT `==`. The store holds rows this project wrote
            // before the key spelling was unified, so a raw compare drops half
            // of one project's history.
            calls.retain(|c| crate::activity::root_key_eq(&c.root, &key));
        }
        calls
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::EdgeKind;

    /// The ignore editor's glob shaping: root-relative + `/`-anchored with
    /// forward slashes, trailing `/` for folders, longest root wins, and an
    /// out-of-root pick falls back to the absolute path. Built with `join` so
    /// the separators are the platform's, like a real picker result.
    #[test]
    fn to_ignore_glob_relativizes_and_anchors() {
        let root = std::env::temp_dir().join("ckg-pick-proj");
        let nested = root.join("nested");
        let roots = vec![root.clone(), nested.clone()];

        let file = root.join("src").join("a.rs");
        assert_eq!(to_ignore_glob(&file, false, &roots), "/src/a.rs");

        let dir = root.join("docs").join("gen");
        assert_eq!(to_ignore_glob(&dir, true, &roots), "/docs/gen/");

        // Under BOTH roots → the longer (nested) one wins.
        let in_nested = nested.join("x.md");
        assert_eq!(to_ignore_glob(&in_nested, false, &roots), "/x.md");

        // Outside every root → absolute fallback with forward slashes.
        let outside = std::env::temp_dir().join("ckg-pick-other").join("f.txt");
        assert_eq!(
            to_ignore_glob(&outside, false, &roots),
            outside.to_string_lossy().replace('\\', "/")
        );
    }

    /// **Previously untested.** The path trace's edge-kind filter, whose failure
    /// mode is silent: a filter the caller could not express has to widen to all
    /// three kinds, because answering `found: false` for a path that exists is
    /// indistinguishable from there being no path.
    #[test]
    fn an_unusable_kind_filter_widens_instead_of_emptying() {
        let all = vec![EdgeKind::Call, EdgeKind::Import, EdgeKind::Contains];
        assert_eq!(parse_path_kinds(None), all);
        assert_eq!(parse_path_kinds(Some(vec![])), all);
        assert_eq!(parse_path_kinds(Some(vec!["nonsense".into()])), all);
        // A usable tag is honoured, case- and whitespace-insensitively, and an
        // unrecognised one beside it is dropped rather than widening the filter
        // back out.
        assert_eq!(
            parse_path_kinds(Some(vec!["  IMPORT ".into(), "nope".into()])),
            vec![EdgeKind::Import]
        );
    }
}
