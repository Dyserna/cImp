//! The Workbench use cases: the diff view, the checkpoint timeline, the
//! session-commit and git-graph history, and the managed worktrees.
//!
//! ## What the A1 workbench run found
//!
//! Twenty-one commands, one collaborator, and one line of Tauri.
//!
//! The collaborator is [`crate::workbench::WorkbenchService`] — the engine that
//! owns the shadow repo, the burst scheduler and the caches. Every command here
//! is `resolve_workbench_root(root)? → one engine call`, which reads like there
//! was nothing to move. What made these WebView-only was not the bodies: it was
//! that the engine took an `AppHandle`, for ONE `emit(FS_BATCH_EVENT, …)`. So
//! the whole Workbench — every checkpoint, every diff, every restore — was
//! unreachable from a test because of one broadcast. That handle is an
//! [`EventSink`](crate::service::sink::EventSink) now (see the engine's field
//! docs), which is what lets the tests at the foot of this module drive a real
//! checkpoint against a real git repo with no Tauri app anywhere.
//!
//! Two of the twenty-one reach across a domain boundary:
//! [`workbench_session_commits`](crate::ipc::commands::workbench_session_commits)
//! and its `_counts` sibling widen the frontend's `from_ms..=to_ms` with the
//! code graph's own canonical session window. That rule — *the frontend's
//! window is a fallback snapshot, the graph's is fresher, take the union* — is
//! the reason a commit made after the last poll still lands inside its session,
//! and it had no test. It is [`widen`] here, behind
//! [`SessionCommitSource`]: the third narrow host trait, beside
//! [`GraphIndexHost`](crate::service::sink::GraphIndexHost) and
//! [`ChecksLangStats`](crate::service::checks::ChecksLangStats), and for the
//! same reason each of those is one — the capability is reached from ANOTHER
//! domain's use case, and the implementor
//! ([`crate::graph::GraphService`]) cannot be built without a Tauri app, so a
//! concrete handle here would make the widening rule untestable. A trait keeps
//! the root resolved exactly once, inside the method, which a
//! values-computed-at-the-boundary shape could not.
//!
//! ## What did NOT change
//!
//! [`resolve_root`] still absolutizes a relative `root` — see its doc for the
//! `root/root/.cimp/…` doubling that rule prevents — and it is still separate
//! from [`crate::service::project_root`], deliberately: the workbench hands its
//! root to spawned `git` as `current_dir`, and the graph does not.
//! [`diff_context`] still clamps the frontend's "full file" toggle to
//! `diff::MAX_CONTEXT`. And `worktree_create` still takes the tab-lifecycle
//! serializer, for the reason its command always did: two concurrent creates of
//! one slug would otherwise both pass the existence check.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use tokio::sync::Mutex as TokioMutex;

use crate::error::{AppError, AppResult};
use crate::workbench::{diff, history, shadow, worktree, WorkbenchService};

/// V13 Phase A: resolve an optional `root` IPC argument to a project
/// directory, falling back to the app's launch directory.
///
/// A small, deliberate duplicate of [`crate::service::project_root`] (see the
/// rationale in `checks/gitls.rs`'s doc comment for the sibling `run_git`
/// split) — kept separate so `workbench` doesn't couple its root-resolution to
/// `graph`'s, and because of the absolutization the graph's copy does not do:
/// the workbench layer joins sub-paths onto this root AND hands it to spawned
/// `git` as `current_dir`, and git resolves argument paths relative to that
/// same cwd — a relative root would double up (`root/root/.cimp/…`).
pub fn resolve_root(root: Option<String>) -> AppResult<PathBuf> {
    match root {
        Some(r) if !r.trim().is_empty() => {
            let path = PathBuf::from(r);
            if path.is_absolute() {
                Ok(path)
            } else {
                std::env::current_dir()
                    .map(|cwd| cwd.join(path))
                    .map_err(|e| AppError::Settings(format!("cwd: {e}")))
            }
        }
        _ => std::env::current_dir().map_err(|e| AppError::Settings(format!("cwd: {e}"))),
    }
}

/// Clamp a frontend-supplied unified-context width: absent means git's default
/// (3); the "full file" toggle sends a huge value, bounded by
/// [`diff::MAX_CONTEXT`] so the argument can't be arbitrary.
pub fn diff_context(context: Option<u32>) -> u32 {
    context.unwrap_or(diff::DEFAULT_CONTEXT).min(diff::MAX_CONTEXT)
}

