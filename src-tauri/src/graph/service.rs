//! V9-01 Phase C — the app-owned **graph service**. This is what closes the
//! gap between "the MCP tools are wired" and "the MCP tools have data": it
//! actually *builds* `<root>/<db_subdir>/graph.db` at app runtime, so the
//! self-contained MCP child (`super::mcp`) has an on-disk index to read.
//!
//! Shape mirrors [`crate::offload::OffloadService`]: constructed once in the
//! setup hook, `app.manage`d so the IPC layer can reach it, holds one warm
//! [`GraphIndex`] per project root, and runs its heavy work (the full-tree
//! walk + parse + store) off the async runtime on a dedicated thread so a
//! large repo never blocks Tauri's workers.
//!
//! What it does *not* do yet: the live fs-watcher (Phase D — incremental
//! re-index on change) and the warm loopback query path (the MCP child still
//! opens the db read-only itself). A rebuild is therefore explicit: it runs
//! once on startup for the launch root when the feature is enabled, and again
//! whenever the `graph_rebuild` IPC is invoked.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use ignore::gitignore::Gitignore;
use ignore::WalkBuilder;
use tauri::{AppHandle, Emitter};
use tracing::{debug, info, warn};

use crate::error::AppResult;
use crate::settings::{GraphSettings, SettingsHandle};

use super::embed::Embedder;
use super::index::{GraphIndex, GraphStats, LangCount};
use super::model::Lang;
use super::parse_file;

/// Bumped if the embedding schema/layout changes in a way that invalidates
/// stored vectors. Part of the epoch fingerprint alongside model + dim.
const EMBED_SCHEMA: &str = "v1";

/// Tauri event carrying a [`GraphStatus`] snapshot whenever a root's build
/// state changes (queued → building → ready/error). The Phase-I monitor tab
/// subscribes to this; for now it's also handy for debugging.
pub const GRAPH_STATUS_EVENT: &str = "graph-status";

/// One project root's live indexing state, serialized to the frontend.
#[derive(Clone, Debug, serde::Serialize)]
pub struct GraphStatus {
    /// Absolute project root, as a display string.
    pub root: String,
    /// Lifecycle: `idle` (never built), `building`, `ready`, or `error`.
    pub state: String,
    /// Whether a build is in flight right now.
    pub building: bool,
    /// Files visited by the last full walk (after ignore/lang/size filtering).
    pub files_indexed: u64,
    /// Stored row counts from the last successful build.
    pub files: u64,
    pub symbols: u64,
    pub edges: u64,
    /// Indexed files grouped by language, biggest first (languages with zero
    /// files are omitted). Drives the monitor tab's per-language table.
    pub langs: Vec<LangCount>,
    /// Last build error, if the most recent attempt failed.
    pub last_error: Option<String>,
    /// Whether file-watch re-indexing is currently paused (a global toggle,
    /// mirrored into every status so the monitor UI can render the right
    /// button label without a separate query).
    pub watch_paused: bool,

    // ── Semantic search (Phase G) ──
    /// Whether semantic search is enabled in settings.
    pub semantic_enabled: bool,
    /// Whether an embedder is configured (an endpoint is set).
    pub embedder_configured: bool,
    /// Whether the last embedder probe/batch succeeded (live reachability).
    pub embedder_ready: bool,
    /// Embedding state: `off` | `idle` | `embedding` | `degraded` | `error`.
    pub embed_state: String,
    /// Vectors stored for the current epoch.
    pub embedded: u64,
    /// Total doc chunks (the embedding denominator).
    pub embed_total: u64,
    /// Chunks still awaiting a current-epoch vector.
    pub embed_pending: u64,
    /// Last embedder error, if any.
    pub embed_error: Option<String>,
}

impl GraphStatus {
    fn idle(root: &Path) -> Self {
        GraphStatus {
            root: root.display().to_string(),
            state: "idle".into(),
            building: false,
            files_indexed: 0,
            files: 0,
            symbols: 0,
            edges: 0,
            langs: Vec::new(),
            last_error: None,
            watch_paused: false,
            semantic_enabled: false,
            embedder_configured: false,
            embedder_ready: false,
            embed_state: "off".into(),
            embedded: 0,
            embed_total: 0,
            embed_pending: 0,
            embed_error: None,
        }
    }
}

/// Result of an on-demand embedder reachability probe (the monitor tab's
/// "Test connection" action). Lets the user see whether the embedding endpoint
/// answers — and the exact error if not — without running a full backfill.
#[derive(Clone, Debug, serde::Serialize)]
pub struct EmbedderProbe {
    /// Whether the endpoint answered with a usable embedding.
    pub ok: bool,
    /// The live vector dimension the endpoint returned (on success).
    pub dim: Option<usize>,
    /// Human-readable status / error message for display.
    pub message: String,
}

