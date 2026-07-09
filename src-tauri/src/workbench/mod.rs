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
//! `diff_summary`/`diff_file`/`send_hunk`/`revert_hunk`); `shadow`/`worktree`
//! remain near-empty placeholders for Phases C/D.

pub mod diff;
pub mod git;
pub mod shadow;
pub mod worktree;

use std::path::{Path, PathBuf};

use serde::Serialize;
use tauri::{AppHandle, Emitter};
use tokio::sync::broadcast;

use crate::error::{AppError, AppResult};
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

    /// Phase B `workbench_diff_summary`: the file list + readonly/source
    /// flags for the Diff section. Thin pass-through to [`diff::summary`] —
    /// kept as a service method (rather than the IPC layer calling `diff::`
    /// directly) so later phases can add cross-cutting behavior here (e.g.
    /// Phase C's shadow-repo fallback) without touching the IPC signature.
    pub async fn diff_summary(&self, root: &Path) -> AppResult<diff::DiffSummary> {
        diff::summary(root).await
    }

    /// Phase B `workbench_diff_file`: one file's full parsed diff.
    pub async fn diff_file(&self, root: &Path, path: &str) -> AppResult<diff::FileDiff> {
        diff::diff_file(root, path).await
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
    let out =
        git::run_with_stdin(&ctx, &["apply", "--reverse", "--unidiff-zero", "-"], &patch, None).await?;
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
        let out = std::process::Command::new("git").args(args).current_dir(dir).output().expect("git");
        assert!(out.status.success(), "git {args:?} failed: {}", String::from_utf8_lossy(&out.stderr));
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

        let after = revert_hunk(&dir, "f.txt", 0, &hash).await.expect("revert_hunk");
        assert!(after.hunks.is_empty(), "expected a clean file after revert: {after:?}");
        let content = std::fs::read_to_string(dir.join("f.txt")).unwrap();
        assert_eq!(content, "line1\nline2\nline3\n");

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

        let err = revert_hunk(&dir, "f.txt", 0, "not-the-real-hash").await.unwrap_err();
        assert!(matches!(err, AppError::Workbench(_)));
        let content = std::fs::read_to_string(dir.join("f.txt")).unwrap();
        assert_eq!(content, "line1\nline2-CHANGED\nline3\n", "stale-hash refusal must not touch the file");

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

        let err = revert_hunk(&dir, "f.txt", 5, "irrelevant").await.unwrap_err();
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
        let _ = std::process::Command::new("git").args(["merge", "side"]).current_dir(&dir).output();
        assert!(diff::readonly(&dir).await, "expected the merge to leave a special state");

        let err = revert_hunk(&dir, "f.txt", 0, "irrelevant").await.unwrap_err();
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