/// Widen a frontend-supplied session window with the code graph's own
/// canonical one, when the graph knows this session.
///
/// The frontend's `from_ms..=to_ms` is a snapshot taken at its last poll; the
/// graph's `session` relation is written as the session runs. Taking the UNION
/// (earliest start, latest end) is what keeps a commit made after that poll
/// inside its own session — narrowing to either side alone would drop it.
fn widen(from_ms: i64, to_ms: i64, canonical: Option<&(i64, i64)>) -> (i64, i64) {
    match canonical {
        Some((s, l)) => (from_ms.min(*s), to_ms.max(*l)),
        None => (from_ms, to_ms),
    }
}

/// The code graph's session bookkeeping, as the Session-commits section needs
/// it. See the module docs for why this is a trait rather than a borrowed
/// `GraphService`, and [`crate::service::checks::ChecksLangStats`] for the
/// precedent it follows.
///
/// One trait for three reads because they are one capability with one consumer
/// surface: "what does the graph know about this project's sessions and their
/// commits?" Splitting them would give three traits with the same implementor,
/// the same caller and nothing else to tell them apart.
pub trait SessionCommitSource: Send + Sync {
    /// Commit hashes recorded for one session (git-printed, usually short —
    /// matched by prefix downstream).
    fn recorded_hashes(&self, root: &Path, session_id: &str) -> Vec<String>;

    /// Recorded commit hashes for every session, in one scan.
    fn recorded_hashes_all(&self, root: &Path) -> HashMap<String, Vec<String>>;

    /// The graph's own `(started_ms, last_ms)` window per session.
    fn session_windows(&self, root: &Path) -> HashMap<String, (i64, i64)>;
}

impl SessionCommitSource for Arc<crate::graph::GraphService> {
    fn recorded_hashes(&self, root: &Path, session_id: &str) -> Vec<String> {
        crate::graph::GraphService::session_commit_hashes(self, root, session_id)
    }

    fn recorded_hashes_all(&self, root: &Path) -> HashMap<String, Vec<String>> {
        crate::graph::GraphService::session_commit_hashes_all(self, root)
    }

    fn session_windows(&self, root: &Path) -> HashMap<String, (i64, i64)> {
        crate::graph::GraphService::session_windows(self, root)
    }
}

/// The Workbench use cases, over one borrowed handle — same shape and rationale
/// as [`crate::service::tabs::TabService`].
///
/// Named `WorkbenchUseCases` rather than `WorkbenchService` because the handle
/// it borrows already has that name: `crate::workbench::WorkbenchService` is
/// the engine (shadow repo, caches, schedulers), and this is the list of things
/// the UI can ask of it. The same distinction `service::tabs` draws against
/// `tabs::registry`.
pub struct WorkbenchUseCases<'a> {
    bench: &'a Arc<WorkbenchService>,
}

impl<'a> WorkbenchUseCases<'a> {
    pub fn new(bench: &'a Arc<WorkbenchService>) -> Self {
        Self { bench }
    }

    /// V13 Phase A: the top-of-view banner data — is `git` on PATH at all, and
    /// is `root` inside a working tree.
    pub async fn status(&self, root: Option<String>) -> AppResult<crate::workbench::WorkbenchStatus> {
        let root = resolve_root(root)?;
        Ok(self.bench.status(&root).await)
    }

    /// V13 Phase B: the Diff section's file list.
    pub async fn diff_summary(&self, root: Option<String>) -> AppResult<diff::DiffSummary> {
        let root = resolve_root(root)?;
        self.bench.diff_summary(&root).await
    }

    /// V13 Phase B: one file's full parsed diff (hunks + lines).
    pub async fn diff_file(
        &self,
        root: Option<String>,
        path: &str,
        context: Option<u32>,
    ) -> AppResult<diff::FileDiff> {
        let root = resolve_root(root)?;
        self.bench.diff_file(&root, path, diff_context(context)).await
    }

    /// V13 Phase B B2: revert one hunk, refused on a stale `hunk_hash` or a
    /// mid-merge/-rebase repo. Returns the file's fresh diff.
    pub async fn revert_hunk(
        &self,
        root: Option<String>,
        path: &str,
        hunk_index: usize,
        hunk_hash: &str,
    ) -> AppResult<diff::FileDiff> {
        let root = resolve_root(root)?;
        self.bench
            .revert_hunk(&root, path, hunk_index, hunk_hash)
            .await
    }