/// The app-owned graph service. Held in `AppState` beside the offload service.
pub struct GraphService {
    app: AppHandle,
    settings: SettingsHandle,
    /// Warm index handle per project root (one SQLite connection each), opened
    /// lazily on first build/status and reused.
    indices: StdMutex<HashMap<PathBuf, Arc<GraphIndex>>>,
    /// Per-root build status, the source of truth for the IPC + the event.
    status: StdMutex<HashMap<PathBuf, GraphStatus>>,
    /// Live fs-watcher handle per watched root (Phase D). Kept alive here so
    /// the OS watch (and its debounce thread) persist; dropped on shutdown.
    watchers: StdMutex<HashMap<PathBuf, notify::RecommendedWatcher>>,
    /// Serializes all store mutations (full rebuild vs incremental re-index)
    /// so a watcher batch can't write into a store that a concurrent rebuild
    /// is mid-`reset()`. Coarse (one lock for all roots), which is fine —
    /// builds/re-indexes are infrequent and a project is usually one root.
    write_lock: StdMutex<()>,
    /// When set, the watcher drops incremental re-index batches (the OS watch
    /// keeps running; events are simply ignored). Drives `graph_set_watch_paused`.
    paused: AtomicBool,
    /// Single-flight guard for the embedding backfill, per root. Without it,
    /// overlapping rebuilds/watcher batches each spawn a backfill task that
    /// races on the same pending set and duplicates embedding-endpoint calls.
    /// `again` records that a request arrived while a backfill was running, so
    /// the in-flight task does one more pass and no late chunk is missed.
    backfill: StdMutex<HashMap<PathBuf, BackfillFlag>>,
}

/// Per-root backfill liveness for the single-flight guard in [`spawn_backfill`].
#[derive(Default)]
struct BackfillFlag {
    running: bool,
    again: bool,
}

/// RAII reset for the [`GraphService::spawn_backfill`] single-flight guard.
/// Clearing `running` on `Drop` covers ABNORMAL termination only — e.g. the
/// async runtime drops the backfill future before it finishes (app shutdown) —
/// so the root isn't left pinned with `running = true`, which would silently
/// disable embedding for the service's lifetime. The NORMAL exit path clears
/// `running` itself, under the same lock as the final `again` check (so a
/// late-arriving request can't be orphaned in the gap), and sets `clean` to
/// suppress this Drop — otherwise it could clobber the `running = true` of a
/// fresh task that started in the window between break and Drop.
struct BackfillGuard {
    svc: Arc<GraphService>,
    root: PathBuf,
    clean: bool,
}

impl Drop for BackfillGuard {
    fn drop(&mut self) {
        if self.clean {
            return;
        }
        if let Ok(mut g) = self.svc.backfill.lock() {
            if let Some(st) = g.get_mut(&self.root) {
                st.running = false;
            }
        }
    }
}

impl GraphService {
    pub fn new(app: AppHandle, settings: SettingsHandle) -> Arc<Self> {
        Arc::new(Self {
            app,
            settings,
            indices: StdMutex::new(HashMap::new()),
            status: StdMutex::new(HashMap::new()),
            watchers: StdMutex::new(HashMap::new()),
            write_lock: StdMutex::new(()),
            paused: AtomicBool::new(false),
            backfill: StdMutex::new(HashMap::new()),
        })
    }

    /// Pause/resume incremental watcher re-indexing. Paused = changes are
    /// ignored until resumed (a manual rebuild still works). Returns the new
    /// state.
    pub fn set_watch_paused(&self, paused: bool) -> bool {
        self.paused.store(paused, Ordering::Relaxed);
        paused
    }

    /// Force a fresh re-embed of all doc chunks (drops the vector store first),
    /// then backfill. Used by the "Rebuild embeddings" action — the recovery
    /// path for a silent model swap behind the same name. No-op when semantic
    /// search is off.
    pub fn spawn_rebuild_embeddings(self: &Arc<Self>, root: PathBuf) {
        if !self.settings.current().graph.semantic_search {
            return;
        }
        if let Ok(idx) = self.index_for(&root) {
            let cleared = {
                let _w = self.write_lock.lock().unwrap();
                idx.clear_vectors()
            };
            // Don't silently no-op: if the clear failed (e.g. the store was
            // briefly locked), the old vectors survive under the same epoch and
            // `chunks_needing_vectors` would find nothing to do — leaving stale
            // embeddings serving forever while the UI reads "100%". Surface it.
            if let Err(e) = cleared {
                self.patch_status(&root, |s| {
                    s.embed_state = "error".into();
                    s.embed_error = Some(format!("failed to clear vectors: {e}"));
                });
                return;
            }
        }
        self.spawn_backfill(root);
    }

