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
//! [`diff`] is filled in by Phase B (unified-diff parsing + the live diff
//! pane's `summary`/`diff_file`, called from this module's
//! `diff_summary`/`diff_file`/`send_hunk`/`revert_hunk`); [`shadow`] by Phase
//! C (the checkpoint shadow repo, called from `on_prompt`/`checkpoint_now`/
//! `checkpoints`/`checkpoint_diff`/`restore`); [`worktree`] by Phase D (the
//! worktree manager, called from `worktrees`/`worktree_create`/
//! `worktree_merge`/`worktree_discard`/`worktree_run_checks` below).

pub mod diff;
pub mod git;
pub mod history;
pub mod shadow;
pub mod worktree;

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::Serialize;
use tauri::{AppHandle, Emitter};
use tokio::sync::broadcast;
use tracing::warn;

use crate::error::{AppError, AppResult};
use crate::settings::{SettingsHandle, TabConfig};

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
    /// Phase C §C3: the min-gap gate every AUTOMATIC checkpoint trigger
    /// (prompt-tap, burst) shares — keyed PER PROJECT ROOT (FIX 4 / V13 code
    /// review: this used to be a single global `Mutex<Option<Instant>>`, so
    /// a checkpoint in project A would swallow project B's within the
    /// min-gap — a checkpoint that never fires for a distinct project just
    /// because some OTHER project happened to checkpoint moments earlier).
    /// Mirrors [`Self::burst_state`]'s per-root shape. No entry for a root
    /// until its first automatic OR manual snapshot fires.
    /// [`checkpoint_now`](Self::checkpoint_now) (the manual trigger) bypasses
    /// the gate itself but still updates its root's entry, so a manual
    /// checkpoint counts toward that root's next automatic trigger's
    /// cooldown.
    checkpoint_last: Mutex<HashMap<PathBuf, Instant>>,
    /// Phase C §C3 burst trigger: per-root rolling window of distinct
    /// changed paths seen since the window last reset. Keyed by project root
    /// since multiple projects can be open in different tabs at once.
    burst_state: Mutex<HashMap<PathBuf, BurstState>>,
    /// Phase D D3: the merge-readiness chip's last "Run checks" result per
    /// `(root, slug)`, so the Worktrees table can show a cached pass/fail +
    /// age without re-running every configured check on every render — only
    /// [`worktree_run_checks`](Self::worktree_run_checks) (an explicit row
    /// button, or a future auto-check hook) refreshes an entry.
    worktree_check_cache: Mutex<HashMap<(PathBuf, String), WorktreeCheckStatus>>,
    /// Short-TTL cache for the Sessions card's per-session commit-count
    /// probe: the lightweight `(hash, ts_ms)` walk behind
    /// [`session_commit_counts`](Self::session_commit_counts), keyed per
    /// root. The counts poll is periodic (every open view ticks it), so the
    /// git subprocess should run at most once per
    /// [`COMMIT_TIMES_TTL`](Self::COMMIT_TIMES_TTL) per root regardless of
    /// how many callers ask — same server-side posture as
    /// [`Self::worktree_check_cache`].
    commit_times_cache: Mutex<HashMap<PathBuf, CachedCommitTimes>>,
}

/// One root's cached commit-time walk: when it was taken and the shared
/// `(hash, ts_ms)` list it produced.
type CachedCommitTimes = (Instant, Arc<Vec<(String, i64)>>);

/// One project root's rolling burst-trigger accumulator (see
/// [`WorkbenchService::handle_fs_batch_for_burst`]).
struct BurstState {
    paths: HashSet<String>,
    window_start: Instant,
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

/// Phase D D3: the merge-readiness chip's cached result for one worktree —
/// `pass` gates the Merge button's green highlight (advisory only; a merge is
/// never auto-triggered by this). `checked_at_unix` lets the frontend show an
/// age ("checked 4m ago") and decide whether to suggest a re-run.
///
/// **Known rough edge** (soft-dep V12 Phase A, called out by the milestone as
/// acceptable to ship with — see `IMPL-PLAN-V13-vibe-guardrails.md` §D3):
/// `checks::run`'s `changed_only` flag filters diagnostics to files touched
/// since the CWD's own `HEAD` (uncommitted/staged changes). In a worktree
/// where everything is already committed (the normal case right before a
/// merge), that filter would see nothing at all — it has no concept of "vs
/// the base branch this worktree was cut from". So this runs every
/// configured check UNFILTERED (`changed_only: false`) with `cwd` = the
/// worktree, which is still a meaningful "does this worktree pass its own
/// checks" signal, just not scoped to only the worktree's own diff the way a
/// `changed_only vs base` mode would be. Promoting `checks::run` to accept an
/// explicit base ref is a clean follow-on, not attempted here.
#[derive(Clone, Debug, Serialize)]
pub struct WorktreeCheckStatus {
    pub pass: bool,
    pub checked_at_unix: u64,
    pub reports: Vec<crate::checks::CheckReport>,
}

impl WorkbenchService {
    /// A generous backlog: the frontend and any backend subscriber are
    /// expected to drain promptly (a diff refresh / burst-trigger check is
    /// cheap), so this is a safety margin against a slow subscriber during a
    /// startup burst, not a steady-state buffer.
    const BROADCAST_CAPACITY: usize = 64;

    /// Phase C §C3 burst trigger: how long a project's rolling
    /// distinct-path window is remembered before it resets on the next
    /// fs-batch, independent of `checkpoint_burst_window_s` (which is read
    /// live from settings on every batch — this cap just bounds how stale a
    /// long-idle project's accumulator can get in memory).
    const BURST_STATE_MAX_AGE: Duration = Duration::from_secs(3600);