    /// V13 Phase B: one hunk as a fenced code block + `path:line` header, for
    /// the compose overlay's "Send to agent".
    pub async fn send_hunk(
        &self,
        root: Option<String>,
        path: &str,
        hunk_index: usize,
    ) -> AppResult<String> {
        let root = resolve_root(root)?;
        self.bench.send_hunk(&root, path, hunk_index).await
    }

    /// V13 Phase C: every checkpoint currently retained in the shadow repo.
    pub async fn checkpoints(&self, root: Option<String>) -> AppResult<Vec<shadow::Checkpoint>> {
        let root = resolve_root(root)?;
        self.bench.checkpoints(&root).await
    }

    /// V13 Phase C: checkpoint `id` vs. the CURRENT working tree.
    pub async fn checkpoint_diff(
        &self,
        root: Option<String>,
        id: &str,
        context: Option<u32>,
    ) -> AppResult<Vec<diff::FileDiff>> {
        let root = resolve_root(root)?;
        self.bench
            .checkpoint_diff(&root, id, diff_context(context))
            .await
    }

    /// V13 Phase C: the manual "Checkpoint now" action — deliberately NOT
    /// throttled by `checkpoint_min_gap_s`.
    pub async fn checkpoint_now(
        &self,
        root: Option<String>,
        label: Option<String>,
    ) -> AppResult<shadow::CheckpointId> {
        let root = resolve_root(root)?;
        self.bench.checkpoint_now(&root, label).await
    }

    /// V13 Phase C: restore the working tree to checkpoint `id`.
    /// **Safety-critical** — see `shadow::restore`'s invariants.
    pub async fn restore(
        &self,
        root: Option<String>,
        id: &str,
        delete_new: bool,
    ) -> AppResult<shadow::RestoreReport> {
        let root = resolve_root(root)?;
        self.bench.restore(&root, id, delete_new).await
    }

    /// V13 Phase D: every cImp-managed worktree of `root`'s repo.
    pub async fn worktrees(&self, root: Option<String>) -> AppResult<Vec<worktree::WorktreeInfo>> {
        let root = resolve_root(root)?;
        self.bench.worktrees(&root).await
    }

    /// V13 Phase D D3: worktree `slug` vs. the base branch it was cut from.
    pub async fn worktree_diff(
        &self,
        root: Option<String>,
        slug: &str,
        context: Option<u32>,
    ) -> AppResult<Vec<diff::FileDiff>> {
        let root = resolve_root(root)?;
        self.bench
            .worktree_diff(&root, slug, diff_context(context))
            .await
    }

    /// The Session-commits section: commits caught live from the transcript
    /// unioned with commits whose committer time falls inside the session's
    /// window, widened by the graph's canonical window — see [`widen`].
    pub async fn session_commits(
        &self,
        root: Option<String>,
        session_id: &str,
        from_ms: i64,
        to_ms: i64,
        sessions: &dyn SessionCommitSource,
    ) -> AppResult<history::SessionCommits> {
        let root = resolve_root(root)?;
        let recorded = sessions.recorded_hashes(&root, session_id);
        let (from_ms, to_ms) = widen(
            from_ms,
            to_ms,
            sessions.session_windows(&root).get(session_id),
        );
        self.bench
            .session_commits(&root, from_ms, to_ms, &recorded)
            .await
    }

    /// Per-session commit counts for the Sessions card's per-row button. Each
    /// window is widened with the graph's canonical one, exactly as
    /// [`Self::session_commits`] widens the single window it is given.
    pub async fn session_commit_counts(
        &self,
        root: Option<String>,
        mut windows: Vec<history::SessionWindow>,
        sessions: &dyn SessionCommitSource,
    ) -> AppResult<HashMap<String, u32>> {
        let root = resolve_root(root)?;
        let recorded = sessions.recorded_hashes_all(&root);
        let canonical = sessions.session_windows(&root);
        for w in &mut windows {
            let (from_ms, to_ms) = widen(w.from_ms, w.to_ms, canonical.get(&w.session_id));
            w.from_ms = from_ms;
            w.to_ms = to_ms;
        }
        self.bench
            .session_commit_counts(&root, &windows, &recorded)
            .await
    }