    /// Probe the configured embedding endpoint without running a backfill — the
    /// monitor tab's "Test connection" action. Returns reachability + the live
    /// vector dimension, or a human-readable error (connection refused, timeout,
    /// HTTP status, decode failure), so the user can diagnose the endpoint
    /// before kicking off a full embed.
    pub async fn test_embedder(&self) -> EmbedderProbe {
        let snap = self.settings.current().graph;
        if !snap.semantic_search {
            return EmbedderProbe {
                ok: false,
                dim: None,
                message: "Semantic search is off — enable it in Settings → Code graph.".into(),
            };
        }
        let Some(embedder) = Embedder::new(&snap.embedding_endpoint, &snap.embedding_model) else {
            return EmbedderProbe {
                ok: false,
                dim: None,
                message: "No embedding endpoint configured.".into(),
            };
        };
        match embedder.probe_dim().await {
            Ok(dim) => EmbedderProbe {
                ok: true,
                dim: Some(dim),
                message: format!("Reachable — {dim}-dim embeddings."),
            },
            Err(e) => EmbedderProbe {
                ok: false,
                dim: None,
                message: e,
            },
        }
    }

    /// Run a `graph_*` tool against this project's WARM index — the single
    /// shared connection the indexer already owns — so cloud Claude's queries
    /// don't open a second (cross-process) handle on the SQLite-backed store.
    /// Resolves the project root from the caller's `cwd` (the same ancestor walk
    /// the MCP child uses) and records the call in the monitor's activity ring.
    /// Backs the loopback `/graph_run` route.
    pub async fn run_graph_tool(
        &self,
        cwd: &Path,
        name: &str,
        args: &serde_json::Value,
    ) -> Result<String, String> {
        let settings = self.settings.current();
        let sub = settings.graph.effective_db_subdir();
        let root = super::mcp::find_graph_root(cwd, &sub)
            .ok_or_else(|| format!("no code graph found from {}", cwd.display()))?;
        let idx = self.index_for(&root).map_err(|e| e.to_string())?;
        super::mcp::dispatch_recorded(&root, &idx, &settings, "claude", name, args).await
    }

    /// The configured per-project db subdirectory (default `.cimp`).
    fn db_subdir(&self) -> String {
        self.settings.current().graph.effective_db_subdir()
    }

    /// Get (opening + caching if needed) the warm index for `root`.
    ///
    /// The `indices` lock is held across the whole check-open-insert so two
    /// concurrent first-callers for the same root can't both `open` and race to
    /// insert (the loser would otherwise keep a live handle backed by a
    /// connection no longer in the cache — split writes). Opens are infrequent
    /// and this lock guards nothing on the hot query path, so holding it across
    /// the open is cheap.
    fn index_for(&self, root: &Path) -> AppResult<Arc<GraphIndex>> {
        let root = root.to_path_buf();
        let mut guard = self.indices.lock().unwrap();
        if let Some(idx) = guard.get(&root).cloned() {
            return Ok(idx);
        }
        let idx = Arc::new(GraphIndex::open(&root, &self.db_subdir())?);
        guard.insert(root, idx.clone());
        Ok(idx)
    }

    /// Every known root's status (the IPC list surface).
    pub fn statuses(&self) -> Vec<GraphStatus> {
        let paused = self.paused.load(Ordering::Relaxed);
        self.status
            .lock()
            .unwrap()
            .values()
            .cloned()
            .map(|mut s| {
                s.watch_paused = paused;
                s
            })
            .collect()
    }

    /// Kick a non-blocking full rebuild of `root` on a dedicated thread. Returns
    /// immediately; progress lands on the `graph-status` event and via
    /// [`status`](Self::status). A no-op (logged) when a build for this root is
    /// already in flight.
    pub fn spawn_rebuild(self: &Arc<Self>, root: PathBuf) {
        {
            let mut guard = self.status.lock().unwrap();
            if let Some(s) = guard.get(&root) {
                if s.building {
                    debug!(root = %root.display(), "graph: rebuild already in flight — skipping");
                    return;
                }
            }
            let mut building = guard
                .get(&root)
                .cloned()
                .unwrap_or_else(|| GraphStatus::idle(&root));
            building.state = "building".into();
            building.building = true;
            building.last_error = None;
            guard.insert(root.clone(), building.clone());
            building.watch_paused = self.paused.load(Ordering::Relaxed);
            let _ = self.app.emit(GRAPH_STATUS_EVENT, &building);
        }

        let this = self.clone();
        let thread_root = root.clone();
        let spawned = std::thread::Builder::new()
            .name("cimp-graph-index".into())
            .spawn(move || {
                let root = thread_root;
                let started = std::time::Instant::now();
                match this.rebuild_blocking(&root) {
                    Ok(stats) => {
                        info!(
                            root = %root.display(),
                            files = stats.files,
                            symbols = stats.symbols,
                            edges = stats.edges,
                            ms = started.elapsed().as_millis() as u64,
                            "graph: rebuild complete"
                        );
                        this.patch_status(&root, |s| {
                            s.state = "ready".into();
                            s.building = false;
                            s.files = stats.files;
                            s.symbols = stats.symbols;
                            s.edges = stats.edges;
                            s.langs = stats.by_lang.clone();
                            s.last_error = None;
                        });
                        // Phase G: embed any new/changed doc chunks (no-op when
                        // semantic search is off).
                        this.spawn_backfill(root.clone());
                    }
                    Err(e) => {
                        warn!(root = %root.display(), error = %e, "graph: rebuild failed");
                        this.patch_status(&root, |s| {
                            s.state = "error".into();
                            s.building = false;
                            s.last_error = Some(e.to_string());
                        });
                    }
                }
            });

        // If the OS refuses the thread, don't leave the root pinned at
        // `building=true` forever (the in-flight guard above would then skip
        // every future rebuild). Roll the status back to `error`.
        if let Err(e) = spawned {
            warn!(root = %root.display(), error = %e, "graph: failed to spawn index thread");
            self.patch_status(&root, |s| {
                s.state = "error".into();
                s.building = false;
                s.last_error = Some(format!("failed to spawn index thread: {e}"));
            });
        }
    }

