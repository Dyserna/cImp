//! V13 — the Workbench feature module: a live diff pane (Phase B),
//! checkpoints via a shadow git repo (Phase C), and a worktree manager
//! (Phase D), all riding one reserved app-rendered tab (`workbench-1`,
//! Phase A). See `docs/MILESTONE-V13-vibe-guardrails.md` and
//! `docs/IMPL-PLAN-V13-vibe-guardrails.md`.
//!
//! Phase A lays the foundation every later phase builds on:
//!   - [`git`] — the spawned-`git` harness every Workbench git operation
//!     runs through (§0.2).
//!   - [`WorkbenchService`] — managed Tauri state owning the fs-batch
//!     broadcast (§0.3) and (from Phase C onward) checkpoint scheduling.
//!   - the reserved tab shell (`WORKBENCH_TAB_ID` in the frontend's
//!     `tabs/types.ts`, mirrored on the backend by `TabId::Workbench`).
//!
//! `diff`/`shadow`/`worktree` are near-empty placeholders filled in by
//! Phases B/C/D respectively.

pub mod diff;
pub mod git;
pub mod shadow;
pub mod worktree;

use std::path::{Path, PathBuf};

use serde::Serialize;
use tauri::{AppHandle, Emitter};
use tokio::sync::broadcast;

use crate::settings::SettingsHandle;

/// Tauri event name for the frontend's fs-batch subscription (§0.3). Payload
/// is [`FsBatch`]. Emitted only while `settings.workbench.enabled` — see
/// [`WorkbenchService::publish_fs_batch`].
pub const FS_BATCH_EVENT: &str = "fs-batch";

/// Hard cap on how many paths one `fs-batch` event/broadcast carries. A
/// coalesced watcher batch from a large checkout/format pass can be huge;
/// consumers (the Phase B diff refresh, a future checkpoint burst trigger)
/// only need "something changed, go re-check", not an exhaustive list, so
/// truncating with a flag is cheaper than an unbounded payload for both the
/// IPC event and the broadcast channel's backlog.
const FS_BATCH_MAX_PATHS: usize = 200;

/// §0.3: one coalesced filesystem-change batch, fanned out both as a Tauri
/// event (for the frontend) and over [`WorkbenchService::subscribe`]'s
/// broadcast channel (for backend consumers — the Phase B diff refresh, the
/// Phase C checkpoint burst trigger). `paths` are project-root-relative,
/// forward-slash normalized; `truncated` is set when the batch exceeded
/// [`FS_BATCH_MAX_PATHS`].
#[derive(Clone, Debug, Serialize)]
pub struct FsBatch {
    pub root: String,
    pub paths: Vec<String>,
    pub truncated: bool,
}

/// Managed Tauri state for the Workbench feature. Constructed unconditionally
/// at startup (like [`crate::graph::GraphService`] and the offload services)
/// so the IPC layer and other backend modules can always reach it — the
/// heavy machinery each phase adds (shadow-repo scheduling in Phase C,
/// worktree bookkeeping in Phase D) gates itself on the relevant settings
/// flag rather than on whether the service exists at all.
pub struct WorkbenchService {
    app: AppHandle,
    settings: SettingsHandle,
    fs_batch_tx: broadcast::Sender<FsBatch>,
}

/// Backend-facing status for the Workbench tab's top-of-view banner (A2): is
/// `git` even on PATH, and if so, is the given root inside a working tree?
/// Both fields are informational only — the individual sections (Diff /
/// Timeline / Worktrees) decide what they need (`git_available` for Diff and
/// Worktrees; Timeline only needs checkpoints, filled in Phase C).
#[derive(Clone, Debug, Serialize)]
pub struct WorkbenchStatus {
    pub git_available: bool,
    pub is_repo: bool,
}

impl WorkbenchService {
    /// A generous backlog: the frontend and any backend subscriber are
    /// expected to drain promptly (a diff refresh / burst-trigger check is
    /// cheap), so this is a safety margin against a slow subscriber during a
    /// startup burst, not a steady-state buffer.
    const BROADCAST_CAPACITY: usize = 64;

    pub fn new(app: AppHandle, settings: SettingsHandle) -> std::sync::Arc<Self> {
        let (fs_batch_tx, _rx) = broadcast::channel(Self::BROADCAST_CAPACITY);
        std::sync::Arc::new(Self {
            app,
            settings,
            fs_batch_tx,
        })
    }

    /// Subscribe to fs-batch events from a backend consumer (Phase B's diff
    /// refresh, Phase C's burst-checkpoint trigger). The frontend instead
    /// listens for the [`FS_BATCH_EVENT`] Tauri event — this channel is
    /// backend-only. Unused until Phase B/C wire a consumer; kept public now
    /// per §0.3's contract rather than added later.
    #[allow(dead_code)]
    pub fn subscribe(&self) -> broadcast::Receiver<FsBatch> {
        self.fs_batch_tx.subscribe()
    }

    /// §0.3: fan a watcher-coalesced batch of changed paths out to Workbench
    /// consumers. Called from [`crate::graph::service::GraphService`] at the
    /// point its debounce thread hands over a batch (`reindex_paths`), before
    /// any graph-specific filtering — a batch of paths the graph itself
    /// ignores (unsupported extension, gitignored) can still be exactly what
    /// the diff pane or a checkpoint trigger cares about.
    ///
    /// Self-gates on `settings.workbench.enabled` so a user with the feature
    /// off sees no new idle chatter — callers don't need their own check.
    /// No-op (and free) when there are no paths.
    pub fn publish_fs_batch(&self, root: &Path, paths: &[PathBuf]) {
        if paths.is_empty() || !self.settings.current().workbench.enabled {
            return;
        }
        let truncated = paths.len() > FS_BATCH_MAX_PATHS;
        let rels: Vec<String> = paths
            .iter()
            .take(FS_BATCH_MAX_PATHS)
            .map(|p| p.display().to_string().replace('\\', "/"))
            .collect();
        let batch = FsBatch {
            root: root.display().to_string(),
            paths: rels,
            truncated,
        };
        // Both sends are best-effort: no frontend window listening, or no
        // backend subscriber yet, are both normal (nothing has opened the
        // Workbench tab this session) — never a reason to log or fail.
        let _ = self.app.emit(FS_BATCH_EVENT, &batch);
        let _ = self.fs_batch_tx.send(batch);
    }

    /// A2's top-of-view banner data: whether `git` resolves on PATH at all,
    /// and whether `root` is inside a working tree. `git_available: false`
    /// implies `is_repo: false` (there's no point probing further).
    pub async fn status(&self, root: &Path) -> WorkbenchStatus {
        if crate::pty::resolve_command("git").is_err() {
            return WorkbenchStatus {
                git_available: false,
                is_repo: false,
            };
        }
        WorkbenchStatus {
            git_available: true,
            is_repo: git::is_repo(root).await,
        }
    }
}