    /// FIX 8 (V13 code review): `worktree_check_cache`'s sibling age-based
    /// eviction — mirrors [`Self::BURST_STATE_MAX_AGE`]'s reasoning. Without
    /// this, a worktree removed out-of-band (a file manager `rm -rf`
    /// bypassing `workbench_worktree_discard`, which explicitly drops its own
    /// entry) leaves a permanent, never-evicted row behind for the rest of
    /// the app's lifetime. A day is generous — check results are
    /// user-triggered (a row's "Run checks" click), not a steady-state
    /// stream, so there's no reason to evict aggressively; this just bounds
    /// the worst case for a long-running session across many projects.
    const WORKTREE_CHECK_CACHE_MAX_AGE_SECS: u64 = 24 * 3600;

    pub fn new(app: AppHandle, settings: SettingsHandle) -> Arc<Self> {
        let (fs_batch_tx, _rx) = broadcast::channel(Self::BROADCAST_CAPACITY);
        let svc = Arc::new(Self {
            app,
            settings,
            fs_batch_tx,
            checkpoint_last: Mutex::new(HashMap::new()),
            burst_state: Mutex::new(HashMap::new()),
            worktree_check_cache: Mutex::new(HashMap::new()),
            commit_times_cache: Mutex::new(HashMap::new()),
        });
        svc.clone().spawn_burst_trigger();
        svc
    }