    /// Synchronous full rebuild: reset the store, walk the tree, parse every
    /// supported file, write each file's graph, and record the visited-file
    /// count. Returns the final stored counts. [`spawn_rebuild`](Self::spawn_rebuild)
    /// wraps this with status bookkeeping on a worker thread; the build itself
    /// lives in the free [`build_tree`] fn so it's testable without the app.
    pub fn rebuild_blocking(&self, root: &Path) -> AppResult<GraphStats> {
        let snap = self.settings.current().graph;
        let idx = self.index_for(root)?;
        // Hold the store-write lock across the whole rebuild so a concurrent
        // watcher batch can't write into the store mid-`reset()`.
        let _w = self.write_lock.lock().unwrap();
        let (indexed, stats) = build_tree(&idx, root, &snap, &self.db_subdir())?;

        // Record the visited-file count alongside the authoritative row counts.
        self.patch_status(root, |s| s.files_indexed = indexed);
        Ok(stats)
    }

    /// Start the Phase-D fs-watcher for `root` (idempotent; a no-op if already
    /// watching or the feature is disabled). Incremental re-indexes flow
    /// through [`reindex_paths`](Self::reindex_paths). Independent of the
    /// initial build — they run in parallel and the `write_lock` serializes
    /// their store writes.
    pub fn start_watch(self: &Arc<Self>, root: PathBuf) {
        if !self.settings.current().graph.enabled {
            return;
        }
        let mut watchers = self.watchers.lock().unwrap();
        if watchers.contains_key(&root) {
            return;
        }
        let debounce =
            Duration::from_millis(self.settings.current().graph.watch_debounce_ms.max(50));
        match super::watcher::start(self.clone(), root.clone(), debounce) {
            Ok(handle) => {
                info!(root = %root.display(), "graph: watching for changes");
                watchers.insert(root, handle);
            }
            Err(e) => warn!(root = %root.display(), error = %e, "graph: watcher failed to start"),
        }
    }

    /// Apply one debounced batch of changed paths to `root`'s store: re-parse
    /// created/modified files, drop rows for deleted ones, then refresh the
    /// status counts. Called from the watcher thread.
    pub fn reindex_paths(self: &Arc<Self>, root: &Path, paths: Vec<PathBuf>) {
        let snap = self.settings.current().graph;
        if !snap.enabled || self.paused.load(Ordering::Relaxed) {
            return;
        }
        let idx = match self.index_for(root) {
            Ok(i) => i,
            Err(e) => {
                warn!(root = %root.display(), error = %e, "graph: reindex open failed");
                return;
            }
        };
        let sub = self.db_subdir();
        let max_bytes = snap.max_file_bytes.max(1);
        let gi = build_gitignore(root, &paths);

        let _w = self.write_lock.lock().unwrap();
        let mut changed = 0u64;
        for path in paths {
            // Never touch our own store directory.
            if path.components().any(|c| c.as_os_str() == sub.as_str()) {
                continue;
            }
            // Only files with a configured language matter (this also filters
            // out directory events, whose path has no indexable extension).
            let Some(lang) = lang_for(&path, &snap.languages) else {
                continue;
            };
            if lang == Lang::Markdown && !snap.index_docs {
                continue;
            }
            let rel = rel_path(root, &path);

            if !path.is_file() {
                // Deleted/moved-away — drop its rows (no-op if never indexed).
                if idx.remove_file(&rel).is_ok() {
                    changed += 1;
                }
                continue;
            }
            // Respect gitignore so editing a build artifact doesn't churn.
            if gi.matched_path_or_any_parents(&path, false).is_ignore() {
                continue;
            }
            match std::fs::metadata(&path) {
                Ok(m) if m.len() > max_bytes => continue,
                Ok(_) => {}
                Err(_) => continue,
            }
            let src = match std::fs::read_to_string(&path) {
                Ok(s) => s,
                Err(_) => continue,
            };
            let fg = parse_file(&rel, &src, lang);
            match idx.index_file_graph(&fg) {
                Ok(()) => changed += 1,
                Err(e) => debug!(file = %rel, error = %e, "graph: incremental index failed"),
            }
        }
        if changed > 0 {
            // Drop vectors for chunks this batch deleted or re-anchored.
            let _ = idx.prune_orphan_vectors();
        }
        drop(_w);

        if changed > 0 {
            if let Ok(stats) = idx.stats() {
                self.patch_status(root, |s| {
                    // A full rebuild may be in flight concurrently; don't stomp
                    // its `building` state to `ready` — just refresh the counts
                    // and let the rebuild own the final transition.
                    if !s.building {
                        s.state = "ready".into();
                    }
                    s.files = stats.files;
                    s.symbols = stats.symbols;
                    s.edges = stats.edges;
                    s.langs = stats.by_lang.clone();
                });
            }
            debug!(root = %root.display(), changed, "graph: incremental re-index applied");
            // Phase G: embed the new/changed doc chunks (no-op when off).
            self.spawn_backfill(root.to_path_buf());
        }
    }

