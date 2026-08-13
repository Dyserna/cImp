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
    /// Phase C §C3: the `checkpoint_min_gap_s` gate every AUTOMATIC
    /// checkpoint trigger (prompt-tap, burst) shares, plus the background
    /// snapshot job it admits. See [`CheckpointScheduler`] for the key it
    /// throttles on and why it is a struct of its own rather than a bare map
    /// field here.
    checkpoints: CheckpointScheduler,
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

/// The bucket [`CheckpointScheduler`]'s min-gap gate throttles on: a canonical
/// project root ([`git::canonical_path`], so two spellings of one project share
/// a bucket) PLUS the [`shadow::Origin::tab`] the checkpoint belongs to.
///
/// **`None` is one shared bucket, not a bypass and not a per-caller bucket.**
/// The only automatic trigger that reaches the gate without a tab is the burst
/// trigger ([`WorkbenchService::handle_fs_batch_for_burst`] — it sees an
/// `FsBatch`, never a conversation); the manual and pre-restore triggers do not
/// go through the gate at all. Giving the tab-less callers their own bucket is
/// what keeps a busy tab from starving the burst trigger, which exists
/// precisely to cover the edits no tab's prompt hook can see (a shell tab, an
/// external editor) — folding them in with a tab, or with each other per
/// trigger, would either silence that fallback or exempt filesystem noise from
/// the one gate that debounces it. Bursts therefore still throttle each other,
/// which is the whole point of the gate for them.
///
/// **Only the tab, not the session.** A tab's session id rolls over (`/clear`,
/// a restart) while the tab stays the same, so keying on the session too would
/// mint an unbounded stream of new buckets over a long-lived tab — a slow
/// throttle leak, and unnecessary: sessions within one tab are sequential, so
/// the tab already names the concurrency the owner asked to separate.
///
/// **V33 Phase F: the TOOL trigger keeps this exact bucket — locked, and not a
/// default.** The milestone's 2026-08-09 amendment posed "should a tool-sourced
/// checkpoint throttle per tab, or per tool call?" as an open question; the
/// answer is *keep the tab bucket*, for the reason the bucket exists at all.
/// Per-tab attribution IS the feature: the Timeline has to answer "which
/// checkpoint was live when THIS tab went bad", and a bucket shared across tabs
/// lets a busy tab starve a quiet one's pre-tool checkpoint — the tool call that
/// broke the tree then has no snapshot of its own to rewind to. Per-CALL (no
/// throttle) was the other candidate and is rejected: `checkpoint_min_gap_s`
/// exists because a shadow-repo snapshot is a `git add -A` over the whole work
/// tree, and an agent's edit bursts arrive several per second.
///
/// The accepted cost is stated rather than hidden: inside one min-gap window a
/// tab gets ONE checkpoint whichever trigger claimed it, so a tool call
/// following its own turn's prompt checkpoint by less than the gap takes none —
/// and correctly so, because [`shadow::snapshot`]'s dedup would have handed it
/// that same tree anyway whenever nothing had changed in between.
type CheckpointKey = (PathBuf, Option<String>);

/// Everything one snapshot job needs out of settings, read once by
/// [`WorkbenchService::maybe_snapshot`] and handed to the scheduler — so the
/// scheduler itself has no dependency on `SettingsHandle` or `AppHandle` and
/// can be exercised directly against a real shadow repo in tests.
#[derive(Clone, Debug)]
struct SnapshotParams {
    /// `workbench.checkpoint_min_gap_s`, as a `Duration`.
    min_gap: Duration,
    /// `graph.ignore` — forwarded to [`shadow::snapshot`]'s `extra_ignore`.
    extra_ignore: Vec<String>,
    /// `graph.max_file_bytes`.
    max_file_bytes: u64,
    /// `workbench.checkpoint_max`, for the opportunistic [`shadow::gc`].
    checkpoint_max: u32,
    /// `workbench.checkpoint_max_age_days`, likewise.
    checkpoint_max_age_days: u32,
    /// **The pre-tool budget** (2026-08-13 amendment), forwarded to
    /// [`shadow::snapshot_detailed`]'s `deadline`. `None` for every trigger
    /// whose caller waits indefinitely; `Some` only for the two out-of-process
    /// Phase F seams, whose wait on the app is bounded because the agent's tool
    /// runs the moment they stop waiting. See [`WorkbenchService::on_tool`].
    deadline: Option<Instant>,
}

/// The automatic-checkpoint min-gap gate and the background snapshot it
/// admits.
///
/// **Split out of [`WorkbenchService`] deliberately.** The service owns a Tauri
/// `AppHandle`, and this crate builds `tauri` without its `test` feature, so
/// there is no way to construct one in a unit test; a gate living directly on
/// the service could only ever be "tested" by re-implementing its keying inside
/// the test, which would keep passing after the real gate regressed. Everything
/// the gate needs (`last`, plus [`SnapshotParams`]) lives here instead, so the
/// tests below drive the *real* gate composed with the *real*
/// [`shadow::snapshot`] against a temp repo.
#[derive(Default)]
struct CheckpointScheduler {
    /// Last checkpoint time per [`CheckpointKey`]. No entry for a key until
    /// its first automatic OR manual snapshot fires.
    last: Mutex<HashMap<CheckpointKey, Instant>>,
}

impl CheckpointScheduler {
    /// Opportunistic eviction bound for [`Self::last`], mirroring
    /// [`WorkbenchService::BURST_STATE_MAX_AGE`]'s reasoning — but note the
    /// eviction below uses `max(min_gap, this)`, which makes it provably
    /// decision-neutral: an evicted entry is by construction older than
    /// `min_gap`, so it could only ever have ADMITTED the next checkpoint
    /// anyway. It exists purely so a bucket for a tab the user has since
    /// deleted from their config, or a project root closed hours ago, does not
    /// sit in the map for the rest of the app's lifetime.
    const ENTRY_MAX_AGE: Duration = Duration::from_secs(3600);

    /// The bucket `origin` checkpoints into on `root`. Canonicalizes the root
    /// here, once, so every writer (prompt-tap, burst, manual, restore) keys
    /// the same way no matter how its caller spelled the path.
    fn key(root: &Path, origin: &shadow::Origin) -> CheckpointKey {
        (git::canonical_path(root), origin.tab.clone())
    }