    /// Phase C §C3: a long-lived task (app lifetime, mirrors the offload
    /// watch-loop pattern in `main.rs`) that subscribes to this service's own
    /// fs-batch broadcast and feeds [`handle_fs_batch_for_burst`]. Holding a
    /// strong `Arc` here is intentional — `WorkbenchService` is a singleton
    /// for the app's lifetime (like `GraphService`), so there's no early-drop
    /// case this would block; the task exits on its own once the broadcast
    /// sender is gone (app shutdown).
    fn spawn_burst_trigger(self: Arc<Self>) {
        let mut rx = self.fs_batch_tx.subscribe();
        tauri::async_runtime::spawn(async move {
            loop {
                match rx.recv().await {
                    Ok(batch) => self.handle_fs_batch_for_burst(batch).await,
                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
        });
    }

    /// Phase C §C3 burst trigger: accumulate `batch`'s paths into `batch.root`'s
    /// rolling window; fire an "activity" checkpoint once the window holds at
    /// least `checkpoint_burst_files` distinct paths within
    /// `checkpoint_burst_window_s` (the window resets on fire OR on going
    /// stale). [`maybe_snapshot`](Self::maybe_snapshot) separately enforces
    /// `checkpoint_min_gap_s`, so a burst that fires right after a prompt-tap
    /// snapshot is still debounced there.
    async fn handle_fs_batch_for_burst(&self, batch: FsBatch) {
        let cfg = self.settings.current();
        if !cfg.workbench.checkpoints {
            return;
        }
        // Normalize the root to a single canonical form: `batch.root` arrives
        // as a `root.display()` string round-trip, while the prompt-tap path
        // keys `checkpoint_last` from a raw `&Path`. Without normalizing, the
        // two representations of the same project become distinct map keys and
        // the shared `checkpoint_min_gap_s` debounce stops working across the
        // burst and prompt triggers.
        let root = git::canonical_path(&PathBuf::from(&batch.root));
        let burst_files = cfg.workbench.checkpoint_burst_files.max(1) as usize;
        let window = Duration::from_secs(cfg.workbench.checkpoint_burst_window_s.max(1) as u64);

        let fire = {
            let mut state = self.burst_state.lock().unwrap_or_else(|e| e.into_inner());
            // Opportunistically drop long-idle roots so this map doesn't grow
            // unbounded across a long session touching many projects.
            state.retain(|_, s| s.window_start.elapsed() < Self::BURST_STATE_MAX_AGE);
            let entry = state.entry(root.clone()).or_insert_with(|| BurstState {
                paths: HashSet::new(),
                window_start: Instant::now(),
            });
            if entry.window_start.elapsed() > window {
                entry.paths.clear();
                entry.window_start = Instant::now();
            }
            entry.paths.extend(batch.paths.iter().cloned());
            if entry.paths.len() >= burst_files {
                entry.paths.clear();
                entry.window_start = Instant::now();
                true
            } else {
                false
            }
        };
        if fire {
            self.maybe_snapshot(&root, "activity".to_string(), shadow::Trigger::Burst, None)
                .await;
        }
    }

    /// `true` iff `settings.workbench.checkpoints` is on — the single gate
    /// every automatic AND manual checkpoint operation checks before doing
    /// any shadow-repo work.
    pub fn checkpoints_enabled(&self) -> bool {
        self.settings.current().workbench.checkpoints
    }

    /// Phase C §C3: the shared entry point for the two AUTOMATIC triggers
    /// (prompt-tap, burst). Enforces `checkpoint_min_gap_s` against `root`'s
    /// own entry in the per-root `checkpoint_last` gate (so a rapid prompt
    /// sequence or a burst right after a prompt-tap snapshot can't spam the
    /// shadow repo — independently per project root), then
    /// does the actual `shadow::snapshot` + opportunistic `shadow::gc` on a
    /// background task so the caller (a prompt hook, an fs-batch handler)
    /// never blocks on it — per the milestone's "never block a prompt or the
    /// UI thread" contract (C2). No-op entirely when checkpoints are off.
    async fn maybe_snapshot(
        &self,
        root: &Path,
        label: String,
        trigger: shadow::Trigger,
        agent: Option<String>,
    ) {
        if !self.checkpoints_enabled() {
            return;
        }
        let cfg = self.settings.current();
        let min_gap = Duration::from_secs(cfg.workbench.checkpoint_min_gap_s as u64);
        // Canonicalize so this gate keys on the same form as every other
        // `checkpoint_last` writer (burst, manual, restore) regardless of how
        // the caller spelled `root`.
        let root = git::canonical_path(root);
        {
            let mut last = self
                .checkpoint_last
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            if let Some(prev) = last.get(&root) {
                if prev.elapsed() < min_gap {
                    return;
                }
            }
            last.insert(root.clone(), Instant::now());
        }
        let extra_ignore = cfg.graph.ignore.clone();
        let max_file_bytes = cfg.graph.max_file_bytes;
        let checkpoint_max = cfg.workbench.checkpoint_max;
        let checkpoint_max_age_days = cfg.workbench.checkpoint_max_age_days;
        tauri::async_runtime::spawn(async move {
            match shadow::snapshot(
                &root,
                &label,
                trigger,
                agent.as_deref(),
                &extra_ignore,
                max_file_bytes,
            )
            .await
            {
                Ok(_) => {
                    if let Err(e) = shadow::gc(&root, checkpoint_max, checkpoint_max_age_days).await
                    {
                        warn!(root = %root.display(), error = %e, "workbench: checkpoint gc failed");
                    }
                }
                Err(e) => {
                    warn!(root = %root.display(), error = %e, "workbench: automatic checkpoint failed")
                }
            }
        });
    }

    /// Phase C §C3 prompt-tap trigger: called from `offload/loopback.rs`'s
    /// `/context/retrieve` handler for EVERY prompt (Claude's
    /// `UserPromptSubmit` shim, the OpenCode `chat.message` plugin) —
    /// deliberately BEFORE that handler's own `context_injection` gate, so
    /// checkpointing runs even when injection is off or yields nothing (the
    /// milestone's Decision 4: checkpointing is decoupled from the injection
    /// toggle even though it reuses the same transport). `agent` is whatever
    /// the calling shim identified itself as (`"claude"`/`"opencode"`/`None`
    /// for unknown); `prompt_head` is used as-is for the label — callers are
    /// expected to have already truncated it (`shadow::snapshot`'s own
    /// [`truncate_label`](shadow) is a hard backstop, not the primary cut).
    pub async fn on_prompt(&self, root: &Path, agent: Option<String>, prompt_head: &str) {
        let label = format!("prompt: {prompt_head}");
        self.maybe_snapshot(root, label, shadow::Trigger::Prompt, agent)
            .await;
    }

    /// Phase C `workbench_checkpoint_now`: the manual trigger. Unlike the
    /// automatic triggers, this does NOT go through
    /// [`maybe_snapshot`](Self::maybe_snapshot)'s min-gap gate or its
    /// background-task indirection — an explicit "Checkpoint now" click is a
    /// deliberate user action awaiting a real result (the new checkpoint id),
    /// not something to silently throttle or defer. It still updates
    /// `checkpoint_last` so a subsequent AUTOMATIC trigger's cooldown counts
    /// from this snapshot too (no back-to-back auto + manual spam).
    pub async fn checkpoint_now(
        &self,
        root: &Path,
        label: Option<String>,
    ) -> AppResult<shadow::CheckpointId> {
        let cfg = self.settings.current();
        let label = label.unwrap_or_else(|| "manual checkpoint".to_string());
        let id = shadow::snapshot(
            root,
            &label,
            shadow::Trigger::Manual,
            None,
            &cfg.graph.ignore,
            cfg.graph.max_file_bytes,
        )
        .await?;
        self.checkpoint_last
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(git::canonical_path(root), Instant::now());
        if let Err(e) = shadow::gc(
            root,
            cfg.workbench.checkpoint_max,
            cfg.workbench.checkpoint_max_age_days,
        )
        .await
        {
            warn!(root = %root.display(), error = %e, "workbench: checkpoint gc failed after manual checkpoint");
        }
        Ok(id)
    }

    /// Phase C `workbench_checkpoints`: the Timeline section's row list.
    pub async fn checkpoints(&self, root: &Path) -> AppResult<Vec<shadow::Checkpoint>> {
        shadow::list(root).await
    }

    /// Phase C `workbench_checkpoint_diff`: checkpoint `id` vs. the CURRENT
    /// working tree, parsed into the same [`diff::FileDiff`] shape the B4
    /// `DiffView` already renders — powers both the Timeline's "Diff vs now"
    /// action and the restore confirmation dialog's dry-run file list.
    pub async fn checkpoint_diff(
        &self,
        root: &Path,
        id: &str,
        context: u32,
    ) -> AppResult<Vec<diff::FileDiff>> {
        let cfg = self.settings.current();
        let text = shadow::diff_vs_now(
            root,
            id,
            &cfg.graph.ignore,
            cfg.graph.max_file_bytes,
            context,
        )
        .await?;
        Ok(diff::parse_unified(&text))
    }

    /// Phase C `workbench_restore`: restore the working tree to checkpoint
    /// `id`. `delete_new` gates invariant D (files created since the
    /// checkpoint are deleted only when explicitly requested) — see
    /// `shadow::restore`'s doc comment for the full sequence and every
    /// safety invariant it upholds. Updates `checkpoint_last` for the same
    /// reason [`checkpoint_now`](Self::checkpoint_now) does.
    pub async fn restore(
        &self,
        root: &Path,
        id: &str,
        delete_new: bool,
    ) -> AppResult<shadow::RestoreReport> {
        let cfg = self.settings.current();
        let report = shadow::restore(
            root,
            id,
            delete_new,
            &cfg.graph.ignore,
            cfg.graph.max_file_bytes,
        )
        .await?;
        self.checkpoint_last
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(git::canonical_path(root), Instant::now());
        Ok(report)
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

    /// Phase B `workbench_diff_summary`: the file list + readonly/source
    /// flags for the Diff section. [`diff::summary`] handles the git case;
    /// this service method layers FIX 7 (V13 code review)'s Phase C
    /// shadow-repo fallback on top for a NON-git project — when `diff::summary`
    /// comes back with `source: None` (not a git repo) AND checkpoints are
    /// on AND at least one checkpoint exists, diff against the LATEST
    /// checkpoint via [`shadow::diff_vs_now`] instead, so a non-git project
    /// with checkpoints enabled gets a working Diff pane rather than just the
    /// requirements banner. Falls back to the plain `source: None` result
    /// (unchanged) when checkpoints are off or there's nothing checkpointed
    /// yet — same "frontend renders the requirements banner" contract
    /// `diff::summary` already documents.
    pub async fn diff_summary(&self, root: &Path) -> AppResult<diff::DiffSummary> {
        let summary = diff::summary(root).await?;
        if summary.source.is_some() || !self.checkpoints_enabled() {
            return Ok(summary);
        }
        let latest = match shadow::list(root).await {
            Ok(list) => list.into_iter().next_back(),
            Err(e) => {
                // A broken shadow repo shouldn't crash the diff pane, but it
                // also shouldn't masquerade as "no checkpoints" — log it so the
                // failure is diagnosable rather than silent.
                warn!(root = %root.display(), error = %e, "workbench: shadow checkpoint list failed; falling back to plain summary");
                None
            }
        };
        let Some(latest) = latest else {
            return Ok(summary);
        };
        let cfg = self.settings.current();
        let text = shadow::diff_vs_now(
            root,
            &latest.id,
            &cfg.graph.ignore,
            cfg.graph.max_file_bytes,
            diff::DEFAULT_CONTEXT,
        )
        .await?;
        let mut files: Vec<diff::FileDiffMeta> = diff::parse_unified(&text)
            .iter()
            .map(diff::file_diff_meta_from_parsed)
            .collect();
        files.sort_by(|a, b| a.path.cmp(&b.path));
        Ok(diff::DiffSummary {
            files,
            readonly: false,
            source: Some(diff::DiffSource::Shadow),
        })
    }

    /// Phase B `workbench_diff_file`: one file's full parsed diff. Mirrors
    /// [`diff_summary`](Self::diff_summary)'s FIX 7 shadow-repo fallback for
    /// a non-git project with checkpoints on: parses the SAME
    /// `shadow::diff_vs_now` blob against the latest checkpoint and picks out
    /// `path`'s entry. A `path` with no entry in that diff (already clean, or
    /// the caller raced a refresh) gets the same "clean, no changes" shape
    /// [`diff::diff_file`] returns for that case.
    pub async fn diff_file(
        &self,
        root: &Path,
        path: &str,
        context: u32,
    ) -> AppResult<diff::FileDiff> {
        if git::is_repo(root).await || !self.checkpoints_enabled() {
            return diff::diff_file_ctx(root, path, context).await;
        }
        let latest = match shadow::list(root).await {
            Ok(list) => list.into_iter().next_back(),
            Err(e) => {
                warn!(root = %root.display(), error = %e, "workbench: shadow checkpoint list failed; falling back to plain diff");
                None
            }
        };
        let Some(latest) = latest else {
            return diff::diff_file_ctx(root, path, context).await;
        };
        let cfg = self.settings.current();
        let text = shadow::diff_vs_now(
            root,
            &latest.id,
            &cfg.graph.ignore,
            cfg.graph.max_file_bytes,
            context,
        )
        .await?;
        if let Some(file) = diff::parse_unified(&text)
            .into_iter()
            .find(|f| f.path == path)
        {
            return Ok(file);
        }
        Ok(diff::FileDiff {
            path: path.to_string(),
            status: diff::FileStatus::Modified,
            binary: false,
            hunks: Vec::new(),
            too_large: false,
        })
    }

    /// Phase B `workbench_send_hunk`. Thin wrapper over [`send_hunk`] (kept
    /// free-standing so it's testable without an `AppHandle` — see that
    /// function's doc comment).
    pub async fn send_hunk(&self, root: &Path, path: &str, hunk_index: usize) -> AppResult<String> {
        send_hunk(root, path, hunk_index).await
    }

    /// Phase B `workbench_revert_hunk`. Thin wrapper over [`revert_hunk`]
    /// (kept free-standing for the same reason as [`send_hunk`] above).
    pub async fn revert_hunk(
        &self,
        root: &Path,
        path: &str,
        hunk_index: usize,
        hunk_hash: &str,
    ) -> AppResult<diff::FileDiff> {
        revert_hunk(root, path, hunk_index, hunk_hash).await
    }

    /// Phase D `workbench_worktrees`: every cImp-managed worktree of `root`'s
    /// repo. `has_live_tab` isn't known to [`worktree::list`] itself (it has
    /// no `Settings` access) — filled in here by checking whether any
    /// configured AI tab's `cwd` points at exactly that worktree's path. D3's
    /// "New tab in worktree" flow always sets an AI tab's `cwd` to the fresh
    /// worktree's path, so this is an exact match, not a heuristic.
    pub async fn worktrees(&self, root: &Path) -> AppResult<Vec<worktree::WorktreeInfo>> {
        let mut infos = worktree::list(root).await?;
        if infos.is_empty() {
            return Ok(infos);
        }
        let live_paths = self.live_tab_paths();
        for info in &mut infos {
            info.has_live_tab = live_paths.contains(&git::canonical_path(Path::new(&info.path)));
        }
        Ok(infos)
    }

    /// Canonicalized `cwd` of every configured AI tab — the "live tab" test
    /// behind [`Self::worktrees`]' `has_live_tab` flag and
    /// [`Self::worktree_discard`]'s refusal. Canonicalize both the tab cwds
    /// and each compared worktree path: a worktree path is git's resolved
    /// form while a tab `cwd` is stored as-configured, so an exact string
    /// compare misses matches that differ only by drive-letter case, 8.3
    /// short name, or the `\\?\` prefix.
    fn live_tab_paths(&self) -> HashSet<PathBuf> {
        self.settings
            .current()
            .tabs
            .iter()
            .filter_map(|t| match t {
                TabConfig::AiTool(cfg) => cfg.cwd.as_ref(),
                TabConfig::Shell(_) | TabConfig::Preview(_) => None,
            })
            .map(|p| git::canonical_path(p))
            .collect()
    }

    /// Phase D D3's per-row **Diff** action: worktree `slug` vs. the base
    /// branch it was cut from, via [`worktree::diff_against_base`]. Read-only
    /// (a diff between two commits, not the working tree) — no revert action
    /// applies here.
    pub async fn worktree_diff(
        &self,
        root: &Path,
        slug: &str,
        context: u32,
    ) -> AppResult<Vec<diff::FileDiff>> {
        worktree::diff_against_base(root, slug, context).await
    }

    /// Short TTL for [`Self::session_commit_counts`]'s cached commit-times
    /// walk — long enough that a 2s frontend poll never re-spawns git,
    /// short enough that a fresh commit's badge appears promptly.
    const COMMIT_TIMES_TTL: Duration = Duration::from_secs(10);

    /// Session-commits section: the union of commits recorded live for the
    /// session (`recorded` hash prefixes, flagged `tracked`) and commits
    /// whose committer time falls inside `from_ms..=to_ms` — see
    /// [`history::session_commits`].
    pub async fn session_commits(
        &self,
        root: &Path,
        from_ms: i64,
        to_ms: i64,
        recorded: &[String],
    ) -> AppResult<history::SessionCommits> {
        history::session_commits(root, from_ms, to_ms, recorded).await
    }

    /// Per-session commit counts for the Sessions card's button state — one
    /// lightweight (hash + time) log walk shared across every window AND
    /// every caller: the walk is cached per root for
    /// [`COMMIT_TIMES_TTL`](Self::COMMIT_TIMES_TTL), so periodic polls hit
    /// the cache instead of re-spawning git. Same union semantics as
    /// [`Self::session_commits`].
    pub async fn session_commit_counts(
        &self,
        root: &Path,
        windows: &[history::SessionWindow],
        recorded: &HashMap<String, Vec<String>>,
    ) -> AppResult<HashMap<String, u32>> {
        let key = git::canonical_path(root);
        let cached = {
            let cache = self
                .commit_times_cache
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            cache
                .get(&key)
                .filter(|(at, _)| at.elapsed() < Self::COMMIT_TIMES_TTL)
                .map(|(_, times)| times.clone())
        };
        let times = match cached {
            Some(times) => times,
            None => {
                let (walk, _) = history::log_commit_times(root, history::MAX_LOG_COMMITS).await?;
                let times = Arc::new(walk);
                self.commit_times_cache
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .insert(key, (Instant::now(), times.clone()));
                times
            }
        };
        Ok(history::commit_counts_from(&times, windows, recorded))
    }

    /// One commit vs. its first parent, in the shared [`diff::FileDiff`]
    /// shape. Read-only.
    pub async fn commit_diff(
        &self,
        root: &Path,
        hash: &str,
        context: u32,
    ) -> AppResult<Vec<diff::FileDiff>> {
        history::commit_diff(root, hash, context).await
    }

    /// The Git-graph section's topologically-ordered commit list + HEAD.
    pub async fn git_graph(&self, root: &Path, limit: usize) -> AppResult<history::GitGraph> {
        history::git_graph(root, limit).await
    }

    /// Phase D `workbench_worktree_create`. Thin pass-through to
    /// [`worktree::create`] — see that function's doc comment for the full
    /// precondition sequence (nested-repo refusal, detached-HEAD refusal,
    /// duplicate-slug refusal).
    pub async fn worktree_create(&self, root: &Path, slug: &str) -> AppResult<PathBuf> {
        worktree::create(root, slug).await
    }

    /// Phase D `workbench_worktree_merge`. **Safety-critical** — see
    /// `worktree::merge`'s doc comment: this either fully merges or leaves
    /// the main tree exactly as it was; there is no partial/half-merged
    /// outcome from this call.
    pub async fn worktree_merge(
        &self,
        root: &Path,
        slug: &str,
    ) -> AppResult<worktree::MergeReport> {
        worktree::merge(root, slug).await
    }

    /// Phase D `workbench_worktree_discard`. Double-confirmation is the
    /// frontend's job (D3); this is the unconditional action once confirmed —
    /// except while an AI tab is still open in the worktree, which is refused:
    /// on Windows the removal would fail anyway (the tab's PTY holds the
    /// directory as its cwd, leaving a confusing git error), and on Linux it
    /// would succeed and yank the directory out from under the still-running
    /// agent. Also drops any cached check status for `slug` so a later
    /// re-creation under the same name doesn't show a stale chip.
    pub async fn worktree_discard(&self, root: &Path, slug: &str) -> AppResult<()> {
        if let Ok(path) = worktree::resolve_path(root, slug) {
            if self.live_tab_paths().contains(&git::canonical_path(&path)) {
                return Err(AppError::Workbench(format!(
                    "an AI tab is still open in worktree '{slug}' — close that tab first, then discard."
                )));
            }
        }
        worktree::discard(root, slug).await?;
        self.worktree_check_cache
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&(root.to_path_buf(), slug.to_string()));
        Ok(())
    }

    /// Phase D `worktree::prune`, called once at app start (see `main.rs`)
    /// for `root` = the launch directory. Best-effort by design — logged, not
    /// propagated, since a prune failure (or `root` not being a repo at all)
    /// should never affect app startup.
    pub async fn worktree_prune_at_startup(&self, root: &Path) {
        if let Err(e) = worktree::prune(root).await {
            warn!(root = %root.display(), error = %e, "workbench: worktree prune at startup failed");
        }
    }

    /// Phase D D3 merge-readiness chip: run every configured check
    /// (`settings.checks`) with `cwd` = worktree `slug`'s directory, cache
    /// the aggregate pass/fail (no `error`-severity diagnostic groups in any
    /// report = pass), and return it. See [`WorktreeCheckStatus`]'s doc
    /// comment for the `changed_only` rough edge this accepts for V1.
    /// `pass` is `true` (vacuously) when no checks are configured at all —
    /// the frontend is expected to render "no checks configured" rather than
    /// a green chip in that case, using `reports.is_empty()` to tell the two
    /// apart.
    pub async fn worktree_run_checks(
        &self,
        root: &Path,
        slug: &str,
    ) -> AppResult<WorktreeCheckStatus> {
        let wt_path = worktree::resolve_path(root, slug)?;
        let checks = self.settings.current().checks.clone();
        let mut reports = Vec::with_capacity(checks.len());
        let mut pass = true;
        for def in &checks {
            match crate::checks::run(&wt_path, def, false).await {
                Ok(report) => {
                    if report
                        .groups
                        .iter()
                        .any(|g| g.severity == crate::checks::Severity::Error)
                    {
                        pass = false;
                    }
                    reports.push(report);
                }
                Err(e) => {
                    warn!(root = %root.display(), slug, check = %def.name, error = %e, "workbench: worktree check run failed");
                    pass = false;
                }
            }
        }
        let checked_at_unix = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let status = WorktreeCheckStatus {
            pass,
            checked_at_unix,
            reports,
        };
        {
            let mut cache = self
                .worktree_check_cache
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            // FIX 8: opportunistic age-based eviction — see
            // `WORKTREE_CHECK_CACHE_MAX_AGE_SECS`'s doc comment.
            cache.retain(|_, v| {
                checked_at_unix.saturating_sub(v.checked_at_unix)
                    < Self::WORKTREE_CHECK_CACHE_MAX_AGE_SECS
            });
            cache.insert((root.to_path_buf(), slug.to_string()), status.clone());
        }
        Ok(status)
    }

    /// The merge-readiness chip's last cached result for `slug`, if any
    /// check has ever been run for it this session — `None` renders as
    /// "not checked yet" rather than a stale/default value.
    pub fn worktree_check_status(&self, root: &Path, slug: &str) -> Option<WorktreeCheckStatus> {
        self.worktree_check_cache
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&(root.to_path_buf(), slug.to_string()))
            .cloned()
    }
}

/// Format one hunk as a fenced block + `path:line` header for the compose
/// overlay (`workbench_send_hunk`). Re-derives the file's diff rather than
/// trusting a client-cached copy — cheap (one `git` round trip) and
/// guarantees the sent text matches what's actually on disk right now. A
/// free function (not a [`WorkbenchService`] method) so it's unit-testable
/// without constructing an `AppHandle` — mirrors how `graph::service`'s own
/// tests drive free functions directly rather than the service struct (see
/// that module's test doc comments).
pub async fn send_hunk(root: &Path, path: &str, hunk_index: usize) -> AppResult<String> {
    let file = diff::diff_file(root, path).await?;
    let hunk = file
        .hunks
        .get(hunk_index)
        .ok_or_else(|| AppError::Workbench(format!("hunk {hunk_index} out of range for {path}")))?;
    Ok(diff::format_hunk_for_agent(path, hunk))
}

/// Phase B B2: revert one hunk by reconstructing a minimal patch and piping
/// it through `git apply --reverse --unidiff-zero -`. Two safety guards
/// before anything touches the working tree:
///   - **readonly**: refused outright while `root` is mid-merge/-rebase
///     ([`diff::readonly`]) — the milestone's edge case for special git
///     states, since a hunk revert during a conflict resolution could easily
///     make things worse, not better.
///   - **hunk_hash staleness**: the caller (the frontend) echoes back the
///     hash it was shown for this exact hunk index; if the file's CURRENT
///     hunk at that index hashes differently, something else (an agent edit,
///     a manual save) changed the file since the diff was rendered, and
///     applying the old patch could revert content the user never saw.
///     Refused rather than best-effort-applied.
///
/// Returns the file's fresh diff after the revert (or an `Err` from either
/// guard, or from `git apply` itself — never a partial apply: `git apply` is
/// all-or-nothing per invocation, and neither guard has touched anything on
/// disk by the time it fires).
///
/// A free function for the same testability reason as [`send_hunk`].
///
/// TODO(Phase C): once `shadow::snapshot` exists, take a "pre-revert"
/// checkpoint here before the `git apply` call, per the milestone's "restore
/// is always undoable" contract — this V1 revert has no undo of its own
/// beyond the user's normal `git` tooling.
pub async fn revert_hunk(
    root: &Path,
    path: &str,
    hunk_index: usize,
    hunk_hash: &str,
) -> AppResult<diff::FileDiff> {
    if diff::readonly(root).await {
        return Err(AppError::Workbench(
            "cannot revert a hunk while a merge or rebase is in progress".to_string(),
        ));
    }
    let file = diff::diff_file(root, path).await?;
    let hunk = file
        .hunks
        .get(hunk_index)
        .ok_or_else(|| AppError::Workbench(format!("hunk {hunk_index} out of range for {path}")))?;
    if diff::hunk_hash(hunk) != hunk_hash {
        return Err(AppError::Workbench(
            "this hunk changed since it was shown (an edit raced the diff view) — refresh and try again"
                .to_string(),
        ));
    }
    let patch = diff::build_hunk_patch(&file, hunk);
    let ctx = git::GitCtx::discover(root);
    let out = git::run_with_stdin(
        &ctx,
        &["apply", "--reverse", "--unidiff-zero", "-"],
        &patch,
        None,
    )
    .await?;
    if !out.success() {
        return Err(AppError::Workbench(format!(
            "git apply --reverse failed: {}",
            out.stderr.trim()
        )));
    }
    diff::diff_file(root, path).await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn has_git() -> bool {
        crate::pty::resolve_command("git").is_ok()
    }

    fn git(dir: &Path, args: &[&str]) {
        let out = std::process::Command::new("git")
            .args(args)
            .current_dir(dir)
            .output()
            .expect("git");
        assert!(
            out.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    fn setup_repo(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("wb-mod-{tag}-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        git(&dir, &["init", "-q"]);
        git(&dir, &["config", "user.email", "test@example.com"]);
        git(&dir, &["config", "user.name", "Test"]);
        git(&dir, &["config", "core.autocrlf", "false"]);
        dir
    }

    /// B6: the core revert-safety round trip — revert a hunk on a real temp
    /// repo, and confirm the file's content actually changed back. This is
    /// the one test that exercises the full `diff_file` → `hunk_hash` →
    /// `build_hunk_patch` → `git apply --reverse` chain end to end, not just
    /// its pieces in isolation.
    #[tokio::test]
    async fn revert_hunk_reverses_a_line_change_on_disk() {
        if !has_git() {
            eprintln!("skipping: git not on PATH");
            return;
        }
        let dir = setup_repo("revert");
        std::fs::write(dir.join("f.txt"), "line1\nline2\nline3\n").unwrap();
        git(&dir, &["add", "f.txt"]);
        git(&dir, &["commit", "-q", "-m", "init"]);
        std::fs::write(dir.join("f.txt"), "line1\nline2-CHANGED\nline3\n").unwrap();

        let before = diff::diff_file(&dir, "f.txt").await.expect("diff before");
        assert_eq!(before.hunks.len(), 1);
        let hash = diff::hunk_hash(&before.hunks[0]);

        let after = revert_hunk(&dir, "f.txt", 0, &hash)
            .await
            .expect("revert_hunk");
        assert!(
            after.hunks.is_empty(),
            "expected a clean file after revert: {after:?}"
        );
        let content = std::fs::read_to_string(dir.join("f.txt")).unwrap();
        assert_eq!(content, "line1\nline2\nline3\n");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Regression (H1): a CRLF-terminated file must survive the full revert
    /// round trip. Before the fix, `parse_unified` split with `str::lines()`
    /// (which strips `\r`), so `build_hunk_patch` emitted an LF-only patch that
    /// `git apply --reverse` could not match against the on-disk `\r\n`, and
    /// every hunk revert on a CRLF file failed. `core.autocrlf=false` (set in
    /// `setup_repo`) keeps git from rewriting the endings, so the `\r\n` bytes
    /// reach the parser verbatim.
    #[tokio::test]
    async fn revert_hunk_round_trips_a_crlf_file() {
        if !has_git() {
            eprintln!("skipping: git not on PATH");
            return;
        }
        let dir = setup_repo("crlf");
        std::fs::write(dir.join("f.txt"), "line1\r\nline2\r\nline3\r\n").unwrap();
        git(&dir, &["add", "f.txt"]);
        git(&dir, &["commit", "-q", "-m", "init"]);
        std::fs::write(dir.join("f.txt"), "line1\r\nline2-CHANGED\r\nline3\r\n").unwrap();

        let before = diff::diff_file(&dir, "f.txt").await.expect("diff before");
        assert_eq!(before.hunks.len(), 1);
        let hash = diff::hunk_hash(&before.hunks[0]);

        let after = revert_hunk(&dir, "f.txt", 0, &hash)
            .await
            .expect("revert_hunk on CRLF file");
        assert!(
            after.hunks.is_empty(),
            "expected a clean file after revert: {after:?}"
        );
        // Byte-exact: the CRLF endings must be restored, not silently
        // normalized to LF.
        let content = std::fs::read(dir.join("f.txt")).unwrap();
        assert_eq!(content, b"line1\r\nline2\r\nline3\r\n");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Regression (H2): a non-ASCII filename must diff and revert. Git's
    /// default `core.quotePath=true` C-quotes such paths in the `diff --git`
    /// header (`"caf\303\251.txt"`), which `parse_diff_git_line` could not
    /// parse — the whole file section was dropped and the diff pane came up
    /// empty (and un-revertable). `diff_file` now pins `core.quotePath=false`.
    #[tokio::test]
    async fn revert_hunk_round_trips_a_non_ascii_filename() {
        if !has_git() {
            eprintln!("skipping: git not on PATH");
            return;
        }
        let dir = setup_repo("unicode");
        let name = "café.txt";
        std::fs::write(dir.join(name), "a\nb\nc\n").unwrap();
        git(&dir, &["add", name]);
        git(&dir, &["commit", "-q", "-m", "init"]);
        std::fs::write(dir.join(name), "a\nB\nc\n").unwrap();

        let before = diff::diff_file(&dir, name).await.expect("diff before");
        assert_eq!(before.path, name, "path must round-trip unquoted");
        assert_eq!(
            before.hunks.len(),
            1,
            "non-ASCII file must produce a hunk, not be dropped"
        );
        let hash = diff::hunk_hash(&before.hunks[0]);

        let after = revert_hunk(&dir, name, 0, &hash)
            .await
            .expect("revert_hunk on unicode-named file");
        assert!(
            after.hunks.is_empty(),
            "expected a clean file after revert: {after:?}"
        );
        let content = std::fs::read_to_string(dir.join(name)).unwrap();
        assert_eq!(content, "a\nb\nc\n");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Regression (L5): reverting a hunk that touches the unterminated final
    /// line of a file must NOT add a trailing newline. Before the fix,
    /// `build_hunk_patch` dropped the `\ No newline at end of file` marker, so
    /// the reverse patch didn't match the on-disk (newline-less) last line —
    /// the revert either failed or silently appended a byte the user never had.
    #[tokio::test]
    async fn revert_hunk_preserves_missing_trailing_newline() {
        if !has_git() {
            eprintln!("skipping: git not on PATH");
            return;
        }
        let dir = setup_repo("nonewline");
        std::fs::write(dir.join("f.txt"), "a\nb\nc").unwrap(); // no trailing newline
        git(&dir, &["add", "f.txt"]);
        git(&dir, &["commit", "-q", "-m", "init"]);
        std::fs::write(dir.join("f.txt"), "a\nb\nCHANGED").unwrap(); // still unterminated

        let before = diff::diff_file(&dir, "f.txt").await.expect("diff before");
        assert_eq!(before.hunks.len(), 1);
        let hash = diff::hunk_hash(&before.hunks[0]);

        let after = revert_hunk(&dir, "f.txt", 0, &hash)
            .await
            .expect("revert_hunk on no-newline file");
        assert!(
            after.hunks.is_empty(),
            "expected a clean file after revert: {after:?}"
        );
        // Byte-exact: the file must NOT have gained a trailing newline.
        let content = std::fs::read(dir.join("f.txt")).unwrap();
        assert_eq!(content, b"a\nb\nc");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// B6: a stale `hunk_hash` (the caller's cached hash no longer matches
    /// the file's current hunk at that index — an edit raced the diff view)
    /// is refused, and refusal must not touch the file at all.
    #[tokio::test]
    async fn revert_hunk_rejects_stale_hash_and_leaves_file_untouched() {
        if !has_git() {
            eprintln!("skipping: git not on PATH");
            return;
        }
        let dir = setup_repo("stale");
        std::fs::write(dir.join("f.txt"), "line1\nline2\nline3\n").unwrap();
        git(&dir, &["add", "f.txt"]);
        git(&dir, &["commit", "-q", "-m", "init"]);
        std::fs::write(dir.join("f.txt"), "line1\nline2-CHANGED\nline3\n").unwrap();

        let err = revert_hunk(&dir, "f.txt", 0, "not-the-real-hash")
            .await
            .unwrap_err();
        assert!(matches!(err, AppError::Workbench(_)));
        let content = std::fs::read_to_string(dir.join("f.txt")).unwrap();
        assert_eq!(
            content, "line1\nline2-CHANGED\nline3\n",
            "stale-hash refusal must not touch the file"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// B6: an out-of-range hunk index is refused (not a panic) — guards
    /// against a stale frontend cache pointing at a hunk index the file no
    /// longer has (e.g. the diff shrank after a partial edit elsewhere).
    #[tokio::test]
    async fn revert_hunk_rejects_out_of_range_index() {
        if !has_git() {
            eprintln!("skipping: git not on PATH");
            return;
        }
        let dir = setup_repo("oor");
        std::fs::write(dir.join("f.txt"), "line1\nline2\nline3\n").unwrap();
        git(&dir, &["add", "f.txt"]);
        git(&dir, &["commit", "-q", "-m", "init"]);
        std::fs::write(dir.join("f.txt"), "line1\nline2-CHANGED\nline3\n").unwrap();

        let err = revert_hunk(&dir, "f.txt", 5, "irrelevant")
            .await
            .unwrap_err();
        assert!(matches!(err, AppError::Workbench(_)));

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// B6: readonly (mid-merge) refuses the revert outright, before even
    /// looking at the hunk hash.
    #[tokio::test]
    async fn revert_hunk_refuses_during_a_merge_conflict() {
        if !has_git() {
            eprintln!("skipping: git not on PATH");
            return;
        }
        let dir = setup_repo("mergeready");
        git(&dir, &["checkout", "-qb", "trunk"]);
        std::fs::write(dir.join("f.txt"), "base\n").unwrap();
        git(&dir, &["add", "f.txt"]);
        git(&dir, &["commit", "-q", "-m", "base"]);
        git(&dir, &["checkout", "-qb", "side"]);
        std::fs::write(dir.join("f.txt"), "side\n").unwrap();
        git(&dir, &["commit", "-qam", "side change"]);
        git(&dir, &["checkout", "-q", "trunk"]);
        std::fs::write(dir.join("f.txt"), "main\n").unwrap();
        git(&dir, &["commit", "-qam", "main change"]);
        let _ = std::process::Command::new("git")
            .args(["merge", "side"])
            .current_dir(&dir)
            .output();
        assert!(
            diff::readonly(&dir).await,
            "expected the merge to leave a special state"
        );

        let err = revert_hunk(&dir, "f.txt", 0, "irrelevant")
            .await
            .unwrap_err();
        assert!(matches!(err, AppError::Workbench(_)));

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// B6: `send_hunk` formats a fenced block with a `path:line` header the
    /// compose overlay can drop straight into the draft.
    #[tokio::test]
    async fn send_hunk_formats_fenced_block_with_path_and_line() {
        if !has_git() {
            eprintln!("skipping: git not on PATH");
            return;
        }
        let dir = setup_repo("send");
        std::fs::write(dir.join("f.txt"), "line1\nline2\nline3\n").unwrap();
        git(&dir, &["add", "f.txt"]);
        git(&dir, &["commit", "-q", "-m", "init"]);
        std::fs::write(dir.join("f.txt"), "line1\nline2-CHANGED\nline3\n").unwrap();

        let text = send_hunk(&dir, "f.txt", 0).await.expect("send_hunk");
        assert!(text.contains("f.txt:"), "{text}");
        assert!(text.contains("```diff"), "{text}");
        assert!(text.contains("-line2"), "{text}");
        assert!(text.contains("+line2-CHANGED"), "{text}");

        let _ = std::fs::remove_dir_all(&dir);
    }
}