    /// Kick a background embedding backfill for `root` (Phase G). No-op unless
    /// semantic search is enabled. Spawned on the async runtime (the embed
    /// calls are network I/O); safe to call after every build/reindex — it
    /// only embeds chunks that are new or changed since the last pass.
    pub fn spawn_backfill(self: &Arc<Self>, root: PathBuf) {
        if !self.settings.current().graph.semantic_search {
            return;
        }
        // Single-flight: if a backfill for this root is already running, just
        // mark that another pass is wanted and let the in-flight task pick up
        // the new chunks — don't spawn a racing duplicate.
        {
            let mut g = self.backfill.lock().unwrap();
            let st = g.entry(root.clone()).or_default();
            if st.running {
                st.again = true;
                return;
            }
            st.running = true;
        }
        let this = self.clone();
        tauri::async_runtime::spawn(async move {
            // The guard clears `running` only if this future is dropped mid-pass
            // (shutdown). The normal path below clears it under the lock.
            let mut guard = BackfillGuard {
                svc: this.clone(),
                root: root.clone(),
                clean: false,
            };
            loop {
                this.embed_backfill(&root).await;
                let mut g = this.backfill.lock().unwrap();
                let st = g.entry(root.clone()).or_default();
                if st.again {
                    st.again = false; // consume the request and loop once more
                    continue;
                }
                // No further request: clear `running` and the `again` check in
                // the SAME locked section so a request arriving after this can't
                // be lost. Mark the guard clean so its Drop won't later clobber a
                // fresh task that may start once we release the lock.
                st.running = false;
                guard.clean = true;
                break;
            }
        });
    }

    /// Run a store mutation under `write_lock` on a blocking thread, returning
    /// its result. An `async` caller must use this instead of locking
    /// `write_lock` directly: a full rebuild can hold the lock for many seconds,
    /// and blocking a Tokio worker on it would starve every other async IPC
    /// handler. `spawn_blocking` moves both the wait and the DB write off the
    /// async worker.
    async fn locked_write<T, F>(self: &Arc<Self>, f: F) -> T
    where
        F: FnOnce() -> T + Send + 'static,
        T: Send + 'static,
    {
        let this = self.clone();
        tauri::async_runtime::spawn_blocking(move || {
            let _w = this.write_lock.lock().unwrap();
            f()
        })
        .await
        .expect("graph write-lock task panicked")
    }