    /// The gate: `true` (and the bucket is stamped `now`) when `key` has not
    /// checkpointed within `min_gap`, `false` when it has. Check and stamp are
    /// one critical section, so two prompts racing on one tab can never both
    /// pass.
    fn admit(&self, key: CheckpointKey, min_gap: Duration) -> bool {
        let mut last = self.last.lock().unwrap_or_else(|e| e.into_inner());
        last.retain(|_, t| t.elapsed() < min_gap.max(Self::ENTRY_MAX_AGE));
        if last.get(&key).is_some_and(|prev| prev.elapsed() < min_gap) {
            return false;
        }
        last.insert(key, Instant::now());
        true
    }

    /// Stamp `origin`'s bucket without gating anything — for the triggers that
    /// deliberately bypass the gate but should still start their own bucket's
    /// cooldown ([`WorkbenchService::checkpoint_now`] and
    /// [`WorkbenchService::restore`], both `Origin::default()`, i.e. the
    /// tab-less bucket).
    fn record(&self, root: &Path, origin: &shadow::Origin) {
        self.last
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(Self::key(root, origin), Instant::now());
    }

    /// Gate, then run the snapshot (+ opportunistic gc) on a background task
    /// so no caller ever blocks on a `git` round trip. `None` when the gate
    /// throttled this trigger; otherwise the spawned task's handle, which
    /// production drops (fire-and-forget — a dropped tauri `JoinHandle`
    /// detaches, it does not abort) and the tests await so they can assert on
    /// the shadow repo afterwards deterministically.
    ///
    /// **The task resolves to whether the trigger RAN TO COMPLETION** — `true`
    /// for a checkpoint created and for a dedup hit (both are settled answers
    /// about the tree), `false` when the snapshot was abandoned against a
    /// pre-tool budget or failed outright. Only [`WorkbenchService::on_tool`]
    /// reads it; every other caller drops the handle exactly as before.
    fn spawn_if_due(
        &self,
        root: &Path,
        label: String,
        trigger: shadow::Trigger,
        origin: shadow::Origin,
        params: SnapshotParams,
    ) -> Option<tauri::async_runtime::JoinHandle<bool>> {
        let key = Self::key(root, &origin);
        if !self.admit(key.clone(), params.min_gap) {
            return None;
        }
        // `key.0` is the already-canonicalized root — snapshot the same
        // spelling the gate keyed on.
        let root = key.0;
        Some(tauri::async_runtime::spawn(async move {
            let started = Instant::now();
            match shadow::snapshot_detailed(
                &root,
                &label,
                trigger,
                &origin,
                &params.extra_ignore,
                params.max_file_bytes,
                params.deadline,
            )
            .await
            {
                // The pre-tool budget expired, so NOTHING was written and there
                // is no id — see `shadow::SnapshotOutcome::Abandoned`. This is
                // the one outcome with no trace of its own in the shadow repo,
                // which is exactly why it gets a row: "the checkpoint that
                // should precede this edit does not exist" is a fact the user
                // needs at the moment they go looking for it, and its absence
                // is otherwise indistinguishable from the seam never firing.
                Ok(shadow::SnapshotOutcome::Abandoned) => {
                    warn!(
                        root = %root.display(),
                        source = origin.source.as_deref().unwrap_or("(none)"),
                        ms = started.elapsed().as_millis() as u64,
                        "workbench: pre-tool checkpoint abandoned — the snapshot could not finish \
                         inside the caller's budget, so no checkpoint claims to precede this call"
                    );
                    crate::activity::record_bg(checkpoint_miss_row(
                        &root,
                        &origin,
                        started.elapsed(),
                    ));
                    false
                }
                Ok(outcome) => {
                    // V33 Phase F: a dedup hit hands back a checkpoint this
                    // caller did not create — possibly another tab's. Logged at
                    // debug so a live-verify can tell "the tool trigger fired
                    // and there was nothing new to capture" apart from "the tool
                    // trigger never fired", which used to look identical from
                    // outside. NOTHING here claims the id.
                    if !outcome.created() {
                        tracing::debug!(
                            root = %root.display(),
                            trigger = ?trigger,
                            existing = outcome.id().unwrap_or("(none)"),
                            "workbench: checkpoint deduped — work tree unchanged, nothing created"
                        );
                    }
                    if let Err(e) =
                        shadow::gc(&root, params.checkpoint_max, params.checkpoint_max_age_days)
                            .await
                    {
                        warn!(root = %root.display(), error = %e, "workbench: checkpoint gc failed");
                    }
                    true
                }
                Err(e) => {
                    warn!(root = %root.display(), error = %e, "workbench: automatic checkpoint failed");
                    false
                }
            }
        }))
    }
}

