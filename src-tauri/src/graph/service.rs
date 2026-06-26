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
use std::sync::{Arc, Mutex as StdMutex};

use ignore::WalkBuilder;
use tauri::{AppHandle, Emitter};
use tracing::{debug, info, warn};

use crate::error::AppResult;
use crate::settings::{GraphSettings, SettingsHandle};

use super::index::{GraphIndex, GraphStats};
use super::model::Lang;
use super::parse_file;

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
    /// Last build error, if the most recent attempt failed.
    pub last_error: Option<String>,
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
            last_error: None,
        }
    }
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
}

impl GraphService {
    pub fn new(app: AppHandle, settings: SettingsHandle) -> Arc<Self> {
        Arc::new(Self {
            app,
            settings,
            indices: StdMutex::new(HashMap::new()),
            status: StdMutex::new(HashMap::new()),
        })
    }

    /// The configured per-project db subdirectory (default `.ccimp`).
    fn db_subdir(&self) -> String {
        let s = self.settings.current().graph.db_subdir;
        if s.trim().is_empty() {
            ".ccimp".to_string()
        } else {
            s
        }
    }

    /// Get (opening + caching if needed) the warm index for `root`.
    fn index_for(&self, root: &Path) -> AppResult<Arc<GraphIndex>> {
        let root = root.to_path_buf();
        if let Some(idx) = self.indices.lock().unwrap().get(&root).cloned() {
            return Ok(idx);
        }
        let idx = Arc::new(GraphIndex::open(&root, &self.db_subdir())?);
        self.indices
            .lock()
            .unwrap()
            .insert(root, idx.clone());
        Ok(idx)
    }

    /// Status snapshot for `root` (idle if never built/known).
    pub fn status(&self, root: &Path) -> GraphStatus {
        self.status
            .lock()
            .unwrap()
            .get(root)
            .cloned()
            .unwrap_or_else(|| GraphStatus::idle(root))
    }

    /// Every known root's status (the IPC list surface).
    pub fn statuses(&self) -> Vec<GraphStatus> {
        self.status.lock().unwrap().values().cloned().collect()
    }

    fn set_status(&self, root: &Path, status: GraphStatus) {
        self.status
            .lock()
            .unwrap()
            .insert(root.to_path_buf(), status.clone());
        let _ = self.app.emit(GRAPH_STATUS_EVENT, &status);
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
            let _ = self.app.emit(GRAPH_STATUS_EVENT, &building);
        }

        let this = self.clone();
        std::thread::Builder::new()
            .name("ccimp-graph-index".into())
            .spawn(move || {
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
                        let mut s = this.status(&root);
                        s.state = "ready".into();
                        s.building = false;
                        s.files = stats.files;
                        s.symbols = stats.symbols;
                        s.edges = stats.edges;
                        s.last_error = None;
                        this.set_status(&root, s);
                    }
                    Err(e) => {
                        warn!(root = %root.display(), error = %e, "graph: rebuild failed");
                        let mut s = this.status(&root);
                        s.state = "error".into();
                        s.building = false;
                        s.last_error = Some(e.to_string());
                        this.set_status(&root, s);
                    }
                }
            })
            .expect("spawn graph index thread");
    }

    /// Synchronous full rebuild: reset the store, walk the tree, parse every
    /// supported file, write each file's graph, and record the visited-file
    /// count. Returns the final stored counts. [`spawn_rebuild`](Self::spawn_rebuild)
    /// wraps this with status bookkeeping on a worker thread; the build itself
    /// lives in the free [`build_tree`] fn so it's testable without the app.
    pub fn rebuild_blocking(&self, root: &Path) -> AppResult<GraphStats> {
        let snap = self.settings.current().graph;
        let idx = self.index_for(root)?;
        let (indexed, stats) = build_tree(&idx, root, &snap, &self.db_subdir())?;

        // Record the visited-file count alongside the authoritative row counts.
        {
            let mut s = self.status(root);
            s.files_indexed = indexed;
            self.status.lock().unwrap().insert(root.to_path_buf(), s);
        }
        Ok(stats)
    }

    /// Drop warm handles on shutdown (SQLite connections close on drop).
    pub fn shutdown(&self) {
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
    let langs = &snap.languages; // matched against `Lang::tag()`

    let mut indexed: u64 = 0;
    let walker = WalkBuilder::new(root)
        .hidden(false) // index dotfiles like `.github/*.md`; the db dir is filtered below
        .git_ignore(true)
        .git_global(true)
        .git_exclude(true)
        .parents(true)
        .build();

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

        let lang = Lang::from_path(path);
        if lang == Lang::Other || !langs.iter().any(|l| l == lang.tag()) {
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

    Ok((indexed, idx.stats()?))
}

/// Project-relative path with forward slashes, matching what the parser stores
/// and the MCP tools query against. Falls back to the file name if `path`
/// isn't under `root` (shouldn't happen for a walk rooted at `root`).
fn rel_path(root: &Path, path: &Path) -> String {
    let rel = path.strip_prefix(root).unwrap_or(path);
    rel.to_string_lossy().replace('\\', "/")
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

        // Distinct subdir so the test never touches a real `.ccimp`.
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
}