    /// Embed any doc chunks missing a current-epoch vector and store them.
    /// Drives the embedding status fields. Degrades cleanly: an unconfigured or
    /// unreachable embedder leaves chunks queryable via full-text (the
    /// structural graph never depends on this).
    async fn embed_backfill(self: &Arc<Self>, root: &Path) {
        let snap = self.settings.current().graph;
        if !snap.semantic_search {
            return;
        }
        let configured = !snap.embedding_endpoint.trim().is_empty();
        self.patch_status(root, |s| {
            s.semantic_enabled = true;
            s.embedder_configured = configured;
        });
        let Some(embedder) = Embedder::new(&snap.embedding_endpoint, &snap.embedding_model) else {
            self.patch_status(root, |s| {
                s.embed_state = "degraded".into();
                s.embedder_ready = false;
                s.embed_error = Some("no embedding endpoint configured".into());
            });
            return;
        };

        // Resolve the vector dimension: the configured one, else probe live.
        let dim = if snap.embedding_dims > 0 {
            snap.embedding_dims as usize
        } else {
            match embedder.probe_dim().await {
                Ok(d) => d,
                Err(e) => {
                    self.patch_status(root, |s| {
                        s.embed_state = "degraded".into();
                        s.embedder_ready = false;
                        s.embed_error = Some(e);
                    });
                    return;
                }
            }
        };

        let idx = match self.index_for(root) {
            Ok(i) => i,
            Err(e) => {
                self.patch_status(root, |s| {
                    s.embed_state = "error".into();
                    s.embed_error = Some(e.to_string());
                });
                return;
            }
        };
        let epoch = embedding_epoch(&snap.embedding_model, dim);
        {
            let idx = idx.clone();
            let model = snap.embedding_model.clone();
            let epoch = epoch.clone();
            if let Err(e) = self
                .locked_write(move || idx.ensure_vector_store(dim, &model, &epoch))
                .await
            {
                self.patch_status(root, |s| {
                    s.embed_state = "error".into();
                    s.embed_error = Some(e.to_string());
                });
                return;
            }
        }

        self.patch_status(root, |s| {
            s.embed_state = "embedding".into();
            s.embed_error = None;
        });

        let batch = snap.embedding_batch.clamp(1, 256);
        loop {
            let pending = match idx.chunks_needing_vectors(&epoch, batch) {
                Ok(p) => p,
                Err(e) => {
                    self.patch_status(root, |s| {
                        s.embed_state = "error".into();
                        s.embed_error = Some(e.to_string());
                    });
                    return;
                }
            };
            if pending.is_empty() {
                break;
            }
            let texts: Vec<String> = pending.iter().map(|(_, _, t)| t.clone()).collect();
            match embedder.embed(&texts).await {
                Ok(vectors) if vectors.len() == pending.len() => {
                    let rows: Vec<(String, String, Vec<f32>)> = pending
                        .into_iter()
                        .zip(vectors)
                        .map(|((id, hash, _), v)| (id, hash, v))
                        .collect();
                    let put = {
                        let idx = idx.clone();
                        let epoch = epoch.clone();
                        self.locked_write(move || idx.put_doc_vectors(&epoch, &rows))
                            .await
                    };
                    if let Err(e) = put {
                        self.patch_status(root, |s| {
                            s.embed_state = "error".into();
                            s.embed_error = Some(e.to_string());
                        });
                        return;
                    }
                    self.refresh_embed_coverage(root, &idx, &epoch, true);
                }
                Ok(_) => {
                    self.patch_status(root, |s| {
                        s.embed_state = "degraded".into();
                        s.embedder_ready = false;
                        s.embed_error = Some("embedding count mismatch".into());
                    });
                    return;
                }
                Err(e) => {
                    // Endpoint went away mid-backfill — degrade, keep what we have.
                    self.patch_status(root, |s| {
                        s.embed_state = "degraded".into();
                        s.embedder_ready = false;
                        s.embed_error = Some(e);
                    });
                    return;
                }
            }
        }

        // A rebuild can delete doc chunks while an embed request is in flight
        // (the write lock is released across the await). Drop any vectors that
        // no longer have a chunk so coverage stays accurate.
        {
            let idx = idx.clone();
            self.locked_write(move || {
                let _ = idx.prune_orphan_vectors();
            })
            .await;
        }
        self.refresh_embed_coverage(root, &idx, &epoch, true);
        self.patch_status(root, |s| {
            s.embed_state = "idle".into();
            s.embedder_ready = true;
            s.embed_error = None;
        });
    }

    /// Recompute `(embedded, total, pending)` for the status from the store.
    fn refresh_embed_coverage(&self, root: &Path, idx: &GraphIndex, epoch: &str, ready: bool) {
        let (embedded, total) = idx.embedding_coverage(epoch).unwrap_or((0, 0));
        self.patch_status(root, |s| {
            s.embedded = embedded;
            s.embed_total = total;
            s.embed_pending = total.saturating_sub(embedded);
            s.embedder_ready = ready;
        });
    }

    /// Apply a mutation to a root's status and emit the change event.
    fn patch_status(&self, root: &Path, f: impl FnOnce(&mut GraphStatus)) {
        let status = {
            let mut guard = self.status.lock().unwrap();
            let s = guard
                .entry(root.to_path_buf())
                .or_insert_with(|| GraphStatus::idle(root));
            f(s);
            s.clone()
        };
        let mut status = status;
        status.watch_paused = self.paused.load(Ordering::Relaxed);
        let _ = self.app.emit(GRAPH_STATUS_EVENT, &status);
    }

    /// Drop warm handles + watchers on shutdown (SQLite connections close on
    /// drop; dropping a watcher ends its debounce thread).
    pub fn shutdown(&self) {
        self.watchers.lock().unwrap().clear();
        self.indices.lock().unwrap().clear();
    }
}