    /// One commit vs. its first parent — the expanded-commit file list.
    pub async fn commit_diff(
        &self,
        root: Option<String>,
        hash: &str,
        context: Option<u32>,
    ) -> AppResult<Vec<diff::FileDiff>> {
        let root = resolve_root(root)?;
        self.bench
            .commit_diff(&root, hash, diff_context(context))
            .await
    }

    /// The Git-graph section: up to `limit` commits from every ref in
    /// topological order, plus the current branch name.
    pub async fn git_graph(
        &self,
        root: Option<String>,
        limit: Option<usize>,
    ) -> AppResult<history::GitGraph> {
        let root = resolve_root(root)?;
        self.bench.git_graph(&root, limit.unwrap_or(500)).await
    }

    /// V13 Phase D: create a bare worktree (no tab) for `slug`. Returns its
    /// absolute path.
    ///
    /// `serializer` is the tab-lifecycle serializer `create_ai_tab_in_worktree`
    /// holds, and it is a parameter rather than a field for the same reason
    /// [`GraphIndexHost`](crate::service::sink::GraphIndexHost) is one on
    /// `SettingsService::update`: exactly one method needs it, and a field
    /// would force the other twenty callers to have one. Two concurrent
    /// creates for one slug could otherwise both pass `worktree::create`'s
    /// existence check before either runs `git worktree add` (git's own locking
    /// makes the loser fail, but with an opaque "branch already exists" instead
    /// of the typed duplicate-slug error).
    pub async fn worktree_create(
        &self,
        serializer: &TokioMutex<()>,
        root: Option<String>,
        slug: &str,
    ) -> AppResult<String> {
        let _serializer = serializer.lock().await;
        let root = resolve_root(root)?;
        let path = self.bench.worktree_create(&root, slug).await?;
        Ok(path.display().to_string())
    }

    /// V13 Phase D: merge worktree `slug`'s branch back into its base.
    /// **Safety-critical** — see `workbench::worktree::merge`.
    pub async fn worktree_merge(
        &self,
        root: Option<String>,
        slug: &str,
    ) -> AppResult<worktree::MergeReport> {
        let root = resolve_root(root)?;
        self.bench.worktree_merge(&root, slug).await
    }

    /// V13 Phase D: remove worktree `slug`'s directory and delete its branch.
    pub async fn worktree_discard(&self, root: Option<String>, slug: &str) -> AppResult<()> {
        let root = resolve_root(root)?;
        self.bench.worktree_discard(&root, slug).await
    }

    /// V13 Phase D D3: the merge-readiness chip's "Run checks" action.
    pub async fn worktree_run_checks(
        &self,
        root: Option<String>,
        slug: &str,
    ) -> AppResult<crate::workbench::WorktreeCheckStatus> {
        let root = resolve_root(root)?;
        self.bench.worktree_run_checks(&root, slug).await
    }

    /// V13 Phase D D3: the last cached check result for `slug`, if any —
    /// `None` means "not checked yet", not a failure.
    pub fn worktree_check_status(
        &self,
        root: Option<String>,
        slug: &str,
    ) -> AppResult<Option<crate::workbench::WorktreeCheckStatus>> {
        let root = resolve_root(root)?;
        Ok(self.bench.worktree_check_status(&root, slug))
    }
}