/// The Activity row an abandoned pre-tool checkpoint writes — **the consumer of
/// the 2026-08-13 amendment's "the miss is surfaced" half.**
///
/// # Why an Activity row, and why this one
///
/// It is the mechanism this codebase already uses for a degraded-but-not-fatal
/// harness-side fact: `loopback::contract_drift_row` writes a hook shim's
/// payload drift the same way — [`ActivityKind::Graph`](crate::activity::ActivityKind),
/// a distinctive `source`/`tool` pair, `ok: false` so the Events feed flags it,
/// and no root when the fact is not about a project. The alternatives were
/// considered and rejected: a `tracing::warn!` alone has no user-visible
/// consumer that survives (the same reasoning that gave `offload_server` its own
/// kind — a log ring that a restart clears is not a record), and an
/// `injection_flag` [`Screen`](crate::offload::outbound::Screen) would be the
/// wrong vocabulary — nothing was screened, refused or detected.
///
/// # Volume
///
/// Bounded by the checkpoint throttle itself: at most one snapshot per
/// `checkpoint_min_gap_s` per `(root, tab)` can even be attempted, so at most
/// one miss per window per tab. It is additionally self-limiting — a miss
/// requires a snapshot that overran a ~2 s budget — so unlike the drift report
/// (which one broken payload fires on every hook invocation) this needs no
/// doubling ledger of its own.
///
/// A pure function, returning the record rather than recording it, for the
/// reason `contract_drift_row` documents: `activity::record_bg` has no
/// `cfg(test)` diversion, so a row written inside the task is unobservable to
/// the suite.
fn checkpoint_miss_row(
    root: &Path,
    origin: &shadow::Origin,
    waited: Duration,
) -> crate::activity::ActivityRecord {
    // The tool call the missing checkpoint would have preceded. Composed
    // app-side as `harness:tool_name` (`loopback::handle_tool_checkpoint`), so
    // it is not a caller-supplied string; `(unknown)` can only mean a
    // non-tool trigger reached here, which no `None` deadline can produce.
    let source = origin.source.as_deref().unwrap_or("(unknown)");
    let ms = waited.as_millis() as u64;
    crate::activity::ActivityRecord {
        entry: crate::activity::ActivityEntry::new(
            crate::activity::ActivityKind::Graph,
            crate::activity::now_ms(),
            crate::activity::root_key(root),
            "workbench".to_string(),
            "checkpoint_missed".to_string(),
            source.to_string(),
            0,
            ms,
            // Never "ok": the whole point of the row is that a guarantee this
            // feature advertises was not met for this call.
            false,
            match origin.tab.as_deref() {
                // The tab has already been narrowed to a CONFIGURED one by
                // `loopback::checkpoint_identity` (an unrecognised id degrades
                // to `None` there, never to another tab), so `Tab` is a fact
                // here rather than a caller's claim.
                Some(tab) => crate::activity::Attribution::Tab(tab.to_string()),
                // Not `Headless`: `None` collapses "a worker task with no tab"
                // together with "the id named no configured tab", and this
                // writer cannot tell those apart — so it says it does not know.
                None => crate::activity::Attribution::Unattributed,
            },
            origin.session.clone(),
        ),
        request: format!(
            "pre-tool checkpoint for {source} abandoned after {ms} ms — the shadow-repo snapshot \
             did not finish inside the calling hook's budget, so no checkpoint is claimed to \
             precede this call (a checkpoint that might contain the change it claims to predate \
             would silently mislead a restore). The tool call itself was never blocked."
        ),
        response: String::new(),
    }
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
            checkpoints: CheckpointScheduler::default(),
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
    /// `checkpoint_min_gap_s` against the TAB-LESS bucket (see
    /// [`CheckpointKey`]), so consecutive bursts still debounce each other —
    /// but, since V33's per-`(root, tab)` keying, a burst is no longer
    /// debounced by a prompt-tap snapshot moments earlier, nor vice versa. That
    /// is the intended direction of the change: the burst trigger's whole job
    /// is to catch the edits no tab's prompt hook sees, which it can only do if
    /// a busy tab cannot starve it.
    async fn handle_fs_batch_for_burst(&self, batch: FsBatch) {
        let cfg = self.settings.current();
        if !cfg.workbench.checkpoints {
            return;
        }
        // Normalize the root to a single canonical form: `batch.root` arrives
        // as a `root.display()` string round-trip, while every other caller
        // passes a raw `&Path`. Without normalizing, the two representations of
        // the same project become distinct `burst_state` keys and the rolling
        // window silently splits in two. (`CheckpointScheduler` canonicalizes
        // its own key, so the min-gap gate no longer depends on this — but
        // `burst_state`, right below, still does.)
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
            // No origin: a burst is filesystem activity, with no conversation
            // behind it — this handler sees an `FsBatch`, not a prompt.
            let _ = self
                .maybe_snapshot(
                    &root,
                    "activity".to_string(),
                    shadow::Trigger::Burst,
                    shadow::Origin::default(),
                    // No budget: nothing is waiting on a burst snapshot.
                    None,
                )
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
    /// (prompt-tap, burst). Enforces `checkpoint_min_gap_s`, then does the
    /// actual `shadow::snapshot` + opportunistic `shadow::gc` on a background
    /// task so the caller (a prompt hook, an fs-batch handler) never blocks on
    /// it — per the milestone's "never block a prompt or the UI thread"
    /// contract (C2). No-op entirely when checkpoints are off.
    ///
    /// **The throttle is per `(project root, tab)`** — see [`CheckpointKey`]
    /// for the exact bucket, including what a trigger with no tab keys on. It
    /// was per project root alone until V33: two AI tabs on one root shared one
    /// cooldown, so a prompt in tab B inside tab A's window produced no
    /// checkpoint at all and B's identity was simply missing from the Timeline
    /// for that window — which the contamination correlation this milestone
    /// builds needs, since it answers "which checkpoint was live when THIS tab
    /// went bad". The accepted cost, decided with the trade-off on the table:
    /// shadow-repo write volume now scales with the number of ACTIVE tabs, and
    /// two tabs editing one working tree interleave their checkpoints, so
    /// restoring one tab's checkpoint can roll back the other tab's work.
    ///
    /// **Per-tab throttling does NOT mean every tab gets its own checkpoint.**
    /// The gate is only the first of two filters. `shadow::snapshot`'s dedup is
    /// the second: when the working tree is byte-identical to the last
    /// checkpoint's tree it commits nothing and hands back that EXISTING
    /// checkpoint, and (by design — see `shadow::snapshot`'s doc comment) does
    /// not relabel it with the current caller's identity. So a tab whose prompt
    /// arrives with no file changes since the previous checkpoint still ends up
    /// with no checkpoint of its own, no matter how long the gap has been; what
    /// this change guarantees is narrower and is the guarantee the feature
    /// actually needs — **a tab whose prompt follows real file changes always
    /// gets its own labelled checkpoint, even inside another tab's cooldown.**
    /// That is correct rather than a gap: a checkpoint names a tree STATE, and
    /// an identical tree needs no second snapshot to be restorable.
    ///
    /// **Returns the spawned task's handle** (`None` when checkpoints are off
    /// or the gate throttled this trigger). Every pre-V33 caller drops it —
    /// fire-and-forget is the contract on the prompt path. V33 Phase F's tool
    /// trigger AWAITS it; see [`on_tool`](Self::on_tool) for why that one
    /// caller is different.
    ///
    /// `deadline` is the caller's pre-tool budget — `None` for every trigger
    /// that nothing is waiting on. See [`SnapshotParams::deadline`].
    async fn maybe_snapshot(
        &self,
        root: &Path,
        label: String,
        trigger: shadow::Trigger,
        origin: shadow::Origin,
        deadline: Option<Instant>,
    ) -> Option<tauri::async_runtime::JoinHandle<bool>> {
        if !self.checkpoints_enabled() {
            return None;
        }
        let cfg = self.settings.current();
        let params = SnapshotParams {
            min_gap: Duration::from_secs(cfg.workbench.checkpoint_min_gap_s as u64),
            extra_ignore: cfg.graph.ignore.clone(),
            max_file_bytes: cfg.graph.max_file_bytes,
            checkpoint_max: cfg.workbench.checkpoint_max,
            checkpoint_max_age_days: cfg.workbench.checkpoint_max_age_days,
            deadline,
        };
        // The handle is returned, not awaited: a dropped tauri `JoinHandle`
        // detaches rather than aborting, so a caller that drops it gets exactly
        // the pre-V33 fire-and-forget behaviour.
        self.checkpoints
            .spawn_if_due(root, label, trigger, origin, params)
    }

    /// Phase C §C3 prompt-tap trigger: called from `offload/loopback.rs`'s
    /// `/context/retrieve` handler for EVERY prompt (Claude's
    /// `UserPromptSubmit` shim, the OpenCode `chat.message` plugin) —
    /// deliberately BEFORE that handler's own `context_injection` gate, so
    /// checkpointing runs even when injection is off or yields nothing (the
    /// milestone's Decision 4: checkpointing is decoupled from the injection
    /// toggle even though it reuses the same transport). `prompt_head` is used
    /// as-is for the label — callers are expected to have already truncated it
    /// (`shadow::snapshot`'s own [`truncate_label`](shadow) is a hard backstop,
    /// not the primary cut).
    ///
    /// `origin` carries the conversation identity the Timeline needs in order
    /// to be joined to a `Screen::Contamination` activity row: the harness name
    /// the shim called itself, the harness SESSION id, and the cImp TAB id the
    /// route resolved. The agent name alone was never enough — it is the
    /// harness *kind*, shared by every tab of that kind, so two Claude tabs on
    /// one project root were indistinguishable in the checkpoint stream and
    /// "nearest preceding checkpoint" could hand the user the other tab's row.
    ///
    /// **Still infallible and still non-blocking**, which the loopback's
    /// fire-and-forget call site depends on: this returns `()`, the real work
    /// runs on a background task inside
    /// [`maybe_snapshot`](Self::maybe_snapshot), and the added fields are plain
    /// data validated where they are written (`shadow::trailer_identity`), so a
    /// malformed identity degrades to an absent one — never to an error on the
    /// prompt path.
    pub async fn on_prompt(&self, root: &Path, origin: shadow::Origin, prompt_head: &str) {
        let label = format!("prompt: {prompt_head}");
        let _ = self
            .maybe_snapshot(root, label, shadow::Trigger::Prompt, origin, None)
            .await;
    }

    /// **V33 Phase F — the pre-tool checkpoint trigger.** Called immediately
    /// before a filesystem-mutating tool call, from the three fire seams:
    /// the offload worker's own dispatch
    /// ([`offload::tools::dispatch`](crate::offload::tools::dispatch)), the
    /// Claude `PreToolUse` shim and the OpenCode `tool.execute.before` plugin
    /// hook — the last two arriving over the loopback's
    /// `/workbench/tool_checkpoint` route.
    ///
    /// `source` is `harness:tool_name` (`claude:Bash`, `offload:run_command`,
    /// `opencode:edit`). It is recorded on the checkpoint as its own trailer so
    /// the Timeline can attribute damage to the exact call and rewind to just
    /// before it, and it becomes the label as well so a row reads meaningfully
    /// in a build whose frontend does not know the field.
    ///
    /// **This one AWAITS the snapshot**, unlike [`on_prompt`](Self::on_prompt).
    /// The whole value of the trigger is the ORDERING — a checkpoint taken
    /// after the edit has landed is a checkpoint of the damage. All three seams
    /// wait for it: the worker's `dispatch` has not spawned anything yet, the
    /// OpenCode plugin awaits its POST inside `tool.execute.before`, and (since
    /// the 2026-08-13 amendment) the Claude `PreToolUse` shim reads its reply
    /// instead of firing and forgetting. It is still bounded by the same
    /// throttle, so the cost is at most one `git add -A` per
    /// `checkpoint_min_gap_s` per tab, and still `None` (instant) when
    /// checkpoints are off or the gate throttles.
    ///
    /// # `deadline` — and why the app enforces the caller's budget
    ///
    /// `None` from the worker seam: it is in-process and waits as long as it
    /// takes. `Some(instant)` from the loopback route, for both out-of-process
    /// seams, because **both of them stop waiting after ~2 s** — the Claude
    /// shim on its reply-read timeout, the OpenCode plugin on its
    /// `AbortSignal.timeout(2000)` — and the harness runs the tool the moment
    /// they do. A snapshot still staging past that point is racing the very
    /// edit it is supposed to precede.
    ///
    /// A caller that merely stops waiting does not stop this side from writing
    /// the row, which is why the budget is enforced down in
    /// [`shadow::snapshot_detailed`] rather than with a `timeout` around this
    /// call: past the deadline nothing is committed at all, and the miss gets
    /// its own Activity row ([`checkpoint_miss_row`]).
    ///
    /// # Return
    ///
    /// `true` when the trigger ran to completion — a checkpoint was created, a
    /// dedup hit settled it, or the throttle legitimately declined it. `false`
    /// **only** when the snapshot was abandoned against `deadline` or failed,
    /// i.e. exactly when no checkpoint can be said to precede this call. The
    /// route reports it as `checkpointed`; nothing gates a tool call on it.
    pub async fn on_tool(
        &self,
        root: &Path,
        origin: shadow::Origin,
        source: &str,
        deadline: Option<Instant>,
    ) -> bool {
        let label = format!("tool: {source}");
        let origin = origin.with_source(Some(source.to_string()));
        match self
            .maybe_snapshot(root, label, shadow::Trigger::Tool, origin, deadline)
            .await
        {
            // A task that panicked joins as `Err`; treat it as the failure it
            // is rather than as a completed trigger.
            Some(handle) => handle.await.unwrap_or(false),
            // Throttled, or checkpoints are off. Neither is a missed guarantee:
            // the throttle means this tab already has a checkpoint newer than
            // `checkpoint_min_gap_s`, and "off" means the user asked for none.
            None => true,
        }
    }

    /// Phase C `workbench_checkpoint_now`: the manual trigger. Unlike the
    /// automatic triggers, this does NOT go through
    /// [`maybe_snapshot`](Self::maybe_snapshot)'s min-gap gate or its
    /// background-task indirection — an explicit "Checkpoint now" click is a
    /// deliberate user action awaiting a real result (the new checkpoint id),
    /// not something to silently throttle or defer. It still stamps the gate
    /// so a subsequent AUTOMATIC trigger's cooldown counts from this snapshot
    /// too (no back-to-back auto + manual spam) — the TAB-LESS bucket
    /// (`Origin::default()`), which since V33's per-`(root, tab)` keying means
    /// it debounces the burst trigger and no longer silences a tab's next
    /// prompt-tap. That follows the same rule the rest of the gate now obeys —
    /// an identity is throttled only by its OWN recent checkpoints — and
    /// stamping every tab's bucket from here would reintroduce exactly the
    /// cross-identity starvation the per-tab key was chosen to remove.
    ///
    /// Deliberately `Origin::default()`: a "Checkpoint now" click comes from the
    /// Workbench panel, not from a conversation, so there is no session and no
    /// tab to attribute it to. Inventing the focused tab's identity here would
    /// put a *conversation* label on a checkpoint that conversation did not
    /// take — the correlation this milestone is building exists to be trusted
    /// after an incident, so an absent identity beats a plausible one.
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
            &shadow::Origin::default(),
            &cfg.graph.ignore,
            cfg.graph.max_file_bytes,
        )
        .await?;
        self.checkpoints.record(root, &shadow::Origin::default());
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
    /// safety invariant it upholds. Stamps the gate's tab-less bucket for the
    /// same reason (and with the same consequences as)
    /// [`checkpoint_now`](Self::checkpoint_now) does.
    ///
    /// **Invariant C is upheld upstream of the gate, not by it.** The
    /// pre-restore safety snapshot is taken inside `shadow::restore` via
    /// `snapshot_inner`, which never consults `checkpoint_min_gap_s` at all —
    /// so no throttle bucket, a tab's or the tab-less one, can swallow the
    /// snapshot that makes a restore undoable. The stamp below happens strictly
    /// AFTER the restore has returned.
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
        self.checkpoints.record(root, &shadow::Origin::default());
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

    // ---- V33 per-`(root, tab)` checkpoint throttle -------------------------
    //
    // These drive the REAL [`CheckpointScheduler`] (the gate `maybe_snapshot`
    // calls) composed with the REAL `shadow::snapshot`, against a real temp
    // shadow repo. `WorkbenchService` itself is unreachable from a unit test —
    // it owns a Tauri `AppHandle` and this crate builds `tauri` without its
    // `test` feature — which is why the gate lives on a struct of its own; the
    // only production step not covered here is `maybe_snapshot`'s three-line
    // settings read + `spawn_if_due` call.

    /// A bare project directory (no user git repo needed — the shadow repo is
    /// self-contained). Under `std::env::temp_dir()`, matching `shadow.rs`.
    fn scratch_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("wb-gate-{tag}-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// `min_gap_s` is the only knob any of these tests vary; the rest are the
    /// shipped defaults. No pre-tool budget — the one test that wants one sets
    /// it explicitly, so every other test keeps measuring the gate alone.
    fn gate_params(min_gap_s: u64) -> SnapshotParams {
        SnapshotParams {
            min_gap: Duration::from_secs(min_gap_s),
            extra_ignore: Vec::new(),
            max_file_bytes: 1_000_000,
            checkpoint_max: 100,
            checkpoint_max_age_days: 7,
            deadline: None,
        }
    }

    /// One AI tab's identity. Distinct session ids on purpose: the gate must
    /// key on the TAB, so two tabs stay two buckets and one tab's rolling
    /// session ids stay one.
    fn tab_origin(tab: &str) -> shadow::Origin {
        shadow::Origin::new(
            Some("claude".to_string()),
            Some(format!("session-of-{tab}")),
            Some(tab.to_string()),
        )
    }

    /// Fire one automatic trigger exactly as `maybe_snapshot` does, then WAIT
    /// for the spawned snapshot task so the assertions that follow see a
    /// settled shadow repo. Returns whether the gate admitted it.
    async fn fire(
        sched: &CheckpointScheduler,
        root: &Path,
        label: &str,
        trigger: shadow::Trigger,
        origin: shadow::Origin,
        params: &SnapshotParams,
    ) -> bool {
        match sched.spawn_if_due(root, label.to_string(), trigger, origin, params.clone()) {
            Some(handle) => {
                handle.await.expect("checkpoint task");
                true
            }
            None => false,
        }
    }

    /// **The point of the per-`(root, tab)` key.** Two tabs on one project
    /// root, both prompting well inside one `checkpoint_min_gap_s` window,
    /// each get their OWN checkpoint carrying their OWN tab id.
    ///
    /// The `b.txt` write between the two prompts is load-bearing: with no file
    /// change, `snapshot`'s dedup would hand tab B tab A's existing checkpoint
    /// and the test would pass on the dedup path while proving nothing about
    /// the throttle. With it, a regression to per-root keying leaves exactly
    /// one checkpoint and this fails.
    #[tokio::test]
    async fn two_tabs_on_one_root_each_get_their_own_checkpoint_inside_one_gap_window() {
        if !has_git() {
            eprintln!("skipping: git not on PATH");
            return;
        }
        let dir = scratch_dir("two-tabs");
        let sched = CheckpointScheduler::default();
        // A gap far longer than the test: every prompt below is "inside the
        // window" by construction, with no timing race.
        let params = gate_params(3600);

        std::fs::write(dir.join("a.txt"), "a1\n").unwrap();
        assert!(
            fire(
                &sched,
                &dir,
                "prompt: from tab a",
                shadow::Trigger::Prompt,
                tab_origin("claude"),
                &params
            )
            .await,
            "the first prompt on a cold gate must always be admitted"
        );

        std::fs::write(dir.join("b.txt"), "b1\n").unwrap();
        assert!(
            fire(
                &sched,
                &dir,
                "prompt: from tab b",
                shadow::Trigger::Prompt,
                tab_origin("opencode"),
                &params
            )
            .await,
            "a second TAB on the same root must not be throttled by the first"
        );

        let cps = shadow::list(&dir).await.expect("list");
        let tabs: Vec<Option<String>> = cps.iter().map(|c| c.tab.clone()).collect();
        assert_eq!(
            tabs,
            vec![Some("claude".to_string()), Some("opencode".to_string())],
            "each tab must have a checkpoint labelled with its own id: {cps:?}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The other half of the contract: making the key per-tab must NOT make it
    /// a no-op. One tab prompting twice inside its own gap is still throttled.
    ///
    /// `c.txt` is written before the second prompt so that a gate which stopped
    /// throttling would actually mint a second checkpoint (dedup would hide the
    /// regression otherwise) — i.e. this test cannot pass by accident on the
    /// dedup path.
    #[tokio::test]
    async fn one_tab_prompting_twice_inside_the_gap_is_still_throttled() {
        if !has_git() {
            eprintln!("skipping: git not on PATH");
            return;
        }
        let dir = scratch_dir("same-tab");
        let sched = CheckpointScheduler::default();
        let params = gate_params(3600);

        std::fs::write(dir.join("a.txt"), "a1\n").unwrap();
        assert!(
            fire(
                &sched,
                &dir,
                "prompt: one",
                shadow::Trigger::Prompt,
                tab_origin("claude"),
                &params
            )
            .await
        );

        std::fs::write(dir.join("c.txt"), "c1\n").unwrap();
        assert!(
            !fire(
                &sched,
                &dir,
                "prompt: two",
                shadow::Trigger::Prompt,
                tab_origin("claude"),
                &params
            )
            .await,
            "the SAME tab inside its own min-gap must still be throttled"
        );

        let cps = shadow::list(&dir).await.expect("list");
        assert_eq!(cps.len(), 1, "throttled prompt must not snapshot: {cps:?}");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// **V33 Phase F: the tool trigger shares the tab's bucket — locked.**
    ///
    /// Two claims, and both matter:
    ///   * a tool call from tab B is NOT throttled by a prompt in tab A, so the
    ///     per-tab attribution the feature exists for survives (the locked
    ///     answer to the 2026-08-09 amendment's open question);
    ///   * a tool call from tab A INSIDE tab A's own prompt window IS throttled,
    ///     i.e. "keep the tab bucket" was not quietly implemented as "the tool
    ///     trigger gets its own bucket", which would have made every edit burst
    ///     a `git add -A` storm.
    ///
    /// The file writes between fires are load-bearing: with an unchanged tree
    /// `snapshot`'s dedup would return the previous checkpoint and both halves
    /// would pass on the dedup path, proving nothing about the gate.
    #[tokio::test]
    async fn a_tool_trigger_shares_its_tabs_bucket_and_no_other_tabs() {
        if !has_git() {
            eprintln!("skipping: git not on PATH");
            return;
        }
        let dir = scratch_dir("tool-bucket");
        let sched = CheckpointScheduler::default();
        let params = gate_params(3600);

        std::fs::write(dir.join("a.txt"), "a1\n").unwrap();
        assert!(
            fire(
                &sched,
                &dir,
                "prompt: from tab a",
                shadow::Trigger::Prompt,
                tab_origin("claude"),
                &params
            )
            .await
        );

        // Tab A is about to run `Bash`, still inside its own gap window.
        std::fs::write(dir.join("b.txt"), "b1\n").unwrap();
        assert!(
            !fire(
                &sched,
                &dir,
                "tool: claude:Bash",
                shadow::Trigger::Tool,
                tab_origin("claude").with_source(Some("claude:Bash".into())),
                &params
            )
            .await,
            "the tool trigger keeps the per-(root, tab) bucket — it must not get \
             a bucket of its own"
        );

        // Tab B's tool call is a different bucket and must be admitted.
        assert!(
            fire(
                &sched,
                &dir,
                "tool: opencode:edit",
                shadow::Trigger::Tool,
                tab_origin("opencode").with_source(Some("opencode:edit".into())),
                &params
            )
            .await,
            "another TAB's tool call must not be throttled by tab A's prompt"
        );

        let cps = shadow::list(&dir).await.expect("list");
        assert_eq!(cps.len(), 2, "one per admitted trigger: {cps:?}");
        assert_eq!(cps[0].trigger, shadow::Trigger::Prompt);
        assert_eq!(cps[0].source, None);
        assert_eq!(cps[1].trigger, shadow::Trigger::Tool);
        assert_eq!(cps[1].source, Some("opencode:edit".to_string()));
        assert_eq!(cps[1].tab, Some("opencode".to_string()));

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// **The 2026-08-13 amendment at the scheduler seam: the throttle is what
    /// makes waiting cheap, and a blown budget reports itself.**
    ///
    /// Three claims, measured rather than asserted:
    ///   * a tool trigger inside its tab's own gap window resolves **without
    ///     touching git at all** — no task is spawned, so the `on_tool` wait a
    ///     Claude `Edit` now pays is a `HashMap` lookup in the common case. This
    ///     is the claim the "every edit now waits for a `git`
    ///     stage-and-write-tree" cost rests on, and it is the DEDUP that does
    ///     *not* buy it: a dedup hit still pays the whole `git add -A`.
    ///   * an admitted trigger whose budget is already spent resolves `false`
    ///     and writes no checkpoint;
    ///   * the same trigger with no budget resolves `true` and writes one, so
    ///     the case above is measuring the deadline.
    #[tokio::test]
    async fn a_blown_pre_tool_budget_reports_itself_and_the_throttle_costs_no_git() {
        if !has_git() {
            eprintln!("skipping: git not on PATH");
            return;
        }
        let dir = scratch_dir("tool-budget");
        let sched = CheckpointScheduler::default();

        // Admitted, unbudgeted: a real checkpoint, and the trigger reports
        // completion.
        std::fs::write(dir.join("a.txt"), "a1\n").unwrap();
        let handle = sched
            .spawn_if_due(
                &dir,
                "tool: claude:Edit".to_string(),
                shadow::Trigger::Tool,
                tab_origin("claude").with_source(Some("claude:Edit".into())),
                gate_params(3600),
            )
            .expect("first tool trigger is admitted");
        assert!(handle.await.expect("task"), "a completed trigger reports true");
        assert_eq!(shadow::list(&dir).await.expect("list").len(), 1);

        // Throttled: no task at all. Timed, because "the throttle is why the
        // wait is affordable" is a latency claim and a spawned-and-deduped
        // snapshot would still take git-process time here.
        let t0 = Instant::now();
        assert!(
            sched
                .spawn_if_due(
                    &dir,
                    "tool: claude:Write".to_string(),
                    shadow::Trigger::Tool,
                    tab_origin("claude").with_source(Some("claude:Write".into())),
                    gate_params(3600),
                )
                .is_none(),
            "a tool call inside its own tab's gap window must not spawn a snapshot"
        );
        assert!(
            t0.elapsed() < Duration::from_millis(50),
            "the throttled path must not touch git — took {:?}",
            t0.elapsed()
        );

        // Admitted (a different tab's bucket) but out of budget: nothing is
        // written and the trigger reports the miss.
        std::fs::write(dir.join("b.txt"), "b1\n").unwrap();
        let mut params = gate_params(3600);
        params.deadline = Some(Instant::now() - Duration::from_secs(1));
        let handle = sched
            .spawn_if_due(
                &dir,
                "tool: claude:Edit".to_string(),
                shadow::Trigger::Tool,
                tab_origin("claude-2").with_source(Some("claude:Edit".into())),
                params,
            )
            .expect("another tab's bucket is admitted");
        assert!(
            !handle.await.expect("task"),
            "an abandoned pre-tool snapshot must NOT report completion — the route \
             reports that as `checkpointed: false`"
        );
        assert_eq!(
            shadow::list(&dir).await.expect("list").len(),
            1,
            "abandonment must write no checkpoint"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The miss row names the call whose checkpoint is missing, is flagged in
    /// the feed, and attributes itself to the tab — the three things a user
    /// looking for "why is there no checkpoint before this edit" needs.
    ///
    /// **What it would still pass with:** a row that said `ok: true` would read
    /// as ordinary traffic in the Events feed and be invisible at a glance, so
    /// the flag is asserted; and a row whose `target` dropped the `harness:tool`
    /// source would be a miss report that names no call, so that is asserted for
    /// its exact value rather than for being non-empty.
    #[test]
    fn a_checkpoint_miss_row_names_the_call_and_flags_itself() {
        let row = checkpoint_miss_row(
            Path::new("."),
            &tab_origin("claude-2").with_source(Some("claude:Edit".into())),
            Duration::from_millis(2400),
        );
        assert_eq!(row.entry.kind, crate::activity::ActivityKind::Graph.as_str());
        assert_eq!(row.entry.source, "workbench");
        assert_eq!(row.entry.tool, "checkpoint_missed");
        assert_eq!(row.entry.target, "claude:Edit");
        assert!(!row.entry.ok, "a miss must be flagged, not read as traffic");
        assert_eq!(row.entry.ms, 2400);
        assert_eq!(
            row.entry.tab,
            crate::activity::Attribution::Tab("claude-2".to_string())
        );
        assert_eq!(row.entry.session.as_deref(), Some("session-of-claude-2"));
        assert!(row.request.contains("claude:Edit"));

        // No tab ⇒ "this writer does not know", never `Headless` (which would
        // claim the call came from a headless consumer) and never another tab.
        let anon = checkpoint_miss_row(
            Path::new("."),
            &shadow::Origin::default().with_source(Some("offload:run_command".into())),
            Duration::ZERO,
        );
        assert_eq!(anon.entry.tab, crate::activity::Attribution::Unattributed);
        assert_eq!(anon.entry.target, "offload:run_command");
    }

    /// The tab-less bucket, asserted in both directions (see [`CheckpointKey`]
    /// for the decision): a burst is NOT starved by a tab that checkpointed
    /// moments ago, and bursts DO still throttle each other.
    #[tokio::test]
    async fn tabless_triggers_share_one_bucket_that_no_tab_can_starve() {
        if !has_git() {
            eprintln!("skipping: git not on PATH");
            return;
        }
        let dir = scratch_dir("tabless");
        let sched = CheckpointScheduler::default();
        let params = gate_params(3600);

        std::fs::write(dir.join("a.txt"), "a1\n").unwrap();
        assert!(
            fire(
                &sched,
                &dir,
                "prompt: tab a",
                shadow::Trigger::Prompt,
                tab_origin("claude"),
                &params
            )
            .await
        );

        std::fs::write(dir.join("b.txt"), "b1\n").unwrap();
        assert!(
            fire(
                &sched,
                &dir,
                "activity",
                shadow::Trigger::Burst,
                shadow::Origin::default(),
                &params
            )
            .await,
            "a tab's checkpoint must not starve the tab-less (burst) bucket"
        );

        std::fs::write(dir.join("c.txt"), "c1\n").unwrap();
        assert!(
            !fire(
                &sched,
                &dir,
                "activity",
                shadow::Trigger::Burst,
                shadow::Origin::default(),
                &params
            )
            .await,
            "tab-less triggers share ONE bucket, so they still debounce each other"
        );

        let cps = shadow::list(&dir).await.expect("list");
        assert_eq!(cps.len(), 2, "expected exactly two checkpoints: {cps:?}");
        assert_eq!(cps[1].tab, None, "the burst checkpoint carries no tab");
        assert_eq!(cps[1].trigger, shadow::Trigger::Burst);

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// **Invariant C under the new key.** `restore`'s pre-restore safety
    /// snapshot — the thing that makes a restore undoable — must be taken even
    /// when every bucket the gate could possibly consult is inside its
    /// cooldown. It is, because `shadow::restore` snapshots via
    /// `snapshot_inner` and never touches the gate at all; this pins that.
    #[tokio::test]
    async fn pre_restore_snapshot_survives_a_hot_gate_on_every_bucket() {
        if !has_git() {
            eprintln!("skipping: git not on PATH");
            return;
        }
        let dir = scratch_dir("pre-restore");
        let sched = CheckpointScheduler::default();
        let params = gate_params(3600);

        std::fs::write(dir.join("a.txt"), "v1\n").unwrap();
        assert!(
            fire(
                &sched,
                &dir,
                "prompt: tab a",
                shadow::Trigger::Prompt,
                tab_origin("claude"),
                &params
            )
            .await
        );
        let target = shadow::list(&dir).await.expect("list")[0].id.clone();

        std::fs::write(dir.join("b.txt"), "b1\n").unwrap();
        assert!(
            fire(
                &sched,
                &dir,
                "activity",
                shadow::Trigger::Burst,
                shadow::Origin::default(),
                &params
            )
            .await
        );

        // Both buckets are now hot — proven, not assumed.
        assert!(
            !fire(
                &sched,
                &dir,
                "prompt: tab a again",
                shadow::Trigger::Prompt,
                tab_origin("claude"),
                &params
            )
            .await,
            "precondition: the tab bucket must be inside its cooldown"
        );
        assert!(
            !fire(
                &sched,
                &dir,
                "activity",
                shadow::Trigger::Burst,
                shadow::Origin::default(),
                &params
            )
            .await,
            "precondition: the tab-less bucket must be inside its cooldown"
        );

        // Uncommitted work that only the pre-restore snapshot can save.
        std::fs::write(dir.join("a.txt"), "v2-unsaved\n").unwrap();
        let before = shadow::list(&dir).await.expect("list").len();

        let report = shadow::restore(&dir, &target, false, &[], params.max_file_bytes)
            .await
            .expect("restore");

        assert_ne!(
            report.pre_restore_id, target,
            "the pre-restore snapshot must be a NEW checkpoint, not a throttled/dedup reuse"
        );
        let cps = shadow::list(&dir).await.expect("list");
        assert_eq!(
            cps.len(),
            before + 1,
            "expected one new checkpoint: {cps:?}"
        );
        let pre = cps
            .iter()
            .find(|c| c.id == report.pre_restore_id)
            .expect("pre-restore checkpoint listed");
        assert_eq!(pre.trigger, shadow::Trigger::PreRestore);
        // And the restore itself really ran (so this isn't passing on a no-op).
        assert_eq!(
            std::fs::read_to_string(dir.join("a.txt")).unwrap(),
            "v1\n",
            "the restore must have rolled a.txt back"
        );
        // ...and the undo point really holds the work that was rolled back.
        let undone =
            shadow::diff_vs_now(&dir, &report.pre_restore_id, &[], params.max_file_bytes, 3)
                .await
                .expect("diff vs the pre-restore checkpoint");
        assert!(
            undone.contains("v2-unsaved"),
            "the pre-restore checkpoint must hold the pre-restore content: {undone}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// **The dedup interaction the `maybe_snapshot` doc comment warns about.**
    /// Passing the gate is not the same as getting a checkpoint: with nothing
    /// changed on disk, `snapshot` returns the existing checkpoint and commits
    /// nothing — inside the window (where the gate refuses anyway) and outside
    /// it (where the gate admits and dedup is the only thing standing there).
    #[tokio::test]
    async fn same_tab_with_no_file_changes_gets_no_new_checkpoint_either_way() {
        if !has_git() {
            eprintln!("skipping: git not on PATH");
            return;
        }
        let dir = scratch_dir("dedup-same-tab");
        let sched = CheckpointScheduler::default();

        std::fs::write(dir.join("a.txt"), "a1\n").unwrap();
        assert!(
            fire(
                &sched,
                &dir,
                "prompt: one",
                shadow::Trigger::Prompt,
                tab_origin("claude"),
                &gate_params(3600)
            )
            .await
        );
        assert_eq!(shadow::list(&dir).await.expect("list").len(), 1);

        // Inside the window: the GATE refuses.
        assert!(
            !fire(
                &sched,
                &dir,
                "prompt: two",
                shadow::Trigger::Prompt,
                tab_origin("claude"),
                &gate_params(3600)
            )
            .await
        );
        assert_eq!(shadow::list(&dir).await.expect("list").len(), 1);

        // Outside the window (gap 0): the gate ADMITS, and dedup is what keeps
        // the shadow repo from gaining a duplicate of an identical tree.
        assert!(
            fire(
                &sched,
                &dir,
                "prompt: three",
                shadow::Trigger::Prompt,
                tab_origin("claude"),
                &gate_params(0)
            )
            .await,
            "with the gap elapsed the gate must admit — dedup, not the gate, is the filter here"
        );
        let cps = shadow::list(&dir).await.expect("list");
        assert_eq!(
            cps.len(),
            1,
            "an unchanged tree must not mint a second checkpoint: {cps:?}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The corollary the doc comment spells out, pinned so nobody "fixes" it:
    /// per-tab THROTTLING does not imply a per-tab CHECKPOINT. Tab B passes
    /// the gate, but with an unchanged tree it gets tab A's checkpoint back,
    /// still labelled tab A — a checkpoint names a tree state, and this tree
    /// state is already captured.
    #[tokio::test]
    async fn a_second_tab_with_no_changes_does_not_relabel_the_existing_checkpoint() {
        if !has_git() {
            eprintln!("skipping: git not on PATH");
            return;
        }
        let dir = scratch_dir("dedup-two-tabs");
        let sched = CheckpointScheduler::default();
        let params = gate_params(3600);

        std::fs::write(dir.join("a.txt"), "a1\n").unwrap();
        assert!(
            fire(
                &sched,
                &dir,
                "prompt: tab a",
                shadow::Trigger::Prompt,
                tab_origin("claude"),
                &params
            )
            .await
        );
        // No file change at all before tab B's prompt.
        assert!(
            fire(
                &sched,
                &dir,
                "prompt: tab b",
                shadow::Trigger::Prompt,
                tab_origin("opencode"),
                &params
            )
            .await,
            "the gate must admit tab B — it has its own bucket"
        );

        let cps = shadow::list(&dir).await.expect("list");
        assert_eq!(
            cps.len(),
            1,
            "dedup: identical tree, one checkpoint: {cps:?}"
        );
        assert_eq!(
            cps[0].tab,
            Some("claude".to_string()),
            "the existing checkpoint keeps the identity that took it"
        );

        let _ = std::fs::remove_dir_all(&dir);
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