/// Reset `idx` and re-index every supported file under `root`, honoring
/// gitignore (+ global/exclude) and the configured language/size filters.
/// Returns `(files_visited, final_stats)`. Free function (no `self`) so the
/// build is unit-testable against a bare [`GraphIndex`].
fn build_tree(
    idx: &GraphIndex,
    root: &Path,
    snap: &GraphSettings,
    db_subdir: &str,
) -> AppResult<(u64, GraphStats)> {
    // A full rebuild starts clean so deleted files don't leave stale rows.
    idx.reset()?;

    let max_bytes = snap.max_file_bytes.max(1);

    let mut indexed: u64 = 0;
    let mut wb = WalkBuilder::new(root);
    wb.hidden(false) // index dotfiles like `.github/*.md`; the db dir is filtered below
        .git_ignore(true)
        .git_global(true)
        .git_exclude(true)
        .parents(true);
    // Honor the user's extra ignore globs (additive to `.gitignore`). An
    // `Override` whose patterns are *ignore* globs needs each prefixed with
    // `!` (overrides are whitelists; a leading `!` flips one to a blacklist).
    if !snap.ignore.is_empty() {
        let mut ob = ignore::overrides::OverrideBuilder::new(root);
        for pat in &snap.ignore {
            let pat = pat.trim();
            if pat.is_empty() {
                continue;
            }
            let rule = if let Some(stripped) = pat.strip_prefix('!') {
                stripped.to_string() // already a re-include
            } else {
                format!("!{pat}") // ignore this glob
            };
            let _ = ob.add(&rule);
        }
        if let Ok(ov) = ob.build() {
            wb.overrides(ov);
        }
    }
    let walker = wb.build();

    for entry in walker {
        let entry = match entry {
            Ok(e) => e,
            Err(e) => {
                debug!(error = %e, "graph: walk entry error (skipped)");
                continue;
            }
        };
        if entry.file_type().map(|t| !t.is_file()).unwrap_or(true) {
            continue;
        }
        let path = entry.path();

        // Never index our own store directory.
        if path.components().any(|c| c.as_os_str() == db_subdir) {
            continue;
        }

        let Some(lang) = lang_for(path, &snap.languages) else {
            continue;
        };
        // `index_docs` off → skip pure-doc (markdown) files; code doc-comments
        // still ride along with their symbols.
        if lang == Lang::Markdown && !snap.index_docs {
            continue;
        }

        // Size guard before reading.
        match entry.metadata() {
            Ok(m) if m.len() > max_bytes => continue,
            Ok(_) => {}
            Err(_) => continue,
        }

        let src = match std::fs::read_to_string(path) {
            Ok(s) => s,
            Err(_) => continue, // binary / non-UTF-8 / unreadable — skip
        };

        let rel = rel_path(root, path);
        let fg = parse_file(&rel, &src, lang);
        if let Err(e) = idx.index_file_graph(&fg) {
            warn!(file = %rel, error = %e, "graph: index_file_graph failed (skipped)");
            continue;
        }
        indexed += 1;
    }

    // `reset()` deliberately keeps the vector store (so unchanged chunks aren't
    // needlessly re-embedded), so vectors for files that vanished since the
    // last build are now orphans — drop them before reporting stats.
    let _ = idx.prune_orphan_vectors();

    Ok((indexed, idx.stats()?))
}

/// Project-relative path with forward slashes, matching what the parser stores
/// and the MCP tools query against. Falls back to the file name if `path`
/// isn't under `root` (shouldn't happen for a walk rooted at `root`).
fn rel_path(root: &Path, path: &Path) -> String {
    let rel = path.strip_prefix(root).unwrap_or(path);
    rel.to_string_lossy().replace('\\', "/")
}

/// The embedding "epoch" fingerprint — a vector is only comparable to others
/// sharing its `{model, dim, schema}`. A change to any of these bumps the
/// epoch, scoping k-NN to matching vectors and triggering a background
/// re-embed. Kept short and human-glanceable (model + dim + a schema tag).
fn embedding_epoch(model: &str, dim: usize) -> String {
    let m = model.trim();
    let m = if m.is_empty() { "default" } else { m };
    format!("{m}|{dim}|{EMBED_SCHEMA}")
}

/// The indexable language for `path`, or `None` if its extension is unknown or
/// not in the configured `languages`. Shared by the full walk and the watcher
/// so they agree on what's in scope.
fn lang_for(path: &Path, languages: &[String]) -> Option<Lang> {
    let lang = Lang::from_path(path);
    if lang == Lang::Other || !languages.iter().any(|l| l == lang.tag()) {
        None
    } else {
        Some(lang)
    }
}