/// V33 step 5: the contamination lifecycle the Workbench Timeline renders
/// beside its checkpoints, plus the root those checkpoints belong to.
///
/// Free rather than a [`WorkbenchUseCases`] method: it reads the offload
/// outbound store, not the workbench engine, so a service would be a handle it
/// never touches. It resolves the root through [`resolve_root`] anyway, because
/// the root it returns is the Timeline's — and the Timeline is a workbench
/// surface.
pub async fn contamination_events(root: Option<String>) -> AppResult<serde_json::Value> {
    let root = crate::activity::root_key(&resolve_root(root)?);
    let events =
        crate::service::on_blocking_pool(crate::offload::outbound::contamination_events).await?;
    Ok(serde_json::json!({ "root": root, "events": events }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::service::sink::testing::RecordingEventSink;
    use crate::settings::{Settings, SettingsHandle};

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

    /// A throwaway project directory with a real user git repo in it — the same
    /// fixture shape `workbench::shadow`'s own tests use, so a checkpoint here
    /// runs against exactly what a user's project looks like.
    struct ScratchRepo(PathBuf);

    impl ScratchRepo {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!("cimp-wbsvc-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir_all(&path).expect("scratch dir");
            git(&path, &["init", "-q"]);
            git(&path, &["config", "user.email", "user@example.com"]);
            git(&path, &["config", "user.name", "User"]);
            git(&path, &["config", "core.autocrlf", "false"]);
            std::fs::write(path.join("tracked.txt"), "hello\n").expect("write");
            std::fs::write(path.join(".gitignore"), ".cimp/\n").expect("write");
            git(&path, &["add", "tracked.txt", ".gitignore"]);
            git(&path, &["commit", "-q", "-m", "init"]);
            Self(path)
        }

        fn arg(&self) -> Option<String> {
            Some(self.0.to_string_lossy().into_owned())
        }
    }

    impl Drop for ScratchRepo {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// The engine, built with a recording sink instead of an app handle — the
    /// whole point of the Phase A change to its constructor.
    fn engine(root: &Path) -> Arc<WorkbenchService> {
        let defaults = Settings::default();
        let settings = SettingsHandle::new(defaults.clone(), defaults, root.to_path_buf());
        WorkbenchService::new(Arc::new(RecordingEventSink::default()), settings)
    }

    /// **Previously "user clicks in the app".** The Timeline's core promise:
    /// press *Checkpoint now*, edit a file, and the checkpoint still holds the
    /// content that was there when it was taken — which is what a restore later
    /// rests on. Every step of this was WebView-only until the engine stopped
    /// taking an `AppHandle`.
    #[tokio::test]
    async fn a_manual_checkpoint_captures_the_tree_and_diffs_against_later_edits() {
        if !has_git() {
            return;
        }
        let repo = ScratchRepo::new();
        let bench = engine(&repo.0);
        let svc = WorkbenchUseCases::new(&bench);

        assert!(
            svc.checkpoints(repo.arg()).await.expect("checkpoints").is_empty(),
            "a project that has never checkpointed has an empty timeline, not an error"
        );

        std::fs::write(repo.0.join("tracked.txt"), "hello\nworld\n").expect("write");
        let id = svc
            .checkpoint_now(repo.arg(), Some("from a test".to_string()))
            .await
            .expect("checkpoint now");

        let rows = svc.checkpoints(repo.arg()).await.expect("checkpoints");
        assert_eq!(rows.len(), 1, "the manual checkpoint is on the timeline");
        assert_eq!(rows[0].id, id);

        // Edit past the checkpoint: "diff vs now" must see the file, because
        // the checkpoint holds the OLD content.
        std::fs::write(repo.0.join("tracked.txt"), "hello\nworld\nagain\n").expect("write");
        let files = svc
            .checkpoint_diff(repo.arg(), &id, None)
            .await
            .expect("checkpoint diff");
        assert!(
            files.iter().any(|f| f.path.ends_with("tracked.txt")),
            "the edit made after the checkpoint shows in its diff-vs-now: {files:#?}"
        );
    }

    /// **Previously "user clicks in the app".** The Diff section against a real
    /// working tree, plus the "full file" toggle's clamp — a frontend can ask
    /// for any context width, and `diff::MAX_CONTEXT` is what stops it being
    /// arbitrary.
    #[tokio::test]
    async fn the_diff_view_sees_a_working_tree_edit_at_any_requested_context() {
        if !has_git() {
            return;
        }
        let repo = ScratchRepo::new();
        let bench = engine(&repo.0);
        let svc = WorkbenchUseCases::new(&bench);

        let status = svc.status(repo.arg()).await.expect("status");
        assert!(status.is_repo, "the fixture is a git working tree");

        assert!(
            svc.diff_summary(repo.arg())
                .await
                .expect("summary")
                .files
                .is_empty(),
            "a clean tree has no changed files"
        );

        std::fs::write(repo.0.join("tracked.txt"), "hello\nchanged\n").expect("write");
        let summary = svc.diff_summary(repo.arg()).await.expect("summary");
        assert!(
            summary.files.iter().any(|f| f.path.ends_with("tracked.txt")),
            "the edit is listed: {summary:#?}"
        );

        // The "full file" toggle's huge value is clamped, not passed through.
        assert_eq!(diff_context(None), diff::DEFAULT_CONTEXT);
        assert_eq!(diff_context(Some(u32::MAX)), diff::MAX_CONTEXT);
        let file = svc
            .diff_file(repo.arg(), "tracked.txt", Some(u32::MAX))
            .await
            .expect("diff file");
        assert!(!file.hunks.is_empty(), "the changed file has a hunk");
    }

    /// A stand-in for the code graph's session bookkeeping. `window` is what
    /// the graph would say it knows about `session`; `None` is a session the
    /// graph has never seen, which is the case the frontend's own snapshot has
    /// to carry alone.
    struct FakeSessions {
        session: String,
        window: Option<(i64, i64)>,
    }

    impl SessionCommitSource for FakeSessions {
        fn recorded_hashes(&self, _root: &Path, _session_id: &str) -> Vec<String> {
            Vec::new()
        }

        fn recorded_hashes_all(&self, _root: &Path) -> HashMap<String, Vec<String>> {
            HashMap::new()
        }

        fn session_windows(&self, _root: &Path) -> HashMap<String, (i64, i64)> {
            match self.window {
                Some(w) => [(self.session.clone(), w)].into_iter().collect(),
                None => HashMap::new(),
            }
        }
    }

    /// **Previously "user clicks in the app".** The union is not cosmetic: with
    /// only the frontend's stale window, a commit made since its last poll is
    /// attributed to no session at all; with the graph's window merged in, the
    /// same call finds it. Both halves run against a real repo.
    #[tokio::test]
    async fn a_commit_outside_the_frontends_window_is_found_via_the_graphs() {
        if !has_git() {
            return;
        }
        let repo = ScratchRepo::new();
        let bench = engine(&repo.0);
        let svc = WorkbenchUseCases::new(&bench);
        let session = "s-1".to_string();

        // The frontend's snapshot predates the fixture's commit entirely.
        let stale = svc
            .session_commits(
                repo.arg(),
                &session,
                0,
                1,
                &FakeSessions {
                    session: session.clone(),
                    window: None,
                },
            )
            .await
            .expect("session commits");
        assert!(
            stale.commits.is_empty(),
            "nothing is in a window that ended in 1970: {stale:#?}"
        );

        // The graph knows the session is still running, so the union reaches
        // the commit the stale snapshot missed.
        let now_ms = crate::activity::now_ms() as i64;
        let widened = svc
            .session_commits(
                repo.arg(),
                &session,
                0,
                1,
                &FakeSessions {
                    session: session.clone(),
                    window: Some((0, now_ms + 3_600_000)),
                },
            )
            .await
            .expect("session commits");
        assert_eq!(
            widened.commits.len(),
            1,
            "the fixture's one commit lands inside the widened window: {widened:#?}"
        );
    }

    /// The session-window union rule, which decides whether a commit made after
    /// the frontend's last poll is attributed to its own session. It had no
    /// test at all while it lived inline in two commands.
    #[test]
    fn the_graphs_session_window_widens_the_frontends_in_both_directions() {
        // No canonical window: the frontend's snapshot stands.
        assert_eq!(widen(100, 200, None), (100, 200));
        // The graph knows the session started earlier and is still running.
        assert_eq!(widen(100, 200, Some(&(50, 500))), (50, 500));
        // Never NARROWS: a canonical window inside the frontend's leaves it be.
        assert_eq!(widen(100, 200, Some(&(150, 160))), (100, 200));
    }

    /// The absolutization rule: a relative `root` is joined onto the launch cwd
    /// here, at the boundary, because the layer below hands it to spawned `git`
    /// as `current_dir` and git resolves argument paths against that same cwd.
    #[test]
    fn a_relative_root_is_absolutized_and_a_blank_one_falls_back() {
        let cwd = std::env::current_dir().expect("cwd");
        assert_eq!(resolve_root(None).expect("none"), cwd);
        assert_eq!(resolve_root(Some("   ".into())).expect("blank"), cwd);
        assert_eq!(resolve_root(Some("sub/dir".into())).expect("rel"), cwd.join("sub/dir"));
        let abs = cwd.join("already");
        assert_eq!(
            resolve_root(Some(abs.to_string_lossy().into_owned())).expect("abs"),
            abs
        );
    }
}