/// Build a gitignore matcher for per-path filtering in the watcher (the full
/// walk gets this for free via `WalkBuilder`). Merges every `.gitignore` from
/// `root` down to each changed path's directory so the watcher agrees with the
/// full walk on nested ignores — a subdirectory `.gitignore` (e.g.
/// `src/gen/.gitignore`) is honored, not just the root one. Only the dirs
/// touched by this batch are scanned, so it stays cheap. An empty matcher
/// (missing/invalid files) simply ignores nothing.
fn build_gitignore(root: &Path, paths: &[PathBuf]) -> Gitignore {
    let mut b = ignore::gitignore::GitignoreBuilder::new(root);
    let mut dirs: std::collections::HashSet<PathBuf> = std::collections::HashSet::new();
    dirs.insert(root.to_path_buf());
    for p in paths {
        for anc in p.ancestors() {
            if !anc.starts_with(root) {
                break;
            }
            if anc.is_dir() {
                dirs.insert(anc.to_path_buf());
            }
            if anc == root {
                break;
            }
        }
    }
    for dir in dirs {
        let gi = dir.join(".gitignore");
        if gi.is_file() {
            let _ = b.add(gi);
        }
    }
    b.build().unwrap_or_else(|_| Gitignore::empty())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::GraphSettings;

    /// A full rebuild over a tiny on-disk Rust project: the store ends up with
    /// the file's symbols, deleted files don't survive a second build, and the
    /// db dir itself is never indexed. Drives the free `build_tree` core
    /// directly, so no `AppHandle`/`SettingsHandle` is needed.
    #[test]
    fn rebuild_indexes_tree_and_prunes_deleted() {
        let dir = std::env::temp_dir().join(format!("ckg-svc-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(
            dir.join("src/lib.rs"),
            "/// Doc.\npub fn alpha() -> i32 { beta() }\nfn beta() -> i32 { 1 }\n",
        )
        .unwrap();
        std::fs::write(dir.join("src/extra.rs"), "pub fn gamma() {}\n").unwrap();

        // Distinct subdir so the test never touches a real `.cimp`.
        let sub = ".ckg-test";
        let snap = GraphSettings::default();
        let idx = GraphIndex::open(&dir, sub).expect("open");

        let (visited, stats) = build_tree(&idx, &dir, &snap, sub).expect("rebuild");
        assert_eq!(visited, 2);
        assert!(stats.symbols >= 3, "alpha/beta/gamma at least: {stats:?}");
        assert_eq!(stats.files, 2);

        // The index can answer a lookup against the freshly built store, and
        // the db dir itself was excluded (only the 2 source files counted).
        assert!(idx
            .find_symbol("alpha")
            .unwrap()
            .iter()
            .any(|s| s.name == "alpha"));

        // Delete one file and rebuild: its rows must be gone (reset prunes).
        std::fs::remove_file(dir.join("src/extra.rs")).unwrap();
        let (_, stats2) = build_tree(&idx, &dir, &snap, sub).expect("rebuild2");
        assert_eq!(stats2.files, 1);
        assert!(idx.find_symbol("gamma").unwrap().is_empty());

        drop(idx);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn rebuild_indexes_markdown_docs_and_honors_index_docs_toggle() {
        let dir = std::env::temp_dir().join(format!("ckg-md-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(dir.join("docs")).unwrap();
        std::fs::write(dir.join("src.rs"), "pub fn f() {}\n").unwrap();
        std::fs::write(
            dir.join("docs/guide.md"),
            "# Guide\n\nHow to configure the widget frobnicator.\n",
        )
        .unwrap();
        let sub = ".ckg-test";

        // index_docs on (default): the markdown chunk is searchable.
        let snap_on = GraphSettings::default();
        let idx = GraphIndex::open(&dir, sub).expect("open");
        build_tree(&idx, &dir, &snap_on, sub).expect("rebuild");
        let hits = idx.search_docs("frobnicator", 10, 200).expect("search");
        assert!(hits.iter().any(|h| h.source_path == "docs/guide.md"));

        // index_docs off: markdown is skipped (the file row is gone after a
        // clean rebuild), so the doc search no longer matches.
        let mut snap_off = GraphSettings::default();
        snap_off.index_docs = false;
        build_tree(&idx, &dir, &snap_off, sub).expect("rebuild2");
        assert!(idx.search_docs("frobnicator", 10, 200).unwrap().is_empty());

        drop(idx);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn lang_for_honors_configured_languages() {
        use std::path::PathBuf;
        let all = GraphSettings::default().languages;
        // A configured language resolves; an unknown extension doesn't.
        assert_eq!(lang_for(&PathBuf::from("src/a.rs"), &all), Some(Lang::Rust));
        assert_eq!(lang_for(&PathBuf::from("a.bin"), &all), None);
        // A recognized language that the user didn't opt into is filtered out.
        let only_rust = vec!["rust".to_string()];
        assert_eq!(lang_for(&PathBuf::from("a.py"), &only_rust), None);
        assert_eq!(lang_for(&PathBuf::from("a.rs"), &only_rust), Some(Lang::Rust));
    }
}
