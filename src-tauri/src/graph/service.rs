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

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, Instant};

use ignore::gitignore::Gitignore;
use ignore::WalkBuilder;
use tauri::{AppHandle, Emitter, Manager};
use tracing::{debug, info, warn};

use crate::error::AppResult;
use crate::settings::{GraphSettings, SettingsHandle};

use super::embed;
use super::embed::Embedder;
use super::index::{GraphIndex, GraphStats, LangCount, SymbolHit};
use super::memory::{
    Effectiveness, MemorySnapshot, ModelUsage, ProjectFact, SessionUsage, SessionUsageDetail,
    SessionUsageRow, ToolUsage, TurnUsage, UsageEvent, UsageSnapshot, UsageTotals, WorkingSetEntry,
};
use super::model::Lang;
use super::parse_file;

/// Bumped if the embedding schema/layout changes in a way that invalidates
/// stored vectors. Part of the epoch fingerprint alongside model + dim.
const EMBED_SCHEMA: &str = "v1";

/// V12 Phase E: the distiller's fixed extraction instruction, prepended to a
/// session's working set + notes before the local-only completion. Verbatim
/// per the milestone spec — do not paraphrase (the validator downstream
/// assumes this exact output contract: 1-3 lines, no numbering, no preamble).
const DISTILL_PROMPT_INSTRUCTION: &str = "You are distilling a coding session's memory into \
durable project facts. From the working set and notes below, extract AT MOST 3 non-obvious, \
durable facts a future coding session on THIS project would need. Skip anything derivable from \
the code itself (structure, naming, what a function does). Prefer decisions, constraints, \
gotchas, and their rationale. Output one fact per line, each <=200 chars, plain text, no \
numbering, no preamble, no blank lines.";

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
    /// V11 Phase G: code-body vectors stored for the current epoch.
    pub code_embedded: u64,
    /// V11 Phase G: total code chunks (the code-embedding denominator).
    pub code_embed_total: u64,
    /// V11 Phase F: cached local-model context digests.
    pub digests: u64,
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
            code_embedded: 0,
            code_embed_total: 0,
            digests: 0,
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

/// V24 Phase B: a registry entry marks a session live within
/// [`LIVE_SESSION_TTL_MS`] of its last refresh (a still-ticking Claude drain
/// tick, or a still-reporting OpenCode session). The Claude drain polls every
/// ~200ms, so even an idle-but-open tab refreshes well inside this window; the
/// generous margin only tolerates a slow drain of a large transcript.
///
/// H1-R2 (2026-08-05 review): the margin is NOT self-evident for a busy tab, and
/// the failure mode is worse than for an idle one. The Claude tap's drain can
/// park for minutes inside `ctx.speak()` (a bounded TTS channel drained at ONNX
/// synthesis speed), so "the loop polls every 200ms" describes only the idle
/// case. A tab whose entries aged out here does not merely go quiet: its
/// co-tenant stops being detected, [`tab_binding_is_ambiguous`] flips to `false`
/// for the sibling, and the sibling's tap — which tails the *stalled* tab's
/// transcript, the newest file — gains a CONFIDENT and WRONG session binding.
/// The tap therefore refreshes both of its entries from an independent heartbeat
/// task (`oob::claude::TapHeartbeat`) that no drain-side await can starve; this
/// TTL only has to outlast that heartbeat's cadence by a wide margin.
const LIVE_SESSION_TTL_MS: i64 = 90_000;

/// V24 Phase B: the recency half of the decided "open tabs + recency"
/// semantics — a session whose last recorded activity falls within this window
/// also counts as active, catching a live session the registry missed (a
/// pre-existing tab from before this process, or the gap before the first
/// drain tick).
const LIVE_SESSION_RECENCY_MS: i64 = 5 * 60_000;

/// V24 Phase B: one live-session registry entry. `session_id` is the value (not
/// the key) so a Claude tab keyed by its stable tab id can rotate the session
/// it reports without leaking a stale key; OpenCode keys by the reporting
/// session id itself (no tab binding on the loopback path).
#[derive(Clone, Debug)]
struct LiveSession {
    /// Which agent reported the entry (`"claude"` / `"opencode"`). Read by
    /// [`GraphService::live_claude_sessions`] — the NC-2 permission-hook's
    /// session→tab mapping only trusts Claude entries, whose key IS a tab id.
    agent: String,
    session_id: String,
    last_seen_ms: i64,
}

/// H1 fix (2026-08-05 review): one RUNNING agent tab and the transcript source
/// it binds its session identity from — for Claude, the
/// `~/.claude/projects/<slug>/` directory its out-of-band tap tails.
///
/// The Claude tap has no per-process discriminator: it binds to the
/// newest-mtime `*.jsonl` under that directory, so TWO running Claude tabs on
/// one project (e.g. the built-in `claude` + `claude-local`, both `cwd: None`)
/// both resolve to whichever session wrote last. Every identity claim keyed by
/// such a tab is therefore unprovable. This map is what makes that condition
/// *detectable* at the registry seam: it is written by the tap itself (so it
/// reflects tabs that are genuinely running, not merely configured), keyed by
/// the stable tab id, refreshed on every poll tick, TTL-expired like
/// [`LiveSession`], and cleared by the tap's RAII guard on tab exit.
#[derive(Clone, Debug)]
struct LiveTabRoot {
    /// Which harness runs in this tab (`"claude"`). Only agents whose binding
    /// is root-derived register here — OpenCode binds per-tab off its own SSE
    /// stream and is deliberately absent (see [`tab_binding_is_ambiguous`]).
    agent: String,
    /// The transcript source directory the tap tails, as a normalized
    /// COMPARISON KEY ([`crate::fsutil::norm_dir_key_path`]) — not a
    /// displayable path. H1-R5: normalized once here, at the single write site
    /// ([`upsert_live_tab_root`]), so every reader compares canonical keys and
    /// two tabs whose hand-set cwds differ only by case or a trailing separator
    /// are still recognized as co-tenants. Same normalization posture as the
    /// permission hook's cwd fallback (`offload::loopback::norm_dir`), which
    /// routes through the same helper.
    root: PathBuf,
    last_seen_ms: i64,
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
    /// V11 Phase B: session ids that have already received the once-per-session
    /// project-map greeting. In-memory only — a restart re-greets each session
    /// once, which is acceptable (and self-corrects). A session is recorded here
    /// only after a non-empty map actually renders, so an early prompt (before
    /// the graph has call edges) retries on the next turn.
    greeted: StdMutex<HashSet<String>>,
    /// V11 Phase C: per-session injection dedup state. In-memory (a restart just
    /// re-injects fresh, which is safe). Keyed by session id; the preview path
    /// (session id `None`) never touches it.
    injected: StdMutex<HashMap<String, InjectState>>,
    /// V11 Phase D/E: session ids that just went through a compaction. Set by the
    /// `/context/compaction` route; consumed by the read advisor (Phase E), which
    /// passes every read until each file is re-read once after a compaction (the
    /// agent genuinely lost the content the summary dropped).
    post_compaction: StdMutex<HashSet<String>>,
    /// V11 Phase E: `(session_id, rel_path)` pairs the read advisor has already
    /// reminded about — one reminder per file per session, so an agent that reads
    /// again after a reminder (it knows better than our heuristic) always passes.
    /// V16 Feature 4: the value records WHEN the reminder fired (turn + wall
    /// clock) and how many chars it displaced, so the transcript tap's bypass
    /// matcher can test "shell read of a just-reminded file" and the
    /// Effectiveness accounting can un-count a bypassed remind.
    /// Keyed session-first so `check_bypass` (which runs on EVERY Claude
    /// Bash `tool_use` across all tabs) scans only its own session's
    /// reminders, not the whole process-wide set.
    reminded: StdMutex<HashMap<String, HashMap<String, RemindMark>>>,
    /// V11 Phase E / F4-fix: the content hash the read advisor last observed on
    /// disk for a `(session_id, rel_path)` — i.e. what the agent actually read
    /// last time. The staleness check compares against THIS, not the index hash:
    /// once the watcher re-indexes an edited file the index hash matches the new
    /// content, but the agent's context still holds the version it read before
    /// the edit, so comparing to the index would wrongly suppress the re-read it
    /// genuinely needs. In-memory (a restart just allows a fresh read); capped.
    /// V16 Feature 5: the value also records the retrieve-turn the observation
    /// happened on, so the trust TTL can expire it.
    /// V17 Phase A: the value is now a [`ReadSeen`] carrying an optional
    /// in-memory snapshot of the last-read content, so a changed re-read can be
    /// answered with a diff. LRU-bounded (snapshots by [`SNAP_TOTAL_MAX`] bytes,
    /// rows by [`READ_SEEN_MAX_ENTRIES`]); eviction drops content, never the
    /// hash/turn observation.
    /// V22 efficiency: wrapped in a [`ReadSeenStore`] that carries a running sum
    /// of live snapshot bytes, so the per-Read insert path enforces the
    /// [`SNAP_TOTAL_MAX`] budget without re-summing the whole map each time.
    read_seen: StdMutex<ReadSeenStore>,
    /// V17 Phase A: monotonic touch counter driving the `read_seen` LRU. Bumped
    /// on every observation; the smallest value is evicted first.
    read_seen_touch: AtomicU64,
    /// V11 Phase F: `(root, file, content_hash)` digest jobs currently in flight,
    /// so a cache miss never spawns a duplicate local-model job for the same
    /// content. Keyed by root too, since one service manages multiple projects
    /// and an identical (path, hash) can occur in two of them. Also caps
    /// concurrent jobs (demand-driven + slot-gated by the supervisor).
    digest_inflight: StdMutex<HashSet<(PathBuf, String, String)>>,
    /// V12 Phase F: per-session auto-check debounce/baseline/pending state
    /// (`/context/post_edit`). Keyed by session id — never by root, so two
    /// agents editing the same project each get their own "what's new" view
    /// (V10 session scoping).
    auto_check_sessions: StdMutex<HashMap<String, crate::checks::auto::AutoCheckState>>,
    /// V12 Phase F: the single-flight check runner shared by every session —
    /// see `checks::auto::RootRunner`'s doc comment.
    auto_check_runner: crate::checks::auto::RootRunner,
    /// V12 review: session ids currently being distilled by [`GraphService::distill_session`],
    /// so two sweeps that both select the same idle-undistilled session (the
    /// rebuild sweep and the watcher-batch sweep can race onto the same
    /// candidate across the ~30s `run_internal` await) don't both distill it —
    /// same single-flight shape as `digest_inflight`/[`InflightGuard`].
    distilling: StdMutex<HashSet<String>>,
    /// V16 Feature 2: per-root cache of the full-scan drift signals — see
    /// [`DriftDbSignals`].
    drift_signals: StdMutex<HashMap<PathBuf, DriftDbSignals>>,
    /// V16 Feature 4: reminder-TEXT chars of every bypassed reminder since
    /// this process started — the like-for-like subtrahend for the panel's
    /// displaced figure (which sums reminder text, not file content).
    /// Process-wide + since-restart, matching the Activity-based sums in
    /// [`Self::effectiveness_totals`].
    bypassed_advice_chars: AtomicU64,
    /// V24 Phase B: the live-session registry — which agent sessions are active
    /// right now. Keyed by a stable key (a Claude tab id, or the reporting
    /// session id on the OpenCode loopback path), value = [`LiveSession`].
    /// Process-wide (one service serves every project), so `usage_snapshot`
    /// intersects it with the queried root's own sessions before reporting
    /// `active_session_ids`. Claude clears its entry on tab cancel; every entry
    /// also expires by [`LIVE_SESSION_TTL_MS`].
    live_sessions: StdMutex<HashMap<String, LiveSession>>,
    /// H1 fix: the running-tab → transcript-root map behind
    /// [`tab_binding_is_ambiguous`] — see [`LiveTabRoot`]. Written only by the
    /// per-tab out-of-band taps; read by every consumer of a tab-keyed identity
    /// claim ([`Self::live_session_for_tab`], [`Self::live_claude_sessions`]).
    live_tab_roots: StdMutex<HashMap<String, LiveTabRoot>>,
    /// V30 Phase C: the session-push bus, when this process has one. `None` for
    /// tests and any standalone construction without an `OffloadService` — a
    /// push is best-effort by contract, so its absence is a silent no-op, never
    /// an error. Only the send half is held (see
    /// [`OffloadService::push_registry`](crate::offload::OffloadService::push_registry)):
    /// no back-reference, so no Arc cycle with the offload service.
    pushes: Option<Arc<crate::offload::service::PushRegistry>>,
}

/// RAII removal of a digest in-flight key (V11 Phase F). Runs on `Drop`, so a
/// panic in the spawned digest task can't leak the key and permanently shrink
/// the in-flight budget — mirrors [`BackfillGuard`]'s cleanup discipline.
struct InflightGuard {
    svc: Arc<GraphService>,
    key: (PathBuf, String, String),
}

impl Drop for InflightGuard {
    fn drop(&mut self) {
        if let Ok(mut g) = self.svc.digest_inflight.lock() {
            g.remove(&self.key);
        }
    }
}

/// RAII removal of a `distilling` in-flight session id (V12 review). Runs on
/// `Drop` so an early `return` (validation failure, offload error, ...) or a
/// future panic in [`GraphService::distill_session`] still frees the session
/// for a later sweep — mirrors [`InflightGuard`]'s cleanup discipline.
struct DistillGuard<'a> {
    svc: &'a GraphService,
    session_id: String,
}

impl Drop for DistillGuard<'_> {
    fn drop(&mut self) {
        if let Ok(mut g) = self.svc.distilling.lock() {
            g.remove(&self.session_id);
        }
    }
}

/// Per-session record of what context has been injected, for dedup (V11 Phase C).
#[derive(Default)]
struct InjectState {
    /// Monotonic retrieve counter for this session (the dedup TTL clock).
    turn: u32,
    /// `path → (content_hash_at_injection, turn_injected)` for files injected in
    /// full, so an unchanged re-candidate can be demoted to a one-line reminder.
    files: HashMap<String, (String, u32)>,
    /// V14 Phase D: cumulative chars of full digests injected this session —
    /// feeds the Usage section's Effectiveness panel (`injected_chars`) via
    /// [`GraphService::effectiveness_totals`]. Honest measured accounting
    /// (summed straight from `RetrieveResult::chars`), not a token estimate.
    injected_chars: u64,
    /// V14 Phase D: cumulative chars suppressed by dedup this session (V11-C
    /// `RetrieveResult::deduped_chars`) — feeds `deduped_chars`.
    deduped_chars: u64,
    /// V14 Phase D2: retrieval turns observed this session, and (`turns_maxed`)
    /// how many of them injected at least 90% of `context_turn_budget_chars`
    /// — the advisor's "budget maxed" signal (rule 3, paired with
    /// [`GraphService::injection_follow_rate`]).
    turns_seen: u32,
    turns_maxed: u32,
    /// V16 Feature 9: chars kept OUT of context so far this session —
    /// dedup-suppressed digests plus advisor-displaced reads. A bypassed
    /// remind (Feature 4) is subtracted back out (it displaced nothing).
    displaced_chars_total: u64,
    /// V16 Feature 9: the compounding readout — on every retrieve turn,
    /// `displaced_chars_total` is added again, because content kept out of
    /// context at turn N is re-saved as a cache read on EVERY turn after N
    /// (the API re-sends the whole conversation each turn). Measured
    /// turn-by-turn as the session actually runs — no projection.
    compounded_chars: u64,
}

/// V16 Feature 4: when (and how big) a read-advisor reminder was, so the
/// transcript tap's bypass matcher can test "shell read within the window"
/// and un-count the displaced chars. One mark per `(session, file)` — the
/// remind-once semantics are unchanged.
struct RemindMark {
    /// The session's retrieve-turn counter when the reminder fired (0 when
    /// context injection is off and the clock never ticks).
    turn: u32,
    /// Wall clock of the reminder — the bypass window's fallback when the
    /// turn clock isn't ticking.
    ts_ms: u64,
    /// Chars of the file content the reminder displaced (the file size, not
    /// the reminder text — what a bypass re-spends).
    chars: u64,
    /// Chars of the reminder TEXT that was returned (the Activity `remind`
    /// event's own size). Kept alongside `chars` because the two are
    /// different units: the panel's displaced figure sums reminder text, so
    /// netting a bypass out of it must subtract reminder text too — not the
    /// whole-file `chars` (one big-file bypass would wipe out the entire
    /// metric).
    advice_chars: u64,
    /// Set once a bypass was recorded against this reminder, so repeated
    /// `cat`s of the same file count one bypass, not N.
    bypassed: bool,
    /// V17 Phase A: how many reminders have fired for this `(session, file)`.
    /// A CHANGED re-read re-arms an already-reminded file (the old remind
    /// promised "unchanged"; the change makes that stale), but only while
    /// `count < READ_REMIND_CAP` — so the advisor can never fight an insistent
    /// agent in a loop. Bumped on every diff remind; an unchanged reminded file
    /// never re-reminds regardless of count. First remind sets it to 1.
    count: u32,
}

/// V17 Phase A — the read advisor's per-`(session, file)` observation: the
/// content hash + turn it was last seen at (the staleness/TTL comparison keys),
/// plus an optional in-memory SNAPSHOT of that content so a later changed
/// re-read can be answered with a diff against exactly what the agent read.
/// The snapshot is dropped (set to `None`) on LRU eviction — but the
/// `(hash, turn)` observation is NEVER forgotten by eviction (only the content
/// is), so the advisor's staleness logic is unaffected by memory pressure.
struct ReadSeen {
    /// Content hash the advisor last observed the agent read.
    hash: String,
    /// The session's retrieve-turn when that read was observed (TTL clock).
    turn: u32,
    /// The content itself, kept only for files ≥ `read_advisor_min_lines` and
    /// ≤ [`SNAP_ENTRY_MAX`] bytes, and only until evicted by the [`SNAP_TOTAL_MAX`]
    /// byte-budget LRU. `None` = small file, over-cap, evicted, or a branch that
    /// deliberately keeps no snapshot (Phase C's first-read tier).
    snapshot: Option<Arc<str>>,
    /// Monotonic touch order for the LRU: both snapshot eviction and the
    /// entry backstop drop the smallest-`touch` first.
    touch: u64,
}

/// V17 Phase A: per-entry snapshot cap — content larger than this is observed
/// (hash/turn recorded) but never snapshotted, so one huge file can't dominate
/// the diff budget.
const SNAP_ENTRY_MAX: usize = 512 * 1024;
/// V17 Phase A: whole-store snapshot byte budget. On overflow the oldest-touched
/// snapshots are dropped (content only — the observation survives).
const SNAP_TOTAL_MAX: usize = 16 * 1024 * 1024;
/// V17 Phase A: backstop bound on the number of `read_seen` OBSERVATIONS (rows,
/// snapshot or not) — the byte-budget LRU alone bounds only snapshotted content,
/// so a long session that touches thousands of small files still needs an entry
/// cap. Subsumes the old 1024-entry blanket clear; evicts oldest-touched whole
/// rows instead of wiping the map (clearing is safe — a dropped row just allows
/// one fresh read).
const READ_SEEN_MAX_ENTRIES: usize = 4096;
/// V17 Phase A: max reminders per `(session, file)` before a changed re-read
/// just passes (see [`RemindMark::count`]). A const, not a setting — promote if
/// field data demands.
const READ_REMIND_CAP: u32 = 3;

/// V17 Phase A: capture the read-advisor snapshot for `content`, or `None` when
/// it isn't worth keeping (fewer than `min_lines` lines, or over
/// [`SNAP_ENTRY_MAX`] bytes). Pure.
fn capture_snapshot(content: &str, min_lines: u32) -> Option<Arc<str>> {
    if (content.lines().count() as u32) >= min_lines && content.len() <= SNAP_ENTRY_MAX {
        Some(Arc::from(content))
    } else {
        None
    }
}

/// V17 Phase A: total bytes of all live snapshots in the store. O(n) over the
/// map — the GROUND TRUTH the incremental [`ReadSeenStore::snap_bytes`] running
/// total must always equal (asserted in tests). Test-only since V22 — the hot
/// path now trusts the incremental running total instead of re-summing.
#[cfg(test)]
fn snapshot_bytes(seen: &HashMap<(String, String), ReadSeen>) -> usize {
    seen.values()
        .filter_map(|v| v.snapshot.as_ref().map(|s| s.len()))
        .sum()
}

/// V22 efficiency: the read-advisor's snapshot store — the
/// `(session, file) → ReadSeen` map plus a running sum of live snapshot bytes.
///
/// `snap_bytes` is maintained INCREMENTALLY at every mutation (insert, replace,
/// eviction, session/whole clear), so the per-Read [`Self::insert`] path enforces
/// the [`SNAP_TOTAL_MAX`] byte budget without the old unconditional O(n)
/// `snapshot_bytes` re-sum. Its value always equals `snapshot_bytes(&self.map)`.
///
/// The O(n) `min_by_key` victim scans remain — but only inside the eviction
/// loops, which run solely when a cap is actually exceeded (rare, and each drop
/// is bounded by one over-budget insert's contribution), so they stay off the
/// common path. Left as-is: a heap/priority index over `touch` would be a
/// disproportionate rebuild for a scan that no longer runs per insert.
#[derive(Default)]
struct ReadSeenStore {
    map: HashMap<(String, String), ReadSeen>,
    /// Running sum of `snapshot.len()` across all live rows. Invariant:
    /// `snap_bytes == snapshot_bytes(&map)` after every method returns.
    snap_bytes: usize,
}

/// Bytes a `ReadSeen`'s snapshot contributes to the running total (0 if none).
fn snap_len(v: &ReadSeen) -> usize {
    v.snapshot.as_ref().map(|s| s.len()).unwrap_or(0)
}

impl ReadSeenStore {
    /// V17 Phase A / V22: insert/replace `key`'s observation and enforce both
    /// bounds — the [`READ_SEEN_MAX_ENTRIES`] entry backstop (evicts whole oldest
    /// rows) and the [`SNAP_TOTAL_MAX`] snapshot byte budget (drops oldest-touched
    /// snapshots but keeps their `hash`/`turn`). Keeps `snap_bytes` consistent at
    /// each step. `touch` is a fresh monotonic value from the service's counter.
    fn insert(
        &mut self,
        key: (String, String),
        hash: String,
        turn: u32,
        snapshot: Option<Arc<str>>,
        touch: u64,
    ) {
        let added = snapshot.as_ref().map(|s| s.len()).unwrap_or(0);
        // A replace drops the old row's snapshot bytes before adding the new.
        if let Some(old) = self.map.insert(
            key,
            ReadSeen {
                hash,
                turn,
                snapshot,
                touch,
            },
        ) {
            self.snap_bytes -= snap_len(&old);
        }
        self.snap_bytes += added;
        // Entry backstop: drop whole oldest-touched rows past the cap.
        while self.map.len() > READ_SEEN_MAX_ENTRIES {
            let victim = self
                .map
                .iter()
                .min_by_key(|(_, v)| v.touch)
                .map(|(k, _)| k.clone());
            match victim {
                Some(k) => {
                    if let Some(v) = self.map.remove(&k) {
                        self.snap_bytes -= snap_len(&v);
                    }
                }
                None => break,
            }
        }
        // Snapshot byte budget: drop oldest-touched SNAPSHOTS (keep hash/turn).
        while self.snap_bytes > SNAP_TOTAL_MAX {
            let victim = self
                .map
                .iter()
                .filter(|(_, v)| v.snapshot.is_some())
                .min_by_key(|(_, v)| v.touch)
                .map(|(k, _)| k.clone());
            match victim {
                Some(k) => {
                    if let Some(v) = self.map.get_mut(&k) {
                        self.snap_bytes -= snap_len(v);
                        v.snapshot = None;
                    }
                }
                None => break,
            }
        }
    }

    /// V22: drop rows for one session (`Some`) or all (`None`), keeping
    /// `snap_bytes` consistent — the read-advisor half of [`GraphService::mem_clear`].
    fn clear_session(&mut self, session_id: Option<&str>) {
        match session_id {
            Some(s) => {
                let dropped = &mut self.snap_bytes;
                self.map.retain(|(sid, _), v| {
                    let keep = sid != s;
                    if !keep {
                        *dropped -= snap_len(v);
                    }
                    keep
                });
            }
            None => {
                self.map.clear();
                self.snap_bytes = 0;
            }
        }
    }
}

/// V17 Phase A — the read advisor's verdict, as a pure decision over the facts
/// [`GraphService::should_read`] has already gathered (no locks, no I/O), so the
/// TTL / re-arm-cap / diff-threshold rules are unit-testable without a live
/// service (which needs an `AppHandle`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReadAdvice {
    /// Let the read through. `restamp` = whether `read_seen` should be
    /// (re)written to the new observation: true on never-seen, on a changed
    /// pass, and on a TTL re-stamp; false on the unchanged pass-throughs
    /// (already-reminded / small-file) that leave the prior observation intact.
    Pass { restamp: bool },
    /// Emit the outline reminder (unchanged, not yet reminded, ≥ min_lines).
    Outline,
    /// Emit the diff reminder (changed, snapshot present, diff worth it, under cap).
    Diff,
}

/// The facts a read verdict depends on. All cheap to compute in `should_read`.
struct VerdictIn {
    /// `read_seen` had an entry for this `(session, file)`.
    seen: bool,
    /// `seen` and the stored hash equals the current hash.
    unchanged: bool,
    /// `unchanged`, the TTL is enabled, and it has expired.
    ttl_expired: bool,
    /// The file was already reminded this session.
    reminded: bool,
    /// The reminder count so far (0 when not reminded).
    remind_count: u32,
    /// Current content is ≥ `read_advisor_min_lines` lines.
    big_enough: bool,
    /// `read_advisor_diffs` is on.
    diffs_on: bool,
    /// A snapshot of the prior content survives (not evicted / not too small).
    have_snapshot: bool,
    /// The rendered diff is ≤ 50% of the new content's length.
    diff_worth_it: bool,
}

/// V17 Phase A — pure verdict. See [`ReadAdvice`]. The never-seen arm returns
/// `Pass { restamp: true }`; Phase C's first-read branch slots in *before* this
/// is consulted (it evaluates the never-seen case itself).
fn read_verdict(i: &VerdictIn) -> ReadAdvice {
    if !i.seen {
        // Never seen this session ⇒ record-and-pass.
        return ReadAdvice::Pass { restamp: true };
    }
    if i.unchanged {
        // Trust TTL expired ⇒ pass and re-stamp the observation's turn.
        if i.ttl_expired {
            return ReadAdvice::Pass { restamp: true };
        }
        // Immediate-second-ask hatch: same file, same content, already reminded
        // (or too small to bother) ⇒ pass, leaving the prior observation.
        if i.reminded || !i.big_enough {
            return ReadAdvice::Pass { restamp: false };
        }
        return ReadAdvice::Outline;
    }
    // Changed. A reminded file re-arms only while under the cap.
    if i.reminded && i.remind_count >= READ_REMIND_CAP {
        return ReadAdvice::Pass { restamp: true };
    }
    if i.diffs_on && i.have_snapshot && i.diff_worth_it {
        return ReadAdvice::Diff;
    }
    // Changed but no diff to offer (feature off / snapshot gone / near-rewrite)
    // ⇒ record-and-pass with the new observation, exactly as pre-V17.
    ReadAdvice::Pass { restamp: true }
}

/// V17 Phase C — the facts the first-read tier gates on, all cheap to compute in
/// `should_read`. Split out (like [`VerdictIn`]) so the eligibility decision is
/// unit-testable without an `AppHandle`.
struct FirstReadIn {
    /// `read_advisor_first_read_kb` (0 = tier off).
    first_read_kb: u32,
    /// Bytes of the already-read content.
    content_len: usize,
    /// A deliberate slice (`Read({offset})` / `{limit}`) is in play.
    slice: bool,
    /// The file parses to code (its outline is non-empty).
    is_code: bool,
}

/// V17 Phase C — does the first-read substitution tier APPLY to this never-seen
/// read? True only when the tier is enabled, the whole-file content is at or over
/// the KiB threshold, the read isn't a deliberate slice, and the file isn't code
/// (data/logs/lockfiles qualify, source never does). A `true` result still needs
/// a cached digest to actually remind — the caller does that impure lookup and
/// enqueues on a miss. Pure.
fn first_read_eligible(i: &FirstReadIn) -> bool {
    i.first_read_kb > 0
        && i.content_len >= (i.first_read_kb as usize).saturating_mul(1024)
        && !i.slice
        && !i.is_code
}

/// V24 Phase B (pure): the "open tabs + recency" active-session set — a session
/// is active when it has recent activity (`last_ms` within
/// [`LIVE_SESSION_RECENCY_MS`]) OR a fresh registry entry (`last_seen_ms` within
/// [`LIVE_SESSION_TTL_MS`]). The registry (`live`) is process-wide, so its
/// contribution is intersected with `sessions` (the queried root's known
/// sessions) to avoid leaking another project's live session into this
/// snapshot — a fresh entry whose session isn't in `sessions` has no row to
/// mark here anyway. Deduped; sorted for a stable payload. Free-standing so it's
/// unit-testable without an `AppHandle`.
fn compute_active_session_ids(
    live: &HashMap<String, LiveSession>,
    sessions: &[SessionUsageRow],
    now: i64,
) -> Vec<String> {
    let known: HashSet<&str> = sessions.iter().map(|r| r.session_id.as_str()).collect();
    let mut active: HashSet<String> = HashSet::new();
    // Recency half: any known session touched within the window.
    for r in sessions {
        if now.saturating_sub(r.last_ms) <= LIVE_SESSION_RECENCY_MS {
            active.insert(r.session_id.clone());
        }
    }
    // Registry half: fresh entries whose session belongs to this root.
    for e in live.values() {
        if now.saturating_sub(e.last_seen_ms) <= LIVE_SESSION_TTL_MS
            && known.contains(e.session_id.as_str())
        {
            active.insert(e.session_id.clone());
        }
    }
    let mut out: Vec<String> = active.into_iter().collect();
    out.sort();
    out
}

/// H1 fix (pure): is `tab`'s session binding UNPROVABLE because another RUNNING
/// tab of the same agent tails the same transcript root?
///
/// **The single implementation of the ambiguity predicate.** Every consumer of a
/// tab-keyed identity claim routes through it, so graph/memory scoping and
/// permission-hook attribution can never disagree about who is ambiguous.
///
/// Semantics:
///  * `false` when `tab` has no entry — an agent that does NOT bind by root
///    (OpenCode: per-tab SSE with the session id on the wire) never registers
///    here, so it is never degraded; likewise a tap that could not resolve a
///    root (no home dir) never marked a session either.
///  * `false` when exactly one running tab holds that root — the overwhelmingly
///    common case, unchanged.
///  * `true` from the moment a second running tab of the same agent registers
///    the same root, for as long as both entries are fresh.
///
/// TTL-filtered on both sides so a leaked/never-cleared entry cannot disable
/// scoping forever, and self-comparison is excluded by key. Free-standing so it
/// is unit-testable without an `AppHandle`.
fn tab_binding_is_ambiguous(
    roots: &HashMap<String, LiveTabRoot>,
    tab: &str,
    agent: &str,
    now: i64,
) -> bool {
    let fresh = |e: &LiveTabRoot| now.saturating_sub(e.last_seen_ms) <= LIVE_SESSION_TTL_MS;
    let Some(mine) = roots.get(tab).filter(|e| e.agent == agent).filter(|e| fresh(e)) else {
        return false;
    };
    roots
        .iter()
        .any(|(k, e)| k != tab && e.agent == agent && e.root == mine.root && fresh(e))
}

/// V28: the live session id reported by `tab`, or `None`. Pure half of
/// [`GraphService::live_session_for_tab`] so it can be unit-tested without an
/// `AppHandle`.
///
/// Deliberately strict, matching NC-2's resolver discipline: an EXACT key match
/// (no prefix/fuzzy tab matching), the entry's `agent` must equal the calling
/// agent (a tab id could in principle be reused across harnesses), and the entry
/// must still be inside [`LIVE_SESSION_TTL_MS`]. Anything else returns `None` —
/// the caller then falls back to today's most-recent-session behavior rather
/// than attributing a call to a session it can't prove.
///
/// H1 fix: also `None` when [`tab_binding_is_ambiguous`] holds — with two
/// running Claude tabs on one project the registry's answer is whichever session
/// wrote last, for BOTH tabs, so honoring it would put tab A's memory writes in
/// tab B's scope. Degrading to unscoped (V28 decision 4's documented fail-open)
/// is strictly better than a confidently wrong scope.
fn lookup_live_session_for_tab(
    live: &HashMap<String, LiveSession>,
    roots: &HashMap<String, LiveTabRoot>,
    tab: &str,
    agent: &str,
    now: i64,
) -> Option<String> {
    if tab_binding_is_ambiguous(roots, tab, agent, now) {
        return None;
    }
    live.get(tab)
        .filter(|e| e.agent == agent)
        .filter(|e| now.saturating_sub(e.last_seen_ms) <= LIVE_SESSION_TTL_MS)
        .map(|e| e.session_id.clone())
}

/// NC-2 + H1 fix (pure): the `(tab_id, session_id)` pairs the permission-hook
/// resolver may trust — every fresh CLAUDE registry entry MINUS the tabs whose
/// binding is ambiguous ([`tab_binding_is_ambiguous`]).
///
/// Dropping the pair (rather than the whole candidate) is what makes
/// `resolve_permission_tab` REFUSE instead of guess: with no session to match
/// on, its session/transcript passes find nothing, and its last-resort `cwd`
/// pass sees the ≥2 same-root tabs and declines too. That also closes the
/// launch-order window in which the registry held tab A → tab B's *fresh*
/// session **uniquely** (A's tap rotates onto B's new file and marks it live
/// before B's own tap confirms) — during that window both tabs are running on
/// one root, so the predicate is already true.
///
/// Pure half of [`GraphService::live_claude_sessions`].
fn live_claude_tab_sessions(
    live: &HashMap<String, LiveSession>,
    roots: &HashMap<String, LiveTabRoot>,
    now: i64,
) -> Vec<(String, String)> {
    live.iter()
        .filter(|(_, e)| {
            e.agent == "claude" && now.saturating_sub(e.last_seen_ms) <= LIVE_SESSION_TTL_MS
        })
        .filter(|(k, _)| !tab_binding_is_ambiguous(roots, k, "claude", now))
        .map(|(k, e)| (k.clone(), e.session_id.clone()))
        .collect()
}

/// H1 fix (pure): upsert the `tab` → transcript-root claim and stamp it fresh at
/// `now`, then evict entries whose last refresh has aged past
/// [`LIVE_SESSION_TTL_MS`] (a tap that died without running its RAII guard must
/// not leave a phantom co-tenant suppressing scoping forever).
///
/// **The single write site for [`LiveTabRoot`]**, and therefore the one place
/// the root is normalized (H1-R5): stored as a comparison key via
/// [`crate::fsutil::norm_dir_key_path`], so [`tab_binding_is_ambiguous`]'s
/// equality test — and any future reader — can never be defeated by two spellings
/// of one directory. Free-standing so it's unit-testable without an `AppHandle`.
fn upsert_live_tab_root(
    roots: &mut HashMap<String, LiveTabRoot>,
    tab: &str,
    agent: &str,
    root: &Path,
    now: i64,
) {
    let key = crate::fsutil::norm_dir_key_path(root);
    // Entry API so the steady-state refresh (drain tick + heartbeat) only
    // stamps `last_seen_ms` in place.
    roots
        .entry(tab.to_string())
        .and_modify(|e| {
            e.last_seen_ms = now;
            e.agent = agent.to_string();
            e.root = key.clone();
        })
        .or_insert_with(|| LiveTabRoot {
            agent: agent.to_string(),
            root: key,
            last_seen_ms: now,
        });
    roots.retain(|_, e| now.saturating_sub(e.last_seen_ms) <= LIVE_SESSION_TTL_MS);
}

/// V24 Phase B: drop live-session registry entries older than
/// [`LIVE_SESSION_TTL_MS`] — the registry half's cutoff. Called opportunistically
/// from [`GraphService::mark_live_session`] so the map doesn't grow without bound
/// (OpenCode keys have no cancel signal on the loopback path, so TTL is their
/// only reclamation). Safe because a TTL-stale entry is already ignored by
/// [`compute_active_session_ids`]'s registry half, and the recency half (the
/// younger [`LIVE_SESSION_RECENCY_MS`] window over recorded activity) covers any
/// session that is still genuinely active — so eviction can never change an
/// active-set result. Free-standing so it's unit-testable without an `AppHandle`.
fn evict_stale_live_sessions(live: &mut HashMap<String, LiveSession>, now: i64) {
    live.retain(|_, e| now.saturating_sub(e.last_seen_ms) <= LIVE_SESSION_TTL_MS);
}

/// V16 drift signals that need full-relation scans (`large_reread_pairs`
/// walks every read event + every symbol span; `claude_tokenless_sessions`
/// walks every usage row). The Overview's advice poll asks every 2s, but
/// these only change when new events land — cache per root with a short TTL
/// instead of rescanning per tick.
struct DriftDbSignals {
    at: Instant,
    large_reread_pairs: u64,
    claude_sessions: u64,
    claude_tokenless: u64,
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
    /// `pushes` is the V30 session-push bus
    /// ([`OffloadService::push_registry`](crate::offload::OffloadService::push_registry)).
    /// `None` disables the index-completion push entirely — the service is
    /// otherwise unchanged, so tests and standalone paths can construct it
    /// without an offload service.
    pub fn new(
        app: AppHandle,
        settings: SettingsHandle,
        pushes: Option<Arc<crate::offload::service::PushRegistry>>,
    ) -> Arc<Self> {
        Arc::new(Self {
            app,
            settings,
            pushes,
            indices: StdMutex::new(HashMap::new()),
            status: StdMutex::new(HashMap::new()),
            watchers: StdMutex::new(HashMap::new()),
            write_lock: StdMutex::new(()),
            paused: AtomicBool::new(false),
            backfill: StdMutex::new(HashMap::new()),
            greeted: StdMutex::new(HashSet::new()),
            injected: StdMutex::new(HashMap::new()),
            post_compaction: StdMutex::new(HashSet::new()),
            reminded: StdMutex::new(HashMap::new()),
            drift_signals: StdMutex::new(HashMap::new()),
            bypassed_advice_chars: AtomicU64::new(0),
            read_seen: StdMutex::new(ReadSeenStore::default()),
            read_seen_touch: AtomicU64::new(0),
            digest_inflight: StdMutex::new(HashSet::new()),
            auto_check_sessions: StdMutex::new(HashMap::new()),
            auto_check_runner: crate::checks::auto::RootRunner::new(),
            distilling: StdMutex::new(HashSet::new()),
            live_sessions: StdMutex::new(HashMap::new()),
            live_tab_roots: StdMutex::new(HashMap::new()),
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
        let snap = self.settings.current().graph;
        if !snap.semantic_search {
            return;
        }
        if let Ok(idx) = self.index_for(&root) {
            let cleared = {
                let _w = self.write_guard();
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
            // V11 Phase G: also drop code vectors when code embedding is on, so
            // "Rebuild embeddings" re-embeds both stores together rather than
            // leaving stale code vectors behind under a fresh doc epoch.
            if snap.embed_code_bodies {
                let cleared_code = {
                    let _w = self.write_guard();
                    idx.clear_code_vectors()
                };
                if let Err(e) = cleared_code {
                    self.patch_status(&root, |s| {
                        s.embed_state = "error".into();
                        s.embed_error = Some(format!("failed to clear code vectors: {e}"));
                    });
                    return;
                }
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
        let Some(mut embedder) = Embedder::new(&snap.embedding_endpoint, &snap.embedding_model)
        else {
            return EmbedderProbe {
                ok: false,
                dim: None,
                message: "No embedding endpoint configured.".into(),
            };
        };
        // "Test connection" is user-initiated and expects a round-trip, so it
        // may probe `/props` — and by seeding the process cache it hands the
        // detected budget to the query paths before any backfill has run.
        let limit = embedder.ensure_max_tokens(snap.embedding_max_tokens).await;
        match embedder.probe_dim().await {
            Ok(dim) => EmbedderProbe {
                ok: true,
                dim: Some(dim),
                message: match limit {
                    Some(t) => format!("Reachable — {dim}-dim embeddings, {t}-token input budget."),
                    None => format!(
                        "Reachable — {dim}-dim embeddings (no token budget detected; \
                         set one in Settings → Code graph if the server rejects long chunks)."
                    ),
                },
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
    ///
    /// V28: `session` is the EXPLICIT session id the caller's tab resolved to
    /// (see [`Self::live_session_for_tab`]); the `context_*` memory tools scope
    /// to it instead of "the most recent session for this agent". `None` keeps
    /// exactly the pre-V28 behavior.
    ///
    /// V32 Phase C2: `taint` is the loopback taint latch's verdict for THIS
    /// call ([`Latch::proxy_gate`](crate::offload::toolclass::Latch::proxy_gate)).
    /// It is consumed by `context_note` alone — a
    /// [`Quarantined`](crate::offload::toolclass::WriteTaint::Quarantined) write
    /// is stored with the `tainted` flag instead of entering project memory. It
    /// is threaded rather than re-derived here because only the proxy holds the
    /// per-tab latch state; the graph layer must never guess it.
    ///
    /// V32 Phase G widens that one verdict into
    /// [`CallGuards`](crate::offload::toolclass::CallGuards) — the taint plus
    /// whether recalled memory is spotlight-enveloped — for the same reason:
    /// both are resolved at the proxy, which is the only layer that knows the
    /// calling TAB and can therefore resolve the three-level hierarchy at the
    /// right scope.
    pub async fn run_graph_tool(
        &self,
        cwd: &Path,
        name: &str,
        args: &serde_json::Value,
        consumer: &str,
        session: Option<&str>,
        guards: crate::offload::toolclass::CallGuards,
    ) -> Result<String, String> {
        let settings = self.settings.current();
        let sub = settings.graph.effective_db_subdir();
        // The consumer selects the activity source + the memory tools' agent
        // scope, so an OpenCode tab's graph/context calls don't read as Claude's.
        let source = super::mcp::source_for_consumer(consumer);

        // `run_check` (V12 Phase A) needs a project root but not a built code
        // graph — resolve root the same way graph tools do when a graph.db
        // exists, else fall back to `cwd` itself, and skip opening an index
        // entirely (same rationale as `mcp::handle_call`'s special case).
        if name == "run_check" {
            let root = super::mcp::find_graph_root(cwd, &sub).unwrap_or_else(|| cwd.to_path_buf());
            return super::mcp::run_check_tool(&root, &settings, source, args).await;
        }

        let root = super::mcp::find_graph_root(cwd, &sub)
            .ok_or_else(|| format!("no code graph found from {}", cwd.display()))?;
        let idx = self.index_for(&root).map_err(|e| e.to_string())?;
        super::mcp::dispatch_recorded(&root, &idx, &settings, source, name, args, session, guards)
            .await
    }

    /// The configured per-project db subdirectory (default `.cimp`).
    pub(super) fn db_subdir(&self) -> String {
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
    ///
    /// The root is CANONICALIZED for both the cache key and the open. Callers
    /// reach the same project under different spellings — the loopback
    /// canonicalizes its root to the `\\?\` verbatim form while IPC and the
    /// taps pass the plain one — and a raw-`PathBuf` key would open a SECOND
    /// cozo storage over the same SQLite file inside one process, with
    /// independent locks (the invariant this guards: one file, one handle).
    /// Same fall-back-to-literal posture as `crate::activity::root_key`.
    /// Scoped strictly to the `indices` cache: `status`, the watchers and the
    /// event payloads stay keyed by the caller's original spelling, which is
    /// what the UI displays.
    fn index_for(&self, root: &Path) -> AppResult<Arc<GraphIndex>> {
        // The caller's spelling is kept for the rebuild hand-off below, which
        // feeds `status`/watchers/events — those stay caller-keyed.
        let spelled = root.to_path_buf();
        let (idx, migrated) = warm_index(&self.indices, root, &self.db_subdir())?;

        if migrated {
            // A pre-upgrade store was emptied to fix its shape. Repopulate it now
            // (via the managed service Arc) so a non-launch root touched only by a
            // query or the memory tap doesn't stay silently empty.
            if let Some(svc) = self.app.try_state::<Arc<GraphService>>() {
                tracing::info!(root = %spelled.display(), "graph: schema migrated — rebuilding");
                // Repair work nobody asked for — never announces itself.
                svc.inner()
                    .clone()
                    .spawn_rebuild(spelled, RebuildOrigin::Automatic);
            }
        }
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

    /// The project's language census for `root` — every language present on
    /// disk with its file count and green/yellow/red classification (drives the
    /// Code Graph tab's language buttons). Walks the tree fresh each call, so
    /// callers should invoke it on tab open and after a rebuild, not on a tight
    /// poll.
    pub fn language_census(&self, root: &Path) -> Vec<LangCensus> {
        let snap = self.settings.current().graph;
        language_census(root, &snap, &self.db_subdir())
    }

    /// V10: candidate dead exports for `root` — public symbols with no reference
    /// and no inbound call edge. Opens the warm index; bounded by
    /// `max_rows_per_query`. Candidates only (see [`GraphIndex::dead_exports`]).
    pub fn dead_exports(&self, root: &Path) -> AppResult<Vec<SymbolHit>> {
        let max = self.settings.current().graph.max_rows_per_query.max(1) as usize;
        self.index_for(root)?.dead_exports(max)
    }

    /// V10: import cycles between files under `root` (each a loop of ≥ 2 files).
    /// Opens the warm index; bounded by `max_rows_per_query`.
    pub fn import_cycles(&self, root: &Path) -> AppResult<Vec<Vec<String>>> {
        let max = self.settings.current().graph.max_rows_per_query.max(1) as usize;
        self.index_for(root)?.import_cycles(max)
    }

    /// V12 Phase B (Analyses): working-tree impact — symbols changed since
    /// `HEAD` plus their transitive dependents. Diff mode only (the
    /// `symbols`-scoped mode is MCP-tool only, where an agent picks explicit
    /// roots — see `graph::mcp::run_impact`). Opens the warm index; bounded
    /// by `max_rows_per_query`; the default depth (3) matches the tool's.
    pub fn impact(&self, root: &Path) -> AppResult<super::impact::ImpactReport> {
        let max = self.settings.current().graph.max_rows_per_query.max(1) as usize;
        let idx = self.index_for(root)?;
        let set = super::impact::changed_symbols(root, &idx)?;
        let mut names: Vec<String> = set.changed.iter().map(|s| s.name.clone()).collect();
        names.sort();
        names.dedup();
        let dependents = if names.is_empty() {
            Vec::new()
        } else {
            idx.dependents_transitive(&names, 3, max, None)?
        };
        Ok(super::impact::ImpactReport {
            changed: set.changed,
            dependents,
            unindexed: set.unindexed,
        })
    }

    /// V15 Feature 1: the shortest path between two entities across the code
    /// edge kinds. Opens the warm index; `max_hops` from `path_max_hops`
    /// (clamped 1–32). `None` when unresolvable or no path within the bound.
    pub fn shortest_path(
        &self,
        root: &Path,
        from: &str,
        to: &str,
        kinds: &[super::model::EdgeKind],
        symmetric: bool,
    ) -> AppResult<Option<super::index::PathHit>> {
        let hops = (self.settings.current().graph.path_max_hops.max(1) as usize).min(32);
        self.index_for(root)?
            .shortest_path(from, to, kinds, hops, symmetric)
    }

    /// V15 Feature 2: the architecture overview (god nodes, subsystems,
    /// surprising edges). Opens the warm index; bounded by the arch settings.
    pub fn architecture(&self, root: &Path) -> AppResult<super::index::ArchReport> {
        let g = self.settings.current().graph;
        let max = g.max_rows_per_query.max(1) as usize;
        self.index_for(root)?.architecture(
            g.arch_max_communities as usize,
            g.arch_min_community_size as usize,
            max,
        )
    }

    /// V15 Feature 4: a bounded subgraph for the Graph View tab. Opens the warm
    /// index; capped at `graph_viz_max_nodes`.
    pub fn viz_snapshot(&self, root: &Path) -> AppResult<super::index::VizGraph> {
        let max = self.settings.current().graph.graph_viz_max_nodes.max(1) as usize;
        self.index_for(root)?.viz_snapshot(max)
    }

    /// Workbench ⌖ support: per-file Graph View presence (indexed? rolled-up
    /// degree?) for a batch of repo-relative paths — drives the jump button's
    /// enabled state. Opens the warm index.
    pub fn viz_file_status(
        &self,
        root: &Path,
        paths: &[String],
    ) -> AppResult<Vec<super::index::VizFileStatus>> {
        self.index_for(root)?.viz_file_status(paths)
    }

    /// Workbench ⌖ support: the 1-hop FILE ego of `path` regardless of the
    /// snapshot's top-N-by-degree cut, so the Graph View can temporarily
    /// inject a hidden file and its neighbors. Opens the warm index.
    pub fn viz_ego(&self, root: &Path, path: &str) -> AppResult<super::index::VizGraph> {
        self.index_for(root)?.viz_ego(path)
    }

    // ── V10 session / action memory ──────────────────────────────────────

    /// Record one memory event for `root`'s current-project graph. A no-op when
    /// the graph is disabled. Best-effort — a store error is logged, never
    /// propagated (memory must never break the agent's turn). Called in-process
    /// from the Claude transcript tap and via the `/memory/event` loopback route.
    #[allow(clippy::too_many_arguments)]
    pub fn record_mem_event(
        &self,
        root: &Path,
        session_id: &str,
        agent: &str,
        kind: &str,
        path: &str,
        symbol: Option<&str>,
        line: Option<u32>,
        detail: Option<&str>,
    ) {
        if !self.settings.current().graph.enabled {
            return;
        }
        // Store paths project-relative with `/` separators so memory paths match
        // the graph's stored file paths (agents send absolute or `\`-separated
        // paths). A pattern/command that isn't under root passes through.
        let rel = relativize_path(root, path);
        let ts = crate::activity::now_ms() as i64;
        match self.index_for(root) {
            Ok(idx) => {
                if let Err(e) =
                    idx.record_mem_event(session_id, agent, kind, &rel, symbol, line, ts, detail)
                {
                    debug!(error = %e, "graph: record_mem_event failed");
                }
            }
            Err(e) => debug!(error = %e, "graph: record_mem_event open failed"),
        }
    }

    /// Record one git commit caught live from an agent transcript (the OOB
    /// tap saw the `git commit` tool call and parsed the produced hash out
    /// of its output). Same best-effort posture as [`Self::record_mem_event`]:
    /// no-op when the graph is disabled, store errors logged, never
    /// propagated.
    pub fn record_session_commit(&self, root: &Path, session_id: &str, hash: &str) {
        if !self.settings.current().graph.enabled {
            return;
        }
        let ts = crate::activity::now_ms() as i64;
        match self.index_for(root) {
            Ok(idx) => {
                if let Err(e) = idx.record_session_commit(session_id, hash, ts) {
                    debug!(error = %e, "graph: record_session_commit failed");
                }
            }
            Err(e) => debug!(error = %e, "graph: record_session_commit open failed"),
        }
    }

    /// Every commit hash recorded for `session_id` (git-printed, usually
    /// short — match by prefix), oldest first. Empty on any store error.
    pub fn session_commit_hashes(&self, root: &Path, session_id: &str) -> Vec<String> {
        let idx = match self.index_for(root) {
            Ok(idx) => idx,
            Err(e) => {
                debug!(error = %e, "graph: session_commit_hashes open failed");
                return Vec::new();
            }
        };
        idx.session_commit_hashes(session_id)
            .inspect_err(|e| debug!(error = %e, "graph: session_commit_hashes failed"))
            .unwrap_or_default()
    }

    /// Recorded commit hashes for every session (session_id → hashes) in one
    /// scan. Empty on any store error.
    pub fn session_commit_hashes_all(
        &self,
        root: &Path,
    ) -> std::collections::HashMap<String, Vec<String>> {
        let idx = match self.index_for(root) {
            Ok(idx) => idx,
            Err(e) => {
                debug!(error = %e, "graph: session_commit_hashes_all open failed");
                return Default::default();
            }
        };
        idx.session_commit_hashes_all()
            .inspect_err(|e| debug!(error = %e, "graph: session_commit_hashes_all failed"))
            .unwrap_or_default()
    }

    /// The graph's own `(started_ms, last_ms)` window for every session —
    /// the CANONICAL session windows (the `session` relation), fresher than
    /// any frontend snapshot for the live session. Empty on any store error.
    pub fn session_windows(&self, root: &Path) -> std::collections::HashMap<String, (i64, i64)> {
        let idx = match self.index_for(root) {
            Ok(idx) => idx,
            Err(e) => {
                debug!(error = %e, "graph: session_windows open failed");
                return Default::default();
            }
        };
        idx.mem_sessions()
            .inspect_err(|e| debug!(error = %e, "graph: session_windows failed"))
            .unwrap_or_default()
            .into_iter()
            .map(|s| (s.session_id, (s.started_ms, s.last_ms)))
            .collect()
    }

    // ── V14 Phase C: usage / cost accounting ──────────────────────────────

    /// Record one usage/cost event for `root`'s current-project graph. A
    /// no-op when the graph is disabled — usage rides `graph.db`, so without
    /// it the token X-ray is simply unavailable (same posture as memory).
    /// Best-effort: a store error is logged, never propagated. Called
    /// in-process from the Claude transcript tap and via the `/memory/event`
    /// loopback route (OpenCode's usage tap — see the C3 spike note atop
    /// `oob/opencode.rs`).
    pub fn record_usage(&self, root: &Path, session_id: &str, agent: &str, event: UsageEvent) {
        if !self.settings.current().graph.enabled {
            return;
        }
        let ts = crate::activity::now_ms() as i64;
        let idx = match self.index_for(root) {
            Ok(idx) => idx,
            Err(e) => {
                debug!(error = %e, "graph: record_usage_event open failed");
                return;
            }
        };
        if let Err(e) = idx.record_usage_event(session_id, agent, &event, ts) {
            debug!(error = %e, "graph: record_usage_event failed");
        }
    }

    /// Summed token totals for `session_id` ("turn" rows only). Empty
    /// defaults on any store error (graph disabled, session unknown, etc.).
    /// Best-effort like the wrappers below it, but never SILENT: every
    /// swallowed store error in this block is traced, so a degraded store is
    /// diagnosable from the log even where the return type can't carry it.
    pub fn usage_session_totals(&self, root: &Path, session_id: &str) -> UsageTotals {
        let idx = match self.index_for(root) {
            Ok(idx) => idx,
            Err(e) => {
                debug!(error = %e, "graph: usage_session_totals open failed");
                return UsageTotals::default();
            }
        };
        idx.usage_session_totals(session_id)
            .inspect_err(|e| debug!(error = %e, "graph: usage_session_totals failed"))
            .unwrap_or_default()
    }

    /// Per-turn token/tool-char series for `session_id`, oldest → newest.
    pub fn usage_turn_series(&self, root: &Path, session_id: &str) -> Vec<TurnUsage> {
        let idx = match self.index_for(root) {
            Ok(idx) => idx,
            Err(e) => {
                debug!(error = %e, "graph: usage_turn_series open failed");
                return Vec::new();
            }
        };
        idx.usage_turn_series(session_id)
            .inspect_err(|e| debug!(error = %e, "graph: usage_turn_series failed"))
            .unwrap_or_default()
    }

    /// Per-session usage totals + cache-hit ratio + `est_only` for every
    /// known session under `root` (drives the Usage section's project totals
    /// table). Unlike its sibling wrappers this one PROPAGATES the store
    /// error instead of defaulting to empty: an open failure and a query
    /// failure are both `Err`, an empty store is `Ok(vec![])`. That
    /// distinction is the whole point — a swallowed error renders as
    /// "0 sessions", which reads as a healthy empty project (see
    /// `usage_snapshot`'s `store_error`).
    pub fn usage_all_sessions(&self, root: &Path) -> AppResult<Vec<SessionUsageRow>> {
        self.index_for(root)?.usage_all_sessions()
    }

    /// V14 Phase D: per-tool ranking (est. tokens + call count) for
    /// `session_id`, descending.
    pub fn usage_tool_ranking(&self, root: &Path, session_id: &str) -> Vec<ToolUsage> {
        let idx = match self.index_for(root) {
            Ok(idx) => idx,
            Err(e) => {
                debug!(error = %e, "graph: usage_tool_ranking open failed");
                return Vec::new();
            }
        };
        idx.usage_tool_ranking(session_id)
            .inspect_err(|e| debug!(error = %e, "graph: usage_tool_ranking failed"))
            .unwrap_or_default()
            .into_iter()
            .map(|(tool, chars, calls)| ToolUsage {
                tool,
                est_tokens: chars / 4,
                calls,
            })
            .collect()
    }

    /// V14 Phase D: the Usage section's on-demand payload for `root` — the
    /// current session's per-turn series + top-tools ranking, every known
    /// session's totals row, and the effectiveness counters.
    /// `offload_local_tasks` is left at `0` — the `graph_usage` IPC handler
    /// fills it in from the (separate) `OffloadService`, which this module
    /// has no dependency on.
    ///
    /// `store_error` carries the one failure the UI cannot infer from the
    /// payload: an unopenable/unqueryable store yields the same empty
    /// `sessions` list as a project that simply has none yet. It is set ONLY
    /// from the sessions path (open failure or `usage_all_sessions` error) —
    /// the `current`-session sub-queries stay best-effort defaults (traced,
    /// not surfaced), because a partial `current` is still a truthful view of
    /// a working store.
    pub fn usage_snapshot(&self, root: &Path) -> UsageSnapshot {
        let effectiveness = self.effectiveness_totals(root);
        let idx = match self.index_for(root) {
            Ok(idx) => idx,
            Err(e) => {
                debug!(error = %e, "graph: usage_snapshot open failed");
                return UsageSnapshot {
                    current: None,
                    sessions: Vec::new(),
                    effectiveness,
                    offload_local_tasks: 0,
                    surface: Default::default(),
                    active_session_ids: Vec::new(),
                    store_error: Some(e.to_string()),
                };
            }
        };
        // Reuses the per-root wrapper methods above (not `idx` directly) so
        // this is the one place that consumes them — same "one caller,
        // exercised for real" posture the rest of the service follows.
        let current = idx
            .mem_current_session()
            .ok()
            .flatten()
            .map(|sid| SessionUsage {
                turns: self.usage_turn_series(root, &sid),
                totals: self.usage_session_totals(root, &sid),
                top_tools: self.usage_tool_ranking(root, &sid),
                session_id: sid,
            });
        let (sessions, store_error) = match self.usage_all_sessions(root) {
            Ok(rows) => (rows, None),
            Err(e) => {
                debug!(error = %e, "graph: usage_snapshot sessions failed");
                (Vec::new(), Some(e.to_string()))
            }
        };
        let active_session_ids =
            self.active_session_ids(&sessions, crate::activity::now_ms() as i64);
        UsageSnapshot {
            current,
            sessions,
            effectiveness,
            offload_local_tasks: 0,
            surface: Default::default(),
            active_session_ids,
            store_error,
        }
    }

    /// V24 Phase B: full drill-in detail for `session_id` under `root` — the
    /// same shape `graph_usage` gives the current session, but for ANY session.
    /// An unknown session (no `session` row, or the graph is off) yields
    /// [`SessionUsageDetail::empty`]. Best-effort: a store error yields empties,
    /// never an error (matches the other usage wrappers). Reuses the per-root
    /// wrapper methods, same posture as [`Self::usage_snapshot`].
    pub fn session_usage_detail(&self, root: &Path, session_id: &str) -> SessionUsageDetail {
        let Some(row) = self.usage_session_row(root, session_id) else {
            return SessionUsageDetail::empty(session_id);
        };
        SessionUsageDetail {
            row,
            turns: self.usage_turn_series(root, session_id),
            top_tools: self.usage_tool_ranking(root, session_id),
            per_model: self.usage_session_model_totals(root, session_id),
        }
    }

    /// V24 Phase B: the single totals row for `session_id`, or `None` when the
    /// session is unknown (or the graph is off / a store error). Same shape as
    /// one entry of [`Self::usage_all_sessions`].
    pub fn usage_session_row(&self, root: &Path, session_id: &str) -> Option<SessionUsageRow> {
        let idx = self
            .index_for(root)
            .inspect_err(|e| debug!(error = %e, "graph: usage_session_row open failed"))
            .ok()?;
        idx.usage_session_row(session_id)
            .inspect_err(|e| debug!(error = %e, "graph: usage_session_row failed"))
            .ok()
            .flatten()
    }

    /// V24 Phase B: per-model token totals + session/agent origin split for
    /// `session_id`, ordered by tokens desc. Empty on any store error.
    pub fn usage_session_model_totals(&self, root: &Path, session_id: &str) -> Vec<ModelUsage> {
        let idx = match self.index_for(root) {
            Ok(idx) => idx,
            Err(e) => {
                debug!(error = %e, "graph: usage_session_model_totals open failed");
                return Vec::new();
            }
        };
        idx.usage_session_model_totals(session_id)
            .inspect_err(|e| debug!(error = %e, "graph: usage_session_model_totals failed"))
            .unwrap_or_default()
    }

    /// V24 Phase B: upsert a live-session registry entry, keyed by `key` (a
    /// Claude tab id, or the reporting session id on the OpenCode loopback
    /// path), stamping `last_seen_ms` to now. Called on every Claude drain tick
    /// and every OpenCode `/memory/event` — cheap and idempotent.
    pub fn mark_live_session(&self, key: &str, agent: &str, session_id: &str) {
        let now = crate::activity::now_ms() as i64;
        if let Ok(mut m) = self.live_sessions.lock() {
            // Entry API so the steady-state 200ms Claude drain tick only stamps
            // `last_seen_ms` in place — no per-tick allocation of a fresh entry.
            m.entry(key.to_string())
                .and_modify(|e| {
                    e.last_seen_ms = now;
                    // The reported session/agent can rotate under a stable key
                    // (a Claude tab keyed by its tab id) — keep them current.
                    e.session_id = session_id.to_string();
                    e.agent = agent.to_string();
                })
                .or_insert_with(|| LiveSession {
                    agent: agent.to_string(),
                    session_id: session_id.to_string(),
                    last_seen_ms: now,
                });
            // Opportunistic eviction so OpenCode session keys — which have no
            // cancel signal on the loopback path — can't accumulate forever.
            evict_stale_live_sessions(&mut m, now);
        }
    }

    /// V24 Phase B: drop a live-session registry entry by `key` — the Claude
    /// tap calls this on tab cancel so a closed tab stops being reported active
    /// before its TTL lapses. OpenCode has no tab binding on the loopback path,
    /// so its entries rely on TTL expiry alone.
    ///
    /// H1 fix: also drops the tab's [`LiveTabRoot`] — the two facts have exactly
    /// one lifetime (this tab's tap is running), and clearing them together is
    /// what lets a *closed* second tab stop suppressing the survivor's scoping
    /// immediately rather than after the TTL. A no-op for keys that never
    /// registered a root (every OpenCode key).
    pub fn clear_live_session(&self, key: &str) {
        if let Ok(mut m) = self.live_sessions.lock() {
            m.remove(key);
        }
        if let Ok(mut m) = self.live_tab_roots.lock() {
            m.remove(key);
        }
    }

    /// H1 fix: record that the tab keyed `tab` is RUNNING `agent` and binds its
    /// session identity from the transcript source `root` — see [`LiveTabRoot`]
    /// and [`tab_binding_is_ambiguous`]. Called from the tab's out-of-band tap
    /// on every poll tick (cheap, idempotent, keeps the entry inside
    /// [`LIVE_SESSION_TTL_MS`]); cleared by the tap's RAII guard via
    /// [`Self::clear_live_session`].
    ///
    /// Only agents whose binding is root-derived call this: registering an entry
    /// is what makes a tab *eligible* to be found ambiguous, so an agent that
    /// binds correctly per-tab (OpenCode) must stay absent.
    ///
    /// H1-R2: also called on a fixed cadence by the tap's heartbeat, so a drain
    /// loop parked in TTS backpressure can't let the claim age out (see
    /// [`LIVE_SESSION_TTL_MS`]). Idempotent and cheap either way; the decision
    /// (including the H1-R5 key normalization) lives in [`upsert_live_tab_root`].
    pub fn mark_live_tab_root(&self, tab: &str, agent: &str, root: &Path) {
        let now = crate::activity::now_ms() as i64;
        if let Ok(mut m) = self.live_tab_roots.lock() {
            upsert_live_tab_root(&mut m, tab, agent, root, now);
        }
    }

    /// NC-2 (issue #5): the live-session registry entries reported by CLAUDE
    /// tabs — `(tab_id, session_id)` per entry still inside
    /// [`LIVE_SESSION_TTL_MS`]. This is the session→tab mapping the
    /// `/permission/event` route resolves a hook payload with: a Claude entry
    /// is keyed by its stable TAB ID (see [`Self::mark_live_session`]) and
    /// carries the session id the tab's transcript tail last saw, which is
    /// exactly the `session_id` a Claude Code hook payload names.
    ///
    /// Stale (TTL-lapsed) entries are filtered out rather than returned, so a
    /// closed tab whose entry hasn't been reclaimed yet can never be credited
    /// with a live session's permission prompt. H1 fix: tabs whose binding is
    /// ambiguous are filtered out too — see [`live_claude_tab_sessions`].
    pub fn live_claude_sessions(&self) -> Vec<(String, String)> {
        let now = crate::activity::now_ms() as i64;
        let Ok(live) = self.live_sessions.lock() else {
            return Vec::new();
        };
        let Ok(roots) = self.live_tab_roots.lock() else {
            return Vec::new();
        };
        live_claude_tab_sessions(&live, &roots, now)
    }

    /// V28 (issue #13): the session id the tab keyed `tab` currently reports,
    /// for `agent` (`"claude"` / `"opencode"`) — the read-side identity the
    /// `context_*` memory tools scope to, so two same-agent tabs on one project
    /// stop sharing a memory scope.
    ///
    /// This is the generalization of [`Self::live_claude_sessions`] (which stays
    /// as-is — the `/permission/event` route needs the whole Claude mapping, not
    /// one tab). `None` means "no proof": no entry under that key, an entry left
    /// by a different agent, or a TTL-stale one. Every caller fails OPEN on
    /// `None` — back to `mem_current_session_for(agent)` — so a missing/unknown/
    /// stale tab can never error a tool call. H1 fix: an AMBIGUOUS tab (two
    /// running same-agent tabs on one transcript root) is `None` as well — same
    /// fail-open, see [`tab_binding_is_ambiguous`].
    pub fn live_session_for_tab(&self, tab: &str, agent: &str) -> Option<String> {
        let now = crate::activity::now_ms() as i64;
        let live = self.live_sessions.lock().ok()?;
        let roots = self.live_tab_roots.lock().ok()?;
        lookup_live_session_for_tab(&live, &roots, tab, agent, now)
    }

    /// V24 Phase B: the "open tabs + recency" active set for `sessions` at
    /// `now`, from the live-session registry. Locks the registry and delegates
    /// the decision to [`compute_active_session_ids`] (pure, unit-tested).
    fn active_session_ids(&self, sessions: &[SessionUsageRow], now: i64) -> Vec<String> {
        match self.live_sessions.lock() {
            Ok(m) => compute_active_session_ids(&m, sessions, now),
            Err(_) => Vec::new(),
        }
    }

    /// V14 Phase D: sum of the honest injected/deduped-char counters
    /// accumulated in the in-memory injection-dedup state (V11-C), across
    /// every session currently resident there, plus the read-advisor's
    /// displaced chars (V11-E Activity events — see [`Effectiveness`]'s doc
    /// comment for why this reads the Activity ring rather than
    /// `usage_stat`). Process-wide and non-durable, like `injected` itself —
    /// a restart loses the running total; good enough for an honest
    /// "since-restart" readout, not a permanent ledger.
    ///
    /// V14 code-review fix (FIX 7): `injected`/`deduped` are now scoped to
    /// `root`'s own sessions (`idx.mem_sessions()`), same as
    /// [`Self::injection_follow_rate`]/[`Self::budget_maxed_rate`] —
    /// previously this summed the WHOLE process-wide `injected` map, so a
    /// multi-project cImp session would attribute every OTHER project's
    /// injected/deduped chars to whichever root happened to call
    /// `usage_snapshot`.
    ///
    /// note: `advisor_displaced_chars` is NOT root-scoped — the Activity
    /// store (`crate::activity::snapshot`) is a single process-wide buffer
    /// with no root/session key to filter on, unlike `injected`. Left as a
    /// process-wide estimate; the UI already labels this figure `est.`.
    /// The store persists across restarts now, so the sum additionally
    /// filters to entries recorded since this process started — keeping the
    /// "since-restart" semantics this readout documents.
    fn effectiveness_totals(&self, root: &Path) -> Effectiveness {
        let since = crate::activity::process_start_ms();
        let activity = crate::activity::snapshot();
        let advisor_displaced_chars: u64 = activity
            .iter()
            .filter(|c| c.source == "read_advisor" && c.tool == "remind" && c.ts_ms >= since)
            .map(|c| c.chars as u64)
            .sum();
        // V16 Feature 4 honest-accounting: a remind the agent answered with a
        // shell read displaced nothing. `bypassed_chars` (whole-file, from
        // the Activity audit events) is what the shell re-read actually
        // re-spent; `bypassed_advice_chars` (reminder text, from the atomic
        // counter `check_bypass` feeds) is the like-for-like amount to net
        // out of `advisor_displaced_chars`, which also sums reminder text —
        // subtracting file sizes from text sizes would let one big-file
        // bypass zero the whole metric.
        let bypassed_chars: u64 = activity
            .iter()
            .filter(|c| c.source == "read_advisor" && c.tool == "bypass" && c.ts_ms >= since)
            .map(|c| c.chars as u64)
            .sum();
        let bypassed_advice_chars = self.bypassed_advice_chars.load(Ordering::Relaxed);
        let root_sessions: HashSet<String> = self
            .index_for(root)
            .ok()
            .and_then(|idx| idx.mem_sessions().ok())
            .map(|sessions| sessions.into_iter().map(|s| s.session_id).collect())
            .unwrap_or_default();
        let map = self.injected.lock().unwrap();
        let (injected_chars, deduped_chars, compounded_chars) = map
            .iter()
            .filter(|(sid, _)| root_sessions.contains(sid.as_str()))
            .fold((0u64, 0u64, 0u64), |(i, d, c), (_, st)| {
                (
                    i + st.injected_chars,
                    d + st.deduped_chars,
                    c + st.compounded_chars,
                )
            });
        Effectiveness {
            injected_chars,
            deduped_chars,
            advisor_displaced_chars,
            bypassed_chars,
            bypassed_advice_chars,
            compounded_chars,
        }
    }

    /// V14 Phase D2: fraction of files injected in full this project (across
    /// every session currently resident in the in-memory dedup state —
    /// V11-C) that were LATER read or edited in that same session (V10
    /// `mem_event`), plus the sample count (distinct injected-file
    /// instances) it's based on. Sessions in the in-memory map that don't
    /// belong to `root` (their id doesn't appear in `root`'s own
    /// `mem_sessions`) are excluded — the map itself isn't root-scoped, only
    /// keyed by session id. `None` when nothing has been injected for this
    /// root yet (context injection never fired, or the graph is off).
    pub fn injection_follow_rate(&self, root: &Path) -> Option<(f64, u64)> {
        let idx = self.index_for(root).ok()?;
        let root_sessions: HashSet<String> = idx
            .mem_sessions()
            .ok()?
            .into_iter()
            .map(|s| s.session_id)
            .collect();
        // Snapshot (session, injected paths) under the lock, then release it
        // before the per-session DB round trips — holding `injected` across
        // `mem_touched_paths` serializes all injection-map access behind SQLite
        // lookups on the hot injection path.
        let snapshot: Vec<(String, Vec<String>)> = {
            let map = self.injected.lock().unwrap();
            map.iter()
                .filter(|(sid, _)| root_sessions.contains(sid.as_str()))
                .map(|(sid, st)| (sid.clone(), st.files.keys().cloned().collect()))
                .collect()
        };
        let mut total = 0u64;
        let mut followed = 0u64;
        for (sid, paths) in snapshot {
            let touched = idx.mem_touched_paths(&sid).unwrap_or_default();
            for path in paths {
                total += 1;
                if touched.contains(&path) {
                    followed += 1;
                }
            }
        }
        if total == 0 {
            None
        } else {
            Some((followed as f64 / total as f64, total))
        }
    }

    /// V14 Phase D2: fraction of retrieval turns whose injected digest filled
    /// at least 90% of `context_turn_budget_chars` — the advisor's "the
    /// budget is maxed out" signal (rule 3's second half). Sample count =
    /// turns observed; same root-session scoping as
    /// [`Self::injection_follow_rate`].
    pub fn budget_maxed_rate(&self, root: &Path) -> Option<(f64, u64)> {
        let idx = self.index_for(root).ok()?;
        let root_sessions: HashSet<String> = idx
            .mem_sessions()
            .ok()?
            .into_iter()
            .map(|s| s.session_id)
            .collect();
        let map = self.injected.lock().unwrap();
        let mut seen = 0u64;
        let mut maxed = 0u64;
        for (sid, st) in map.iter() {
            if !root_sessions.contains(sid) {
                continue;
            }
            seen += st.turns_seen as u64;
            maxed += st.turns_maxed as u64;
        }
        if seen == 0 {
            None
        } else {
            Some((maxed as f64 / seen as f64, seen))
        }
    }

    /// V14 Phase D2: wraps [`GraphIndex::advisor_reread_rate`] — this query
    /// is already fully root+session scoped (it reads `mem_event` from
    /// `root`'s own index), unlike the two signals above.
    pub fn advisor_reread_rate(&self, root: &Path) -> Option<(f64, u64)> {
        self.index_for(root)
            .ok()?
            .advisor_reread_rate()
            .ok()
            .flatten()
    }

    /// V17 Phase F1: wraps [`GraphIndex::redundant_read_candidates`] — root+
    /// session scoped like [`Self::advisor_reread_rate`]. Returns `(redundant
    /// same-file re-read pairs, distinct sessions scanned)` over the most
    /// recent `last_sessions` sessions; `None` when no reads exist.
    pub fn redundant_read_candidates(
        &self,
        root: &Path,
        min_lines: u32,
        last_sessions: usize,
    ) -> Option<(u64, u64)> {
        self.index_for(root)
            .ok()?
            .redundant_read_candidates(min_lines, last_sessions)
            .ok()
            .flatten()
    }

    /// V14 Phase D2: how many distinct sessions this root's memory knows
    /// about — the advisor's "≥5 sessions" half of the cold-start floor.
    pub fn advisor_session_count(&self, root: &Path) -> u64 {
        let Ok(idx) = self.index_for(root) else {
            return 0;
        };
        idx.mem_sessions().map(|v| v.len() as u64).unwrap_or(0)
    }

    /// Ranked working set for a session (default: the current session).
    pub fn mem_working_set(
        &self,
        root: &Path,
        session_id: Option<&str>,
        max: usize,
    ) -> Vec<WorkingSetEntry> {
        let Ok(idx) = self.index_for(root) else {
            return Vec::new();
        };
        // Treat an empty session id as "unspecified" so a hook/plugin that sends
        // "" (rather than omitting the field) still falls back to the current
        // session instead of querying a session literally named "".
        let sid = match session_id.filter(|s| !s.is_empty()) {
            Some(s) => s.to_string(),
            None => match idx.mem_current_session().ok().flatten() {
                Some(s) => s,
                None => return Vec::new(),
            },
        };
        idx.mem_working_set(&sid, max).unwrap_or_default()
    }

    /// Pin/unpin a note.
    pub fn mem_set_note_pinned(&self, root: &Path, note_id: &str, pin: bool) -> AppResult<()> {
        self.index_for(root)?.mem_set_note_pinned(note_id, pin)
    }

    /// The full memory readout for `root` (drives the Memory UI section).
    pub fn memory_snapshot(&self, root: &Path) -> MemorySnapshot {
        let Ok(idx) = self.index_for(root) else {
            return MemorySnapshot {
                current_session: None,
                working_set: Vec::new(),
                notes: Vec::new(),
                quarantined: Vec::new(),
                sessions: Vec::new(),
            };
        };
        let current = idx.mem_current_session().ok().flatten();
        let working_set = current
            .as_deref()
            .map(|s| idx.mem_working_set(s, 50).unwrap_or_default())
            .unwrap_or_default();
        // With a current session, notes = its notes + pinned; without, just
        // pinned. Quarantined notes are NOT among them (`mem_notes` filters them
        // at the storage layer) — they come back separately below, project-wide,
        // because the Memory UI is the only reader allowed to see them.
        let notes = idx
            .mem_notes(current.as_deref().unwrap_or(""))
            .unwrap_or_default();
        let quarantined = idx.mem_quarantined_notes().unwrap_or_default();
        let sessions = idx.mem_sessions().unwrap_or_default();
        MemorySnapshot {
            current_session: current,
            working_set,
            notes,
            quarantined,
            sessions,
        }
    }

    /// V32 Phase C2: release a quarantined note into normal memory (its pinned
    /// state is preserved — see [`GraphIndex::mem_promote_note`]).
    pub fn mem_promote_note(&self, root: &Path, note_id: &str) -> AppResult<()> {
        self.index_for(root)?.mem_promote_note(note_id)
    }

    /// V32 Phase C2: permanently discard a quarantined note.
    pub fn mem_delete_note(&self, root: &Path, note_id: &str) -> AppResult<()> {
        self.index_for(root)?.mem_delete_note(note_id)
    }

    /// Clear one session's memory (`Some`) or the whole project's (`None`).
    pub fn mem_clear(&self, root: &Path, session_id: Option<&str>) -> AppResult<()> {
        // Clear the persisted memory FIRST — it's the fallible part. Only on
        // success drop the in-memory greeting/dedup/reminder bookkeeping, so a
        // failed clear doesn't leave the two out of sync (in-memory wiped while
        // the DB session is intact).
        self.index_for(root)?.mem_clear(session_id)?;
        // V11 Phase B/C/E: drop the in-memory greeting + injection-dedup +
        // read-advisor state for the cleared scope so it re-greets/re-injects.
        if let Ok(mut map) = self.injected.lock() {
            match session_id {
                Some(s) => {
                    map.remove(s);
                }
                None => map.clear(),
            }
        }
        if let Ok(mut set) = self.greeted.lock() {
            match session_id {
                Some(s) => {
                    set.remove(s);
                }
                None => set.clear(),
            }
        }
        if let Ok(mut set) = self.reminded.lock() {
            match session_id {
                Some(s) => {
                    set.remove(s);
                }
                None => set.clear(),
            }
        }
        if let Ok(mut seen) = self.read_seen.lock() {
            seen.clear_session(session_id);
        }
        // V12 review: `auto_check_sessions` (debounce/baseline/pending state
        // for `/context/post_edit`) grows per session and was never evicted —
        // clear the same scope here too, mirroring `injected`/`greeted`/`reminded`.
        if let Ok(mut sessions) = self.auto_check_sessions.lock() {
            match session_id {
                Some(s) => {
                    sessions.remove(s);
                }
                None => sessions.clear(),
            }
        }
        // F14: also drop the post-compaction flag. Left set, `should_read`
        // short-circuits to "pass every read" for the cleared session until the
        // next `retrieve_context` happens to clear it — silently disabling the
        // read advisor for that session in the interim.
        if let Ok(mut set) = self.post_compaction.lock() {
            match session_id {
                Some(s) => {
                    set.remove(s);
                }
                None => set.clear(),
            }
        }
        Ok(())
    }

    // ── V12 Phase E: project facts (memory distillation) ─────────────────

    /// Live (non-archived) project facts for `root`'s Facts UI list — pinned
    /// first, then newest. `root` defaults handled by the caller (IPC).
    pub fn list_project_facts(
        &self,
        root: &Path,
        include_archived: bool,
        max: usize,
    ) -> Vec<ProjectFact> {
        let Ok(idx) = self.index_for(root) else {
            return Vec::new();
        };
        idx.list_project_facts(include_archived, max)
            .unwrap_or_default()
    }

    /// Pin/unpin a fact.
    pub fn set_fact_pinned(&self, root: &Path, fact_id: &str, pinned: bool) -> AppResult<()> {
        self.index_for(root)?.set_fact_pinned(fact_id, pinned)
    }

    /// Archive a fact (excludes it from the cap, `context_recall`, and
    /// promotion, but keeps the row).
    pub fn set_fact_archived(&self, root: &Path, fact_id: &str, archived: bool) -> AppResult<()> {
        self.index_for(root)?.set_fact_archived(fact_id, archived)
    }

    /// Permanently delete a fact.
    pub fn delete_fact(&self, root: &Path, fact_id: &str) -> AppResult<()> {
        self.index_for(root)?.delete_fact(fact_id)
    }

    /// Manually add a fact from the Facts UI's "add fact" input. Recorded
    /// with `source_session = "manual"` so the UI can distinguish it from a
    /// distiller-produced fact (whose source session is a real session id).
    pub fn add_project_fact_manual(&self, root: &Path, text: &str, pin: bool) -> AppResult<()> {
        let idx = self.index_for(root)?;
        let fact_id = uuid::Uuid::new_v4().to_string();
        let ts = crate::activity::now_ms() as i64;
        idx.add_project_fact(&fact_id, text, "manual", ts, pin)
    }

    /// Idle-sweep candidates older than this go through the distiller.
    const DISTILL_IDLE_MS: i64 = 24 * 60 * 60 * 1000;

    /// V12 Phase E: opportunistically distill any session idle more than 24h
    /// that hasn't been distilled yet. There's no dedicated periodic timer in
    /// this service, so this piggybacks on the two places the store is
    /// touched by normal activity anyway — a completed full rebuild and an
    /// applied watcher batch (see call sites) — which keeps distillation
    /// roughly in step with the project without a separate clock. Cheap when
    /// there's nothing to do: one small query, no spawn.
    pub fn spawn_distillation_sweep(self: &Arc<Self>, root: PathBuf) {
        if !self.settings.current().graph.memory_distillation {
            return;
        }
        let Ok(idx) = self.index_for(&root) else {
            return;
        };
        let cutoff = crate::activity::now_ms() as i64 - Self::DISTILL_IDLE_MS;
        let Ok(candidates) = idx.sessions_idle_undistilled(cutoff) else {
            return;
        };
        if candidates.is_empty() {
            return;
        }
        let this = self.clone();
        tauri::async_runtime::spawn(async move {
            for sid in candidates {
                this.distill_session(&root, &sid).await;
            }
        });
    }

    /// V12 Phase E: distill one session's working set + notes into at most 3
    /// durable project facts via the **local-only** offload path
    /// (`OffloadSupervisor::run_internal`, V11 Phase F — never remote/cloud),
    /// then mark the session distilled either way. Gated on
    /// `memory_distillation` + a ready local backend — no ready backend
    /// leaves the session undistilled (V10's "evict undistilled" fallback
    /// still applies; the sweep will retry it next pass). A validation
    /// failure on the model's output DOES mark the session distilled — the
    /// "never retry-loop a bad generation" posture from the milestone spec.
    async fn distill_session(&self, root: &Path, session_id: &str) {
        if !self.settings.current().graph.memory_distillation {
            return;
        }
        let Some(sup) = self
            .app
            .try_state::<Arc<crate::offload::OffloadSupervisor>>()
        else {
            return;
        };
        let sup = sup.inner().clone();

        // Single-flight guard: `is_session_distilled` (below) and
        // `mark_session_distilled` (at the end) are check-then-act across a
        // ~30s `.await`, and two sweeps (the rebuild sweep and a watcher-batch
        // sweep) can both select the same idle-undistilled session in that
        // window. Claim the session id here; a second concurrent call for the
        // same id bails out immediately. `_guard` releases it on every exit
        // path below, including the early returns.
        {
            let mut inflight = self.distilling.lock().unwrap();
            if !inflight.insert(session_id.to_string()) {
                return;
            }
        }
        let _guard = DistillGuard {
            svc: self,
            session_id: session_id.to_string(),
        };

        let Ok(idx) = self.index_for(root) else {
            return;
        };
        // Idempotency guard: `spawn_distillation_sweep`'s candidate query
        // already filters to undistilled sessions, but this also protects a
        // future direct caller (e.g. an eviction-time trigger) from
        // re-distilling a session two triggers raced onto.
        if idx.is_session_distilled(session_id).unwrap_or(false) {
            return;
        }

        let working_set = idx.mem_working_set(session_id, 10).unwrap_or_default();
        let notes = idx.mem_notes(session_id).unwrap_or_default();
        let ts = crate::activity::now_ms() as i64;
        if working_set.is_empty() && notes.is_empty() {
            // Nothing to distill — mark it done so the sweep doesn't keep
            // re-selecting an empty session on every future pass.
            let _ = idx.mark_session_distilled(session_id, ts);
            return;
        }

        let mut body = String::new();
        for e in &working_set {
            body.push_str(&format!(
                "- {} ({}x, last {})\n",
                e.path, e.touches, e.last_kind
            ));
        }
        for n in &notes {
            body.push_str(&format!("- note: {}\n", n.text));
        }
        let prompt = format!("{DISTILL_PROMPT_INSTRUCTION}\n\n{body}");

        let text = match sup
            .run_internal(prompt, 256, std::time::Duration::from_secs(30))
            .await
        {
            Ok(t) => t,
            // No ready local backend (or the call otherwise failed) — leave
            // undistilled, per V10 semantics; retried by a later sweep.
            Err(_) => return,
        };
        match super::memory::parse_distilled_facts(&text) {
            Some(facts) => {
                for f in facts {
                    let fact_id = uuid::Uuid::new_v4().to_string();
                    if let Err(e) = idx.add_project_fact(&fact_id, &f, session_id, ts, false) {
                        debug!(error = %e, "graph: add_project_fact failed");
                    }
                }
            }
            None => {
                debug!(
                    session_id,
                    "graph: distiller output failed validation, skipping insert"
                );
            }
        }
        let _ = idx.mark_session_distilled(session_id, ts);
    }

    // ── V10 context injection ────────────────────────────────────────────

    /// Rank files for `prompt` and build the injectable digest for `root`.
    /// Requires the graph to be enabled but **not** the injection toggle — the
    /// toggle is enforced by the caller (the `/context/retrieve` route), so the
    /// preview surface can show what *would* be injected while it's off. Returns
    /// an empty result when the graph is off or nothing clears the threshold.
    pub fn retrieve_context(
        &self,
        root: &Path,
        prompt: &str,
        session_id: Option<&str>,
    ) -> super::context::RetrieveResult {
        let g = self.settings.current().graph;
        if !g.enabled {
            return super::context::RetrieveResult::default();
        }
        let Ok(idx) = self.index_for(root) else {
            return super::context::RetrieveResult::default();
        };
        let session_files: Vec<(String, f64)> = if g.context_include_session {
            self.mem_working_set(root, session_id, 30)
                .into_iter()
                .map(|e| (e.path, super::context::session_weight(&e.last_kind)))
                .collect()
        } else {
            Vec::new()
        };

        // V11 Phase E: a new prompt ends the post-compaction recovery window —
        // the agent has re-read what it needed, so the advisor resumes next turn.
        if let Some(sid) = session_id.filter(|s| !s.is_empty()) {
            if let Ok(mut s) = self.post_compaction.lock() {
                s.remove(sid);
            }
        }

        // V11 Phase C: injection dedup, only when scoped to a session (the
        // preview passes `None` and so never dedups or mutates injected state).
        let ttl = g.context_dedup_ttl_turns;
        let sid = session_id.filter(|s| !s.is_empty());
        let (snapshot, current_turn) = match sid {
            Some(s) => {
                let mut map = self.injected.lock().unwrap();
                // Bound the per-session dedup state: nothing prunes it when a
                // session simply ends, so cap it (clearing is safe — it just
                // re-injects once). The greeted/reminded/read_seen sets are
                // likewise capped at their own insert sites (F13).
                if map.len() > 1024 && !map.contains_key(s) {
                    map.clear();
                }
                let st = map.entry(s.to_string()).or_default();
                st.turn = st.turn.saturating_add(1);
                (st.files.clone(), st.turn)
            }
            None => (HashMap::new(), 0),
        };

        let result = super::context::build_context(
            &idx,
            prompt,
            &session_files,
            g.context_per_file_chars as usize,
            g.context_turn_budget_chars as usize,
            g.context_min_score as f64,
            &snapshot,
            current_turn,
            ttl,
            g.context_llm_digests,
        );

        // V14 Phase D/D2: honest-accounting counters for the Usage section's
        // Effectiveness panel (injected/deduped chars) and the D2 advisor's
        // budget-maxed signal. Tracked unconditionally — independent of the
        // `ttl > 0` gate below, which only governs the files_used tracking
        // used for dedup demotion.
        if let Some(s) = sid {
            let mut map = self.injected.lock().unwrap();
            let st = map.entry(s.to_string()).or_default();
            st.injected_chars += result.chars as u64;
            st.deduped_chars += result.deduped_chars as u64;
            st.turns_seen = st.turns_seen.saturating_add(1);
            // V16 Feature 9: dedup-suppressed chars join the compounding
            // base, and everything displaced SO FAR is re-counted once per
            // retrieve turn — content kept out of context is saved again as
            // a cache read on every later turn.
            st.displaced_chars_total = st
                .displaced_chars_total
                .saturating_add(result.deduped_chars as u64);
            st.compounded_chars = st.compounded_chars.saturating_add(st.displaced_chars_total);
            let budget = g.context_turn_budget_chars as u64;
            // "Maxed" = this turn's injected chars reached ≥90% of the
            // budget — a proxy for "the budget is the binding constraint",
            // not "we injected literally everything that ranked in".
            if budget > 0 && (result.chars as u64) * 10 >= budget * 9 {
                st.turns_maxed = st.turns_maxed.saturating_add(1);
            }
        }

        // Record the files injected in full so the next turn can dedup them.
        if let Some(s) = sid {
            if ttl > 0 {
                // Resolve each file's current hash BEFORE taking the lock, so
                // the injection map isn't held across per-file SQLite lookups
                // on this hot path.
                let hashes: Vec<(String, String)> = result
                    .files_used
                    .iter()
                    .filter_map(|f| match idx.stored_file_hash(f) {
                        Ok(Some(h)) => Some((f.clone(), h)),
                        _ => None,
                    })
                    .collect();
                let mut map = self.injected.lock().unwrap();
                let st = map.entry(s.to_string()).or_default();
                for (f, h) in hashes {
                    // Only overwrite if this turn is at least as new as the
                    // recorded one, so an interleaved earlier retrieve can't
                    // clobber a newer turn's record with a stale hash.
                    match st.files.get(&f) {
                        Some((_, prev_turn)) if *prev_turn > current_turn => {}
                        _ => {
                            st.files.insert(f, (h, current_turn));
                        }
                    }
                }
            }
        }

        // V11 Phase F: schedule background local-model digests for no-outline
        // files that ranked in but had no cached digest (docs/configs/scripts).
        if g.context_llm_digests {
            for (file, hash) in &result.digest_misses {
                self.enqueue_digest(root, file, hash);
            }
        }
        result
    }

    /// V11 Phase F — kick off a background local-model digest for `(file,
    /// content_hash)` unless one is already in flight, or the in-flight fleet is
    /// at its cap (bounded, demand-driven). The heavy work is slot-gated by the
    /// offload supervisor, so this never floods the local backend. Injection
    /// never waits on it — a miss uses the V10 fallback and the digest lands next
    /// time. Requires the app to be running on the tokio runtime (it always is
    /// when this is reached — via the loopback/IPC async handlers).
    fn enqueue_digest(&self, root: &Path, file: &str, content_hash: &str) {
        const MAX_INFLIGHT: usize = 32;
        let key = (
            root.to_path_buf(),
            file.to_string(),
            content_hash.to_string(),
        );
        {
            let mut inflight = match self.digest_inflight.lock() {
                Ok(g) => g,
                Err(_) => return,
            };
            if inflight.len() >= MAX_INFLIGHT || !inflight.insert(key.clone()) {
                return;
            }
        }
        let Some(me) = self.app.try_state::<Arc<GraphService>>() else {
            let _ = self.digest_inflight.lock().map(|mut g| g.remove(&key));
            return;
        };
        let me = me.inner().clone();
        tokio::spawn(async move {
            // Guard removes the key on Drop — even if the compute panics.
            let _guard = InflightGuard {
                svc: me.clone(),
                key: key.clone(),
            };
            me.compute_and_cache_digest(&key.0, &key.1, &key.2).await;
        });
    }

    /// Compute one digest via the **local-only** offload path and cache it.
    /// Silent on any failure (no local backend, read error, bad output) — the
    /// fallback digest keeps working and the item is simply not cached.
    async fn compute_and_cache_digest(&self, root: &Path, file: &str, content_hash: &str) {
        let Some(sup) = self
            .app
            .try_state::<Arc<crate::offload::OffloadSupervisor>>()
        else {
            return;
        };
        let sup = sup.inner().clone();
        // Read off the async worker — file I/O would otherwise block a tokio
        // thread for the duration.
        let path = root.join(file);
        let content = match tokio::task::spawn_blocking(move || std::fs::read_to_string(path)).await
        {
            Ok(Ok(c)) => c,
            _ => return,
        };
        // Only the first 4 KiB drives the digest — enough for a doc/config head.
        let head: String = content.chars().take(4096).collect();
        let prompt = format!(
            "Summarize this file for a code-assistant context block in at most 3 short lines. \
             No preamble, no code fences.\n\n{head}"
        );
        let text = match sup
            .run_internal(prompt, 128, std::time::Duration::from_secs(20))
            .await
        {
            Ok(t) => t.trim().to_string(),
            Err(_) => return,
        };
        // Validate: non-empty and not oversized (a runaway generation).
        if text.is_empty() || text.chars().count() > 400 {
            return;
        }
        if let Ok(idx) = self.index_for(root) {
            // The SQLite write is synchronous — run it on a blocking thread so
            // it doesn't stall an async worker.
            let file = file.to_string();
            let content_hash = content_hash.to_string();
            let ts = crate::activity::now_ms() as i64;
            let _ = tokio::task::spawn_blocking(move || {
                idx.put_digest(&file, &content_hash, &text, ts)
            })
            .await;
        }
    }

    /// V11 Phase B — the once-per-session project-map greeting for `root`, or
    /// `None` when it's disabled, already delivered this session, or empty.
    /// Called **only** from the real injection path (the `/context/retrieve`
    /// route), never the preview, so the preview can't consume the once-flag. A
    /// session is marked greeted only after a non-empty map renders, so an early
    /// prompt (before the graph has call edges) simply retries next turn.
    pub fn session_greeting(&self, root: &Path, session_id: Option<&str>) -> Option<String> {
        let g = self.settings.current().graph;
        if !g.enabled || !g.context_injection || !g.repo_map_on_session_start {
            return None;
        }
        let sid = session_id.filter(|s| !s.is_empty())?;
        if self.greeted.lock().ok()?.contains(sid) {
            return None;
        }
        let idx = self.index_for(root).ok()?;
        let session_files: Vec<(String, f64)> = self
            .mem_working_set(root, session_id, 30)
            .into_iter()
            .map(|e| (e.path, super::context::session_weight(&e.last_kind)))
            .collect();
        let map = super::context::repo_map(&idx, g.repo_map_budget_chars as usize, &session_files);
        if map.is_empty() {
            return None;
        }
        // Mark greeted (a rare concurrent double-render is harmless). F13: cap
        // the set — nothing prunes it when a session ends, so it grew one entry
        // per greeted session for the process lifetime.
        {
            let mut set = self.greeted.lock().ok()?;
            if set.len() > 1024 && !set.contains(sid) {
                set.clear();
            }
            set.insert(sid.to_string());
        }
        Some(map)
    }

    /// V11 Phase D — handle a compaction for `root`/`session_id`. **Side effects
    /// run unconditionally** (even when the block is gated off): clear the
    /// session's injection-dedup state so the next turn re-injects fresh, and
    /// mark the session post-compaction so the read advisor (Phase E) stops
    /// suppressing reads until each file is re-read. Returns the working-set +
    /// notes block to carry through the summary, or `None` when the block is
    /// disabled or empty.
    pub fn compaction_context(&self, root: &Path, session_id: Option<&str>) -> Option<String> {
        let g = self.settings.current().graph;
        if let Some(sid) = session_id.filter(|s| !s.is_empty()) {
            if let Ok(mut m) = self.injected.lock() {
                m.remove(sid);
            }
            if let Ok(mut s) = self.post_compaction.lock() {
                s.insert(sid.to_string());
            }
        }
        // The block itself is gated (side effects above already ran).
        if !g.enabled || !g.context_injection || !g.compaction_context {
            return None;
        }
        let idx = self.index_for(root).ok()?;
        // V32 Phase C2, #48 finding M-1: the compaction carry-over is the fourth
        // memory-replay path and had no envelope. Resolved HERE rather than
        // threaded from the caller because the caller is the loopback's
        // `POST /context/compaction` route, whose body carries a `cwd` and a
        // `session_id` and no tab identity at all — so `Scope::App` is not a
        // fallback, it is the only scope that exists on this route. The same
        // choice, for the same reason, as the headless MCP child's.
        let spotlight = crate::settings::injection::effective(
            crate::settings::injection::Feature::Spotlighting,
            crate::settings::injection::Scope::App,
            &self.settings.current(),
        );
        let block = super::context::compaction_block(&idx, session_id, spotlight);
        if block.is_empty() {
            None
        } else {
            Some(block)
        }
    }

    /// V11 Phase E — whether `session_id` is flagged post-compaction (the read
    /// advisor passes reads while this holds). Cleared on the next prompt's
    /// `retrieve_context`, so the recovery window is "until the next user turn".
    pub fn is_post_compaction(&self, session_id: &str) -> bool {
        self.post_compaction
            .lock()
            .map(|s| s.contains(session_id))
            .unwrap_or(false)
    }

    /// V11 Phase E / V17 Phase A — the read advisor's verdict for a `Read` of
    /// `file_path`: `Some(reminder_text)` to deny-with-content, or `None` to let
    /// the read proceed. Passes when: the advisor is off; there's no session; the
    /// session is recovering from a compaction; the file was never seen this
    /// session (record-and-pass); it was seen UNCHANGED and either already
    /// reminded, under the min-lines floor, or past its trust TTL; or it CHANGED
    /// but no diff is available (feature off / snapshot evicted / near-rewrite /
    /// re-arm cap reached). It REMINDS when: seen unchanged, not yet reminded,
    /// ≥ min-lines (outline, plus the body in substitute mode); or seen CHANGED
    /// with a surviving snapshot and a small-enough diff (a unified diff against
    /// exactly what the agent last read). A changed file re-arms an already-fired
    /// reminder up to [`READ_REMIND_CAP`] times.
    ///
    /// V17 Phase C adds the first-read tier: when `read_advisor_first_read_kb > 0`
    /// a NEVER-seen whole-file read of a large **non-code** file (empty outline)
    /// with a cached digest is answered with a digest + head/tail sample instead
    /// of the full content (a digest miss enqueues one and passes).
    pub fn should_read(
        &self,
        root: &Path,
        session_id: Option<&str>,
        file_path: &str,
        offset: Option<u32>,
        limit: Option<u32>,
    ) -> Option<String> {
        let g = self.settings.current().graph;
        if !g.enabled || !g.read_advisor {
            return None;
        }
        // V17 Phase B threads `limit` through; V17 Phase C's first-read branch
        // (in the never-seen Pass arm below) consumes it — a deliberate slice
        // (`offset` OR `limit` present) always passes. Existing offset-only
        // behavior is unchanged.
        let sid = session_id.filter(|s| !s.is_empty())?;
        // Recovering from a compaction ⇒ pass everything (content was lost).
        if self.is_post_compaction(sid) {
            return None;
        }
        let rel = relativize_path(root, file_path);
        if rel.is_empty() {
            return None;
        }
        let key = (sid.to_string(), rel.clone());
        let idx = self.index_for(root).ok()?;

        // The session's turn counter — the V16 trust-TTL clock (and the
        // bypass window's turn stamp). Ticked by `retrieve_context` when
        // injection is on, by the transcript tap's [`Self::note_user_turn`]
        // otherwise.
        let cur_turn = self.session_turn(sid);

        // V17 Phase A: the `reminded` short-circuit moved BELOW the fs read +
        // hash compare — the re-arm rule needs the current hash to tell an
        // unchanged re-ask (always passes) from a CHANGED re-read of an
        // already-reminded file (may re-arm, capped). The fs read was already
        // unconditional on the remind path, so this only adds a read to the
        // already-reminded case.
        let abs = root.join(&rel);
        let content = std::fs::read_to_string(&abs).ok()?;
        let cur_hash = super::model::fnv1a_hex(&content);

        // What (if anything) we last observed the agent read for this file, plus
        // the snapshot of that content if one survived the LRU. Compare against
        // THIS, not the index hash: once the watcher re-indexes an edited file
        // the index hash equals the NEW content, yet the agent's context still
        // holds the pre-edit version — an index-hash match would wrongly suppress
        // the re-read it genuinely needs. Content hash rather than mtime, so a
        // filesystem clock skew (network shares, WSL2 bind-mounts) can't mislead.
        let prev = {
            let seen = self.read_seen.lock().ok()?;
            seen.map
                .get(&key)
                .map(|v| (v.hash.clone(), v.turn, v.snapshot.clone()))
        };
        let (seen, unchanged, prev_turn, prev_snapshot) = match &prev {
            Some((h, t, snap)) => (true, *h == cur_hash, *t, snap.clone()),
            None => (false, false, 0, None),
        };
        let ttl = g.read_advisor_ttl_turns;
        let ttl_expired = unchanged && ttl > 0 && cur_turn.saturating_sub(prev_turn) > ttl;

        // Already reminded this session? (Count drives the re-arm cap.)
        let (reminded_before, prev_count) = {
            let set = self.reminded.lock().ok()?;
            match set.get(sid).and_then(|m| m.get(&rel)) {
                Some(mark) => (true, mark.count),
                None => (false, 0),
            }
        };
        let big_enough = (content.lines().count() as u32) >= g.read_advisor_min_lines;
        let diffs_on = g.read_advisor_diffs;

        // Render the diff only when a changed re-read could actually use it
        // (feature on, snapshot survived, not already at the re-arm cap). The
        // "worth it" gate: a rendered diff over half the new content is a
        // near-rewrite — not worth a denial. `read_to_string` above already
        // guarantees UTF-8, so binary files never reach here (they fail the read).
        let diff_eligible = !unchanged
            && diffs_on
            && prev_snapshot.is_some()
            && !(reminded_before && prev_count >= READ_REMIND_CAP);
        let mut rendered_diff: Option<String> = None;
        let mut diff_worth_it = false;
        if diff_eligible {
            if let Some(old) = prev_snapshot.as_deref() {
                let d = super::context::unified_diff(old, &content, &rel);
                // ≤ 50% of the new content's length (chars).
                if d.chars().count().saturating_mul(2) <= content.chars().count() {
                    diff_worth_it = true;
                    rendered_diff = Some(d);
                }
            }
        }

        let vin = VerdictIn {
            seen,
            unchanged,
            ttl_expired,
            reminded: reminded_before,
            remind_count: prev_count,
            big_enough,
            diffs_on,
            have_snapshot: prev_snapshot.is_some(),
            diff_worth_it,
        };

        match read_verdict(&vin) {
            ReadAdvice::Pass { restamp } => {
                // ─── Phase C (C1): first-read tier for huge non-code files ──
                // The NEVER-SEEN case lands here (`seen == false`), evaluated
                // BEFORE the record-and-pass below. When the tier is on and the
                // file is large + non-code + a deliberate slice isn't in play,
                // and a digest is cached for the current hash, substitute a
                // `first_read_advice` reminder; on a digest MISS, enqueue one and
                // fall through to a plain pass (never-block — protection begins on
                // the next, cross-session encounter, since digests are
                // content-hash keyed and survive sessions).
                // Cheap gates first (setting, size, slice) so the `outline` DB
                // query only runs when the tier is on AND the file qualifies —
                // never on the common tiny-first-read path or when the tier is off.
                let slice = offset.is_some() || limit.is_some();
                let big = g.read_advisor_first_read_kb > 0
                    && content.len()
                        >= (g.read_advisor_first_read_kb as usize).saturating_mul(1024);
                if !seen && big && !slice {
                    let outline_empty = idx.outline(&rel).map(|o| o.is_empty()).unwrap_or(false);
                    let fin = FirstReadIn {
                        first_read_kb: g.read_advisor_first_read_kb,
                        content_len: content.len(),
                        slice,
                        is_code: !outline_empty,
                    };
                    if first_read_eligible(&fin) {
                        match idx.get_digest(&rel, &cur_hash) {
                            Ok(Some(digest)) => {
                                let text =
                                    super::context::first_read_advice(&rel, &content, &digest);
                                let displaced = content.chars().count() as u64;
                                let request = format!(
                                    "agent read of `{rel}` (huge non-code — digest substituted, first-read)"
                                );
                                let out = self.record_remind(
                                    root, &idx, sid, &rel, text, request, displaced, 1, cur_turn,
                                );
                                // C3: the file enters `reminded` (via record_remind)
                                // but read_seen keeps NO snapshot — generated-file
                                // diffs are useless and would blow the LRU. A later
                                // CHANGED re-read has no snapshot, so it just passes.
                                let touch = self.read_seen_touch.fetch_add(1, Ordering::Relaxed);
                                if let Ok(mut seen_map) = self.read_seen.lock() {
                                    seen_map.insert(key, cur_hash, cur_turn, None, touch);
                                }
                                return out;
                            }
                            _ => {
                                // Miss (or read error) on an otherwise-qualifying
                                // file ⇒ enqueue a digest and fall through to pass.
                                self.enqueue_digest(root, &rel, &cur_hash);
                            }
                        }
                    }
                }
                if restamp {
                    // Capture a fresh snapshot of the current content (never-seen,
                    // changed-pass, and TTL re-stamp all record the new observation).
                    let snap = capture_snapshot(&content, g.read_advisor_min_lines);
                    let touch = self.read_seen_touch.fetch_add(1, Ordering::Relaxed);
                    if let Ok(mut seen) = self.read_seen.lock() {
                        seen.insert(key, cur_hash, cur_turn, snap, touch);
                    }
                }
                None
            }
            ReadAdvice::Outline => {
                // Unchanged, first remind: outline (+ body in substitute mode).
                let substitute = g.read_advisor_mode.eq_ignore_ascii_case("substitute");
                let text = super::context::read_advice(
                    &idx,
                    root,
                    &rel,
                    offset,
                    substitute,
                    g.max_body_bytes as usize,
                );
                let displaced = content.chars().count() as u64;
                let request =
                    format!("agent re-read of `{rel}` (the trigger — no explicit request)");
                // read_seen stays as-is (content is unchanged).
                self.record_remind(
                    root,
                    &idx,
                    sid,
                    &rel,
                    text,
                    request,
                    displaced,
                    prev_count.saturating_add(1),
                    cur_turn,
                )
            }
            ReadAdvice::Diff => {
                // Changed: answer with a diff against the last-read snapshot.
                let diff_body = rendered_diff.unwrap_or_default();
                let text = super::context::diff_advice(&rel, prev_turn, &diff_body);
                let displaced = content.chars().count() as u64;
                let request = format!("agent re-read of `{rel}` (changed — diff substituted)");
                let out = self.record_remind(
                    root,
                    &idx,
                    sid,
                    &rel,
                    text,
                    request,
                    displaced,
                    prev_count.saturating_add(1),
                    cur_turn,
                );
                // After a diff remind the agent holds the CURRENT content: update
                // read_seen to (new hash, cur turn, new snapshot) so a further
                // change diffs against what it now knows. The bypass window keys
                // off RemindMark.turn/ts_ms, which `record_remind` just re-stamped.
                let snap = capture_snapshot(&content, g.read_advisor_min_lines);
                let touch = self.read_seen_touch.fetch_add(1, Ordering::Relaxed);
                if let Ok(mut seen) = self.read_seen.lock() {
                    seen.insert(key, cur_hash, cur_turn, snap, touch);
                }
                out
            }
        }
    }

    /// V17 Phase A: shared read-advisor remind bookkeeping for the outline and
    /// diff branches — inserts the `RemindMark` (re-arm `count` included), adds
    /// the displaced content to the session's compounding base, persists the
    /// `mem_event{kind:"remind"}` row, and records the Activity event. Returns
    /// `Some(text)` (the reminder) so the caller can hand it straight back.
    #[allow(clippy::too_many_arguments)]
    fn record_remind(
        &self,
        root: &Path,
        idx: &GraphIndex,
        sid: &str,
        rel: &str,
        text: String,
        request: String,
        displaced: u64,
        new_count: u32,
        cur_turn: u32,
    ) -> Option<String> {
        let advice_chars = text.chars().count() as u64;
        // F13: cap the map — one entry per (session, file), never pruned on
        // session end (clearing is safe: a dropped key just allows one re-remind).
        {
            let mut set = self.reminded.lock().ok()?;
            let total: usize = set.values().map(HashMap::len).sum();
            if total > 4096 && !set.get(sid).is_some_and(|m| m.contains_key(rel)) {
                set.clear();
            }
            set.entry(sid.to_string()).or_default().insert(
                rel.to_string(),
                RemindMark {
                    turn: cur_turn,
                    ts_ms: crate::activity::now_ms(),
                    chars: displaced,
                    advice_chars,
                    bypassed: false,
                    count: new_count,
                },
            );
        }
        // V16 Feature 9: the displaced file content joins this session's
        // compounding base — every later retrieve turn re-counts it as a cache
        // read avoided. (Session-scoped, unlike the process-wide Activity sum the
        // panel also shows; the two coexist — Activity stays the audit trail.)
        if let Ok(mut map) = self.injected.lock() {
            let st = map.entry(sid.to_string()).or_default();
            st.displaced_chars_total = st.displaced_chars_total.saturating_add(displaced);
        }
        let ts = crate::activity::now_ms() as i64;
        // V14 Phase D2: also persist a root+session-scoped `mem_event{kind:
        // "remind"}` row — distinct from the process-wide Activity event below —
        // so `GraphIndex::advisor_reread_rate` can precisely check whether the
        // agent re-read this exact file afterward. Reaching a remind means the
        // agent has already read this file at least once this session
        // (`read_seen` held its prior hash), so the session row normally exists;
        // `"claude"` is a safe default if the lookup somehow misses.
        let agent = idx
            .session_agent(sid)
            .ok()
            .flatten()
            .unwrap_or_else(|| "claude".to_string());
        let _ = idx.record_mem_event(sid, &agent, "remind", rel, None, None, ts, None);
        // Activity: `chars` is the reminder's actual size (what we returned),
        // consistent with every other graph tool's honest response-size figure —
        // not a fabricated token estimate.
        crate::activity::record_bg(crate::activity::ActivityRecord {
            request,
            response: text.clone(),
            entry: crate::activity::ActivityEntry::new(
                crate::activity::ActivityKind::Graph,
                ts as u64,
                crate::activity::root_key(root),
                "read_advisor".to_string(),
                "remind".to_string(),
                rel.to_string(),
                text.chars().count(),
                0,
                true,
            ),
        });
        Some(text)
    }

    /// V16 Feature 4: test a Bash command's path-like tokens against this
    /// session's recent read-advisor reminders; record a `bypass` Activity
    /// event (and un-count the displaced chars) for each hit. Called from
    /// the OOB transcript tap on every Claude Bash `tool_use` — detection is
    /// free there; no new hook (a `PostToolUse` shim spawn per shell command
    /// was considered and rejected, see the milestone doc).
    ///
    /// Matching is deliberately heuristic (labeled `est.` everywhere it's
    /// counted): a token matches a reminded file when it equals the file's
    /// relative path, is a path ending in it, or shares its basename. The
    /// window is ≤3 retrieve turns after the remind, with a 5-minute
    /// wall-clock fallback for sessions where injection is off and the turn
    /// clock never ticks. One bypass per reminder (`RemindMark::bypassed`).
    pub fn check_bypass(&self, root: &Path, session_id: &str, command: &str) {
        const BYPASS_TURNS: u32 = 3;
        const BYPASS_MS: u64 = 5 * 60 * 1000;
        if session_id.is_empty() {
            return;
        }
        let g = self.settings.current().graph;
        if !g.enabled || !g.read_advisor {
            return;
        }
        // V17 Phase B5: a provable whole-file shell read (`cat foo`) is handled
        // by the read advisor's Bash hook — it was either intercepted-and-denied
        // (the remind is already recorded by `should_read`) or verdict-passed
        // (not a bypass). Skip it BEFORE scoring: otherwise the denied `cat`
        // still shows up as a `tool_use` in the transcript and this tap would
        // double-count it as a bypass, poisoning `drift.read_bypass.v1`. With
        // the guard, the canary measures only RESIDUAL escape routes (`sed -n`,
        // `head`, redirections — the strict parser rejects those).
        if intercepted_whole_file_read(g.read_advisor_shell, command) {
            return;
        }
        let tokens = path_like_tokens(command);
        if tokens.is_empty() {
            return;
        }
        let cur_turn = self.session_turn(session_id);
        let now = crate::activity::now_ms();

        // Collect hits under the lock, record outside it. Session-keyed map:
        // only this session's own reminders are scanned.
        let mut hits: Vec<(String, u64, u64)> = Vec::new();
        if let Ok(mut set) = self.reminded.lock() {
            if let Some(marks) = set.get_mut(session_id) {
                for (rel, mark) in marks.iter_mut() {
                    if mark.bypassed {
                        continue;
                    }
                    // "Within 3 retrieve turns of the remind" when the turn
                    // clock is ticking; the 5-minute wall-clock window when it
                    // isn't (injection off ⇒ the counter never advances, and a
                    // 0-0 turn delta would otherwise match forever).
                    let in_window = if cur_turn > mark.turn {
                        cur_turn - mark.turn <= BYPASS_TURNS
                    } else {
                        now.saturating_sub(mark.ts_ms) <= BYPASS_MS
                    };
                    if !in_window {
                        continue;
                    }
                    if tokens.iter().any(|t| token_matches_path(t, rel)) {
                        mark.bypassed = true;
                        hits.push((rel.clone(), mark.chars, mark.advice_chars));
                    }
                }
            }
        }
        for (rel, chars, advice_chars) in hits {
            // Un-count from the session's compounding base — a bypassed
            // remind displaced nothing, so it stops compounding from this
            // turn forward (already-compounded turns stay counted; the
            // readout is measured, not retroactive).
            if let Ok(mut map) = self.injected.lock() {
                if let Some(st) = map.get_mut(session_id) {
                    st.displaced_chars_total = st.displaced_chars_total.saturating_sub(chars);
                }
            }
            // The panel's displaced figure sums reminder TEXT — net this
            // bypass out of it in the same unit (`effectiveness_totals`),
            // not in whole-file chars (which would let one big-file bypass
            // zero the entire metric).
            self.bypassed_advice_chars
                .fetch_add(advice_chars, Ordering::Relaxed);
            crate::activity::record_bg(crate::activity::ActivityRecord {
                request: format!("shell read of `{rel}` after a read-advisor reminder (est.)"),
                response: String::new(),
                entry: crate::activity::ActivityEntry::new(
                    crate::activity::ActivityKind::Graph,
                    now,
                    crate::activity::root_key(root),
                    "read_advisor".to_string(),
                    "bypass".to_string(),
                    rel,
                    chars as usize,
                    0,
                    false, // a bypass is a miss for the advisor — flag it
                ),
            });
        }
    }

    /// The session's turn counter — 0 when nothing ever ticked it. Ticked by
    /// `retrieve_context` (one per retrieve) when context injection is on,
    /// and by [`Self::note_user_turn`] (one per genuine user prompt from the
    /// transcript tap) when it's off.
    fn session_turn(&self, session_id: &str) -> u32 {
        self.injected
            .lock()
            .ok()
            .and_then(|m| m.get(session_id).map(|st| st.turn))
            .unwrap_or(0)
    }

    /// V16 review fix: advance the session's turn clock on a genuine user
    /// prompt when CONTEXT INJECTION IS OFF. `retrieve_context` is the clock
    /// when injection is on (one tick per retrieve); with injection off it
    /// never runs, `InjectState.turn` stays 0 forever, and (a) the read
    /// advisor's trust TTL (`read_advisor_ttl_turns`) could never expire —
    /// the one decision it exists to govern — and (b) the Feature-9
    /// compounding readout never accrues. Gated exactly opposite to the
    /// injection tick so the two clocks can't double-count a turn; gated on
    /// the read advisor because nothing else consumes the clock offline.
    pub fn note_user_turn(&self, session_id: &str) {
        if session_id.is_empty() {
            return;
        }
        let g = self.settings.current().graph;
        if !g.enabled || !g.read_advisor || g.context_injection {
            return;
        }
        if let Ok(mut map) = self.injected.lock() {
            // Same bound as `retrieve_context`'s insert site.
            if map.len() > 1024 && !map.contains_key(session_id) {
                map.clear();
            }
            let st = map.entry(session_id.to_string()).or_default();
            st.turn = st.turn.saturating_add(1);
            // Feature 9: displaced content is saved again on every later
            // turn — the same per-turn re-count `retrieve_context` does when
            // injection is on.
            st.compounded_chars = st.compounded_chars.saturating_add(st.displaced_chars_total);
        }
    }

    /// V16 Feature 2 drift signals that need full-relation scans, cached per
    /// root for [`DRIFT_SIGNALS_TTL`] (the Overview polls every 2s; these
    /// only change when new events land): `large_reread_pairs` — (session,
    /// file) pairs with ≥2 observed reads of a file at/above
    /// `read_advisor_min_lines`, the condition `should_read` reminds on
    /// (zero reminds + many large re-reads ⇒ the hook isn't firing) — plus
    /// `GraphIndex::claude_tokenless_sessions`' two counts.
    pub fn drift_db_signals(&self, root: &Path) -> (u64, u64, u64) {
        const DRIFT_SIGNALS_TTL: Duration = Duration::from_secs(30);
        if let Ok(cache) = self.drift_signals.lock() {
            if let Some(s) = cache.get(root) {
                if s.at.elapsed() < DRIFT_SIGNALS_TTL {
                    return (s.large_reread_pairs, s.claude_sessions, s.claude_tokenless);
                }
            }
        }
        let (pairs, claude, tokenless) = match self.index_for(root) {
            Ok(idx) => {
                let min_lines = self.settings.current().graph.read_advisor_min_lines;
                let pairs = idx.large_reread_pairs(min_lines).unwrap_or(0);
                let (claude, tokenless) = idx.claude_tokenless_sessions().unwrap_or((0, 0));
                (pairs, claude, tokenless)
            }
            Err(_) => (0, 0, 0),
        };
        if let Ok(mut cache) = self.drift_signals.lock() {
            cache.insert(
                root.to_path_buf(),
                DriftDbSignals {
                    at: Instant::now(),
                    large_reread_pairs: pairs,
                    claude_sessions: claude,
                    claude_tokenless: tokenless,
                },
            );
        }
        (pairs, claude, tokenless)
    }

    /// V16 Feature 4 (`drift.read_bypass.v1` signal): share of read-advisor
    /// reminders answered with a shell read, from the process-wide Activity
    /// events since this run started (est. — same posture as the panel's
    /// displaced figure). Sample count = reminders. `None` when the advisor
    /// never reminded this run. Takes the caller's snapshot so one
    /// `graph_usage_advice` call clones the activity ring once, not once per
    /// signal.
    pub fn bypass_rate(&self, activity: &[crate::activity::ActivityEntry]) -> Option<(f64, u64)> {
        let since = crate::activity::process_start_ms();
        let mut reminds = 0u64;
        let mut bypasses = 0u64;
        for e in activity {
            if e.source != "read_advisor" || e.ts_ms < since {
                continue;
            }
            match e.tool.as_str() {
                "remind" => reminds += 1,
                "bypass" => bypasses += 1,
                _ => {}
            }
        }
        if reminds == 0 {
            None
        } else {
            Some(((bypasses as f64 / reminds as f64).min(1.0), reminds))
        }
    }

    /// Whether context injection is currently enabled (graph + toggle). The
    /// injection routes gate on this; the preview path does not.
    pub fn context_injection_enabled(&self) -> bool {
        let g = self.settings.current().graph;
        g.enabled && g.context_injection
    }

    // ── V12 Phase F: proactive automation (`/context/post_edit`) ─────────

    /// The client-facing budget for one `post_edit` call: run(s) that finish
    /// within this window return their result immediately; a slower run keeps
    /// going in the background and its result is PARKED for the next
    /// `post_edit`/`/context/retrieve` call to drain (see
    /// [`Self::drain_auto_check`]). Generous relative to the hook shim's own
    /// ~600 ms socket timeout would allow, but the shim's timeout is the real
    /// backstop — this just avoids obviously wasting the budget on a run that
    /// was never going to make it (most real checkers take seconds).
    const POST_EDIT_BUDGET_MS: u64 = 800;

    /// V12 Phase F (6a/6b) — the `/context/post_edit` route's core. Debounces
    /// this session's edits (`auto_check_debounce_s`), runs the project's
    /// configured checks single-flight per root, diffs the result against the
    /// session's OWN baseline (never another session's — V10 scoping), and
    /// appends an auto-impact blast-radius note when the edited file's
    /// symbols are heavily depended on. Fail-open/non-blocking throughout:
    /// disabled settings, no checks configured, a missing session id, or a
    /// slow run all yield `None` now (a slow run's result is parked instead —
    /// drained by the next call).
    pub async fn post_edit(
        self: &Arc<Self>,
        root: &Path,
        session_id: Option<&str>,
        file_path: &str,
    ) -> Option<String> {
        let settings = self.settings.current();
        if !settings.graph.enabled || !settings.graph.auto_check || settings.checks.is_empty() {
            return None;
        }
        let sid = session_id.filter(|s| !s.is_empty())?.to_string();
        let debounce = Duration::from_secs(settings.graph.auto_check_debounce_s.max(1) as u64);

        let run_now = {
            let mut sessions = self.auto_check_sessions.lock().unwrap();
            // Bound the per-session auto-check state the same way `injected`
            // is bounded: nothing evicts it when a session simply ends
            // (`mem_clear` only covers an explicit clear), so cap it —
            // clearing is safe, it just re-runs the debounce/baseline fresh.
            if sessions.len() > 1024 && !sessions.contains_key(&sid) {
                sessions.clear();
            }
            let st = sessions.entry(sid.clone()).or_default();
            crate::checks::auto::should_run(st, Instant::now(), debounce)
        };
        if !run_now {
            return self.drain_auto_check(Some(&sid));
        }

        let root_buf = root.to_path_buf();
        let defs = settings.checks.clone();
        let this = self.clone();
        let mut handle =
            tokio::spawn(async move { this.auto_check_runner.run(&root_buf, &defs).await });

        tokio::select! {
            res = &mut handle => {
                let (reports, check_errors) = res.unwrap_or_default();
                self.finish_post_edit(&sid, root, file_path, reports, check_errors, false)
            }
            _ = tokio::time::sleep(Duration::from_millis(Self::POST_EDIT_BUDGET_MS)) => {
                // Slow: let the run keep going in the background and park its
                // (diffed) result for the next call to pick up. The turn is
                // never blocked on it.
                let this2 = self.clone();
                let sid2 = sid.clone();
                let root2 = root.to_path_buf();
                let file2 = file_path.to_string();
                tokio::spawn(async move {
                    // Bound the parked run: a check command that hangs (waiting
                    // on a lock, reading stdin) must not live forever, or the
                    // single-flight `RootRunner` coalesces every later post_edit
                    // onto the stuck task and auto-check is silently wedged for
                    // this root. On timeout, abort so the root isn't pinned.
                    const PARKED_MAX_MS: u64 = 60_000;
                    match tokio::time::timeout(
                        Duration::from_millis(PARKED_MAX_MS),
                        &mut handle,
                    )
                    .await
                    {
                        Ok(Ok((reports, check_errors))) => {
                            this2.finish_post_edit(&sid2, &root2, &file2, reports, check_errors, true);
                        }
                        Ok(Err(_join_err)) => {}
                        Err(_elapsed) => handle.abort(),
                    }
                });
                None
            }
        }
    }

    /// Diff `reports` against `sid`'s baseline (updating it to the fresh
    /// result regardless of what's surfaced), append `check_errors` (any
    /// configured check that failed to spawn/run — V12 review: these must
    /// stay visible, never silently vanish and read as "ran clean"), append
    /// the auto-impact note, record one Activity event per non-empty
    /// injection, and either return the block (`park = false`, the fast path)
    /// or stash it in the session's `pending` slot (`park = true`, drained by
    /// the next call).
    fn finish_post_edit(
        self: &Arc<Self>,
        sid: &str,
        root: &Path,
        file_path: &str,
        reports: Vec<crate::checks::CheckReport>,
        check_errors: Vec<String>,
        park: bool,
    ) -> Option<String> {
        let cap_chars = 1500usize;
        let mut per_check: Vec<(String, Vec<crate::checks::DiagGroup>)> = Vec::new();
        {
            let mut sessions = self.auto_check_sessions.lock().unwrap();
            let st = sessions.entry(sid.to_string()).or_default();
            for report in &reports {
                let baseline = st.baseline.entry(report.name.clone()).or_default();
                let new_groups: Vec<crate::checks::DiagGroup> =
                    crate::checks::auto::diff_groups(baseline, &report.groups)
                        .into_iter()
                        .cloned()
                        .collect();
                per_check.push((report.name.clone(), new_groups));
            }
            // Update the baseline to this run's full result regardless of what
            // was surfaced — the session has now effectively "seen" it.
            for report in &reports {
                st.baseline.insert(
                    report.name.clone(),
                    crate::checks::auto::to_baseline(&report.groups),
                );
            }
        }
        let per_check_refs: Vec<(&str, Vec<&crate::checks::DiagGroup>)> = per_check
            .iter()
            .map(|(name, groups)| (name.as_str(), groups.iter().collect()))
            .collect();
        let mut block = crate::checks::auto::format_diff_block(&per_check_refs, cap_chars);

        // V12 review: a check that failed to spawn/run must stay visible —
        // never let it silently vanish and read as "everything's clean" just
        // because there was nothing else to report.
        if !check_errors.is_empty() {
            if !block.is_empty() {
                block.push('\n');
            }
            block.push_str(&check_errors.join("\n"));
        }

        if let Some(note) = self.auto_impact_note(root, file_path) {
            if block.is_empty() {
                block = note;
            } else {
                block.push('\n');
                block.push_str(&note);
            }
        }

        if block.is_empty() {
            return None;
        }

        // Activity: one event per injection — the graduation evidence for
        // milestone Decision 4 (whether auto-check injections correlate with
        // a same-turn fix).
        crate::activity::record_bg(crate::activity::ActivityRecord {
            entry: crate::activity::ActivityEntry::new(
                crate::activity::ActivityKind::Graph,
                crate::activity::now_ms(),
                crate::activity::root_key(root),
                "auto_check".to_string(),
                "auto_check".to_string(),
                file_path.to_string(),
                block.chars().count(),
                0,
                true,
            ),
            request: format!("agent edit of `{file_path}` (the trigger — no explicit request)"),
            response: block.clone(),
        });

        if park {
            let mut sessions = self.auto_check_sessions.lock().unwrap();
            sessions.entry(sid.to_string()).or_default().pending = Some(block);
            None
        } else {
            Some(block)
        }
    }

    /// V12 Phase F (6b) — the auto-impact blast-radius note for an edit to
    /// `file_path`: every symbol the edited file DEFINES (its outline) whose
    /// direct inbound call count (`callers_count`) meets
    /// `auto_impact_min_dependents` gates the note; the note's own numbers are
    /// the wider transitive dependents/files/tests counts. `None` when the
    /// graph is unavailable, the file isn't indexed, or nothing meets the
    /// threshold.
    fn auto_impact_note(&self, root: &Path, file_path: &str) -> Option<String> {
        let g = self.settings.current().graph;
        let idx = self.index_for(root).ok()?;
        let rel = relativize_path(root, file_path);
        if rel.is_empty() {
            return None;
        }
        let outline = idx.outline(&rel).ok()?;
        if outline.is_empty() {
            return None;
        }
        let min_dependents = g.auto_impact_min_dependents as u64;
        let mut max_callers: u64 = 0;
        let mut names: Vec<String> = Vec::new();
        for s in &outline {
            let callers = idx.callers_count(&s.name).unwrap_or(0);
            max_callers = max_callers.max(callers);
            if callers >= min_dependents {
                names.push(s.name.clone());
            }
        }
        if names.is_empty() {
            return None;
        }
        names.sort();
        names.dedup();
        let max = g.max_rows_per_query.max(1) as usize;
        let dependents = idx.dependents_transitive(&names, 3, max, None).ok()?;
        let files: HashSet<&str> = dependents.iter().map(|d| d.symbol.file.as_str()).collect();
        let tests = idx.tests_for(&names, 3, max).unwrap_or_default();
        crate::checks::auto::impact_note(
            max_callers,
            dependents.len(),
            files.len(),
            tests.len(),
            min_dependents,
        )
    }

    /// Drain (take) `session_id`'s parked auto-check block, if any — called
    /// from `post_edit` (a coalesced call) and from `/context/retrieve` so a
    /// slow check's result is never lost, only delayed. `None` for an unknown
    /// session id or an empty session id.
    pub fn drain_auto_check(&self, session_id: Option<&str>) -> Option<String> {
        let sid = session_id.filter(|s| !s.is_empty())?;
        self.auto_check_sessions
            .lock()
            .unwrap()
            .get_mut(sid)
            .and_then(|st| st.pending.take())
    }

    // ── V12 Phase F (6c): analyses-auto trigger ───────────────────────────

    /// Run `dead_exports` + `import_cycles` for `root` (bounded, read-only on
    /// the warm index — cheap) and emit `graph-analyses` when the counts
    /// changed since the last completed pass. Called at the end of every
    /// completed index pass ([`Self::spawn_rebuild`]'s success handler and
    /// [`Self::reindex_paths`]'s "changed > 0" branch), same spot as the
    /// distillation sweep. No-op when `analyses_auto` is off.
    /// V22 Phase D: per-language indexed file counts for `root`, as
    /// [`crate::checks::detect::LangStat`]s (the last successful build's
    /// `langs`, empty before a first build). Decoupled from [`LangCount`] so the
    /// `checks` module needn't depend on the graph crate. Feeds detection's
    /// code-graph evidence source.
    pub fn checks_lang_stats(&self, root: &Path) -> Vec<crate::checks::detect::LangStat> {
        self.status
            .lock()
            .unwrap()
            .get(root)
            .map(|s| {
                s.langs
                    .iter()
                    .map(|l| crate::checks::detect::LangStat {
                        lang: l.lang.clone(),
                        files: l.files,
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    /// V22 Phase D: when `checks_auto_configure` is on and the project has no
    /// checks yet, apply the validated detection proposals once the graph is
    /// built — opt-in zero-touch setup. Called at the end of every completed
    /// index pass ([`Self::spawn_rebuild`]'s success handler and
    /// [`Self::reindex_paths`]'s "changed > 0" branch), inline on the index
    /// worker thread like [`run_analyses_trigger`](Self::run_analyses_trigger);
    /// detection is
    /// bounded filesystem + PATH work (no network). The settings `mutate`
    /// broadcasts and persists (per-project overlay), so the UI reflects the
    /// applied set — the entries carry `CheckDef::auto = true`.
    fn checks_auto_configure_trigger(&self, root: &Path) {
        let snap = self.settings.current();
        if !snap.checks_auto_configure || !snap.checks.is_empty() {
            return;
        }
        let stats = self.checks_lang_stats(root);
        let valid: Vec<crate::checks::CheckDef> = crate::checks::detect::detect(root, &stats)
            .into_iter()
            .filter(|p| p.valid)
            .map(|p| p.check)
            .collect();
        if valid.is_empty() {
            return;
        }
        let mut applied: Vec<String> = Vec::new();
        self.settings.mutate(|s| {
            // Re-check under the lock: a concurrent apply may have populated
            // `checks` (or the toggle flipped) between the snapshot and here.
            if s.checks_auto_configure && s.checks.is_empty() {
                applied = crate::checks::detect::merge_auto(&mut s.checks, valid);
            }
        });
        if !applied.is_empty() {
            info!(root = %root.display(), applied = ?applied, "checks: auto-configured run_check from detection");
        }
    }

    fn run_analyses_trigger(&self, root: &Path) {
        if !self.settings.current().graph.analyses_auto {
            return;
        }
        let Ok(idx) = self.index_for(root) else {
            return;
        };
        let max = self.settings.current().graph.max_rows_per_query.max(1) as usize;
        let dead = idx.dead_exports(max).map(|v| v.len()).unwrap_or(0);
        let cycles = idx.import_cycles(max).map(|v| v.len()).unwrap_or(0);
        const KEY: &str = "analyses_counts";
        let prev = idx.get_meta(KEY).ok().flatten();
        let cur = format!("{dead},{cycles}");
        if analyses_changed(prev.as_deref(), &cur) {
            let _ = idx.put_meta(KEY, &cur);
            let payload = serde_json::json!({
                "root": root.display().to_string(),
                "dead_exports": dead,
                "import_cycles": cycles,
            });
            let _ = self.app.emit("graph-analyses", &payload);
        }
    }

    /// V30 Phase C producer: announce a finished **full** index build into every
    /// channel-armed session of this instance.
    ///
    /// **Pull twin (milestone invariant 2): the `graph_*` tools themselves.**
    /// The pushed fact IS queryable state — everything this notice claims is
    /// re-derivable with `graph_stats` / any `graph_*` query, and the Code
    /// Intelligence tab renders the same numbers from `graph-status`. A dropped
    /// push therefore costs timeliness, never information; no new tool needed.
    ///
    /// Gated hard, deliberately. Delivering a push to an IDLE Claude tab
    /// **starts a model turn**, so every notice has a token price:
    ///
    /// - **Settings.** `offload.session_push` is read LIVE here, not cached, so
    ///   turning the feature off stops app-side pushes on the next producer run
    ///   with no restart. (The child-side latch is per-tab-until-restart; this
    ///   is the half that can react immediately.)
    /// - **User-initiated full builds only.** [`Self::spawn_rebuild`] is the
    ///   only call site, and it is reached by four AUTOMATIC paths too — the
    ///   startup build and the settings-enable watcher (`main.rs`), the
    ///   watcher's channel-overflow recovery (`watcher.rs`), the schema-migration
    ///   repair in [`Self::index_for`], and [`Self::reindex_paths`]'
    ///   `DirWalk::TooBig` escalation. None of those is news anybody asked for
    ///   (app launch on a big repo, a large `git checkout`), so only
    ///   [`RebuildOrigin::User`] is push-eligible — the graph twin of the audit
    ///   runner's `Initiator::Gui` gate.
    /// - **`>= GRAPH_PUSH_MIN_BUILD_MS` wall clock.** A fast rebuild is not
    ///   news; the notice exists for builds long enough that an agent gave up
    ///   waiting on the index.
    ///
    /// Best-effort and non-blocking (`try_send` under the hood): no bus, no
    /// subscribers, no channel-armed child, or a full queue all mean "not
    /// delivered" and the rebuild neither retries nor fails.
    fn announce_index_complete(
        &self,
        root: &Path,
        stats: &GraphStats,
        elapsed_ms: u64,
        origin: RebuildOrigin,
    ) {
        let Some(pushes) = self.pushes.as_ref() else {
            return;
        };
        // "Off means off": the gate is read LIVE here, so the producer stops the
        // moment the user unticks it — the child-side latch cannot.
        let session_push = self.settings.current().offload.session_push;
        if !index_push_worthy(session_push, origin, elapsed_ms) {
            return;
        }
        let project = root
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("project");
        // Locked decision 9: a static template plus app-owned values. Every
        // slot below is cImp's own — the project directory name, the indexer's
        // counts, an elapsed time — never model output, a finding message or
        // fetched content. `PushNotice::new` takes `&'static str`, so
        // interpolating anything else here would not compile (#47).
        let notice = crate::offload::service::PushNotice::new(
            "cImp finished a full code-graph index of {} ({}): {} files, {} symbols, {} edges in {}s. The index is live — the graph_* tools now see the current tree.",
            &[
                project,
                &root.display().to_string(),
                &stats.files.to_string(),
                &stats.symbols.to_string(),
                &stats.edges.to_string(),
                &(elapsed_ms / 1000).to_string(),
            ],
            [("kind", "graph_index")],
        );
        let delivered = pushes.push_broadcast(notice);
        debug!(
            root = %root.display(),
            ms = elapsed_ms,
            delivered,
            "graph: pushed index-complete notice"
        );
    }

    /// Kick a non-blocking full rebuild of `root` on a dedicated thread. Returns
    /// immediately; progress lands on the `graph-status` event and via
    /// [`status`](Self::status). A no-op (logged) when a build for this root is
    /// already in flight.
    ///
    /// `origin` says who asked (V30 / review M2). It changes nothing about the
    /// build itself — its only consumer is [`Self::announce_index_complete`],
    /// which pushes a completion notice into every channel-armed session and
    /// must not do so for a rebuild nobody requested. Pass
    /// [`RebuildOrigin::User`] only from a real user action.
    pub fn spawn_rebuild(self: &Arc<Self>, root: PathBuf, origin: RebuildOrigin) {
        // Build the status under the lock but emit AFTER dropping it: `emit`
        // dispatches synchronously on this thread, and a same-thread listener
        // that read `self.statuses()` during delivery would re-lock the
        // non-reentrant `status` mutex and self-deadlock (the discipline
        // `patch_status` already follows).
        let building = {
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
            building
        };
        let _ = self.app.emit(GRAPH_STATUS_EVENT, &building);

        let this = self.clone();
        let thread_root = root.clone();
        let spawned = std::thread::Builder::new()
            .name("cimp-graph-index".into())
            .spawn(move || {
                let root = thread_root;
                let started = std::time::Instant::now();
                match this.rebuild_blocking(&root) {
                    Ok(stats) => {
                        let elapsed_ms = started.elapsed().as_millis() as u64;
                        info!(
                            root = %root.display(),
                            files = stats.files,
                            symbols = stats.symbols,
                            edges = stats.edges,
                            ms = elapsed_ms,
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
                        // V12 Phase E: opportunistic idle-session distillation
                        // sweep (no-op when the setting is off or nothing's idle).
                        this.spawn_distillation_sweep(root.clone());
                        // V12 Phase F (6c): re-check dead-exports/import-cycles
                        // counts and badge the Analyses UI when they changed
                        // (no-op when `analyses_auto` is off). Cheap, read-only
                        // on the just-built index; runs inline on this worker
                        // thread like the status bookkeeping above.
                        this.run_analyses_trigger(&root);
                        // V22 Phase D: opt-in auto-configure of `run_check` from
                        // language detection (no-op unless `checks_auto_configure`
                        // is on and `checks` is empty). Inline like the analyses
                        // trigger above — bounded fs + PATH work.
                        this.checks_auto_configure_trigger(&root);
                        // V30 Phase C: announce an EXPENSIVE, USER-REQUESTED
                        // index build into every channel-armed session (last, so
                        // the push says "done" only once the post-build triggers
                        // above have also run). Gated — see
                        // `announce_index_complete`.
                        this.announce_index_complete(&root, &stats, elapsed_ms, origin);
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
        let _w = self.write_guard();
        let (indexed, stats) = build_tree(&idx, root, &snap, &self.db_subdir())?;

        // Record the visited-file count alongside the authoritative row counts.
        self.patch_status(root, |s| s.files_indexed = indexed);
        Ok(stats)
    }

    /// Kick a non-blocking `graph.ignore` resync of every known root (one
    /// worker thread per root). Called by `settings_update` when the effective
    /// glob list changes: newly-excluded files are dropped from the index,
    /// newly-included ones are indexed. A full rebuild would also converge,
    /// but its in-flight guard SKIPS (not queues) a second trigger, so rapid
    /// edits could leave the final list unapplied — these passes instead
    /// serialize on the store write-lock and each reads the settings fresh,
    /// so the last edit always wins.
    pub fn spawn_ignore_resync(self: &Arc<Self>) {
        let roots: Vec<PathBuf> = self.status.lock().unwrap().keys().cloned().collect();
        for root in roots {
            let this = self.clone();
            let spawned = std::thread::Builder::new()
                .name("cimp-graph-ignore".into())
                .spawn(move || match this.ignore_resync_blocking(&root) {
                    Ok((removed, added)) => {
                        if removed > 0 || added > 0 {
                            info!(
                                root = %root.display(),
                                removed,
                                added,
                                "graph: ignore resync applied"
                            );
                        }
                    }
                    Err(e) => {
                        warn!(root = %root.display(), error = %e, "graph: ignore resync failed");
                    }
                });
            if let Err(e) = spawned {
                warn!(error = %e, "graph: failed to spawn ignore-resync thread");
            }
        }
    }

    /// Synchronous body of the ignore resync: [`resync_tree`] under the store
    /// write-lock, then the same post-index bookkeeping as a watcher batch
    /// (churn refresh, status counts, embedding backfill).
    fn ignore_resync_blocking(self: &Arc<Self>, root: &Path) -> AppResult<(u64, u64)> {
        let snap = self.settings.current().graph;
        if !snap.enabled {
            return Ok((0, 0));
        }
        let idx = self.index_for(root)?;
        let w = self.write_guard();
        let (removed, added, added_rels) = resync_tree(&idx, root, &snap, &self.db_subdir())?;
        if !added_rels.is_empty() {
            if let Ok(churn) = super::gitmeta::collect_for(root, &added_rels) {
                let _ = idx.put_commit_touches(&churn);
            }
        }
        drop(w);

        if removed > 0 || added > 0 {
            if let Ok(stats) = idx.stats() {
                self.patch_status(root, |s| {
                    // Same discipline as `reindex_paths`: a concurrent full
                    // rebuild owns the final `building` → `ready` transition.
                    if !s.building {
                        s.state = "ready".into();
                    }
                    s.files = stats.files;
                    s.symbols = stats.symbols;
                    s.edges = stats.edges;
                    s.langs = stats.by_lang.clone();
                });
            }
            self.spawn_backfill(root.to_path_buf());
        }
        Ok((removed, added))
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
        // V13 §0.3: fan this coalesced batch out to Workbench consumers
        // (fs-batch Tauri event for the frontend + an internal broadcast for
        // backend subscribers) BEFORE any graph-specific filtering below — a
        // batch of paths the graph itself ignores (unsupported extension,
        // gitignored, graph disabled) can still be exactly what the diff pane
        // or a future checkpoint burst trigger cares about. Reached via
        // `AppHandle::state` rather than a constructor dependency so `graph`
        // and `workbench` don't need to know about each other's lifecycle;
        // `WorkbenchService` self-gates on `workbench.enabled`, so this is a
        // cheap no-op when the feature is off.
        if let Some(workbench) = self
            .app
            .try_state::<Arc<crate::workbench::WorkbenchService>>()
        {
            workbench.publish_fs_batch(root, &paths);
        }

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
        let gi = build_gitignore(root, &paths, &snap.ignore);

        let _w = self.write_guard();
        let mut changed = 0u64;
        // V12 Phase D: rel paths touched this batch (indexed OR removed), fed
        // to `gitmeta::collect_for` below for an incremental churn refresh —
        // a small, bounded set since watcher batches are debounced and small.
        let mut touched_rels: Vec<String> = Vec::new();
        for path in paths {
            // Never touch our own store directory.
            if path.components().any(|c| c.as_os_str() == sub.as_str()) {
                continue;
            }
            let rel = rel_path(root, &path);

            // A path that EXISTS as a directory was just created or renamed/
            // moved in. Windows reports an atomic directory rename as one
            // dir-level OLD/NEW pair — the children are never re-reported —
            // so without walking here the moved subtree stays missing from
            // the graph until an unrelated full rebuild (the old-name side is
            // handled by the removal branch below). Walk it with the same
            // tree walker as a full rebuild and index every eligible file.
            // A stored-hash check skips unchanged children, so a redundant
            // dir event costs one read+hash per child, not a re-parse.
            if path.is_dir() {
                match index_dir_tree(&idx, root, &path, &snap, &sub, max_bytes, &gi) {
                    DirWalk::Indexed { indexed, rels } => {
                        changed += indexed;
                        touched_rels.extend(rels);
                    }
                    // Too big for the incremental path: a full rebuild does
                    // the same per-file work on its own thread with progress
                    // reporting and single-flight guarding, rather than
                    // pinning the watcher thread for an unbounded stretch.
                    // It covers this whole batch, so stop processing it.
                    DirWalk::TooBig => {
                        debug!(root = %root.display(), dir = %rel,
                            "graph: moved-in directory exceeds incremental walk cap; full rebuild");
                        drop(_w);
                        // Escalated out of the incremental watcher path — an
                        // automatic rebuild, so it must not push (this is one of
                        // the two paths that used to reach the producer despite
                        // its doc comment claiming the watcher never can).
                        self.spawn_rebuild(root.to_path_buf(), RebuildOrigin::Automatic);
                        return;
                    }
                }
                continue;
            }

            // Deletions/moves-away are handled BEFORE the `lang_for` gate: a
            // removed path may be a directory (rename or `rm -r`), which has no
            // indexable extension and would otherwise be dropped by that gate,
            // leaking every child file's rows until a full rebuild. Drop the
            // exact file if it was indexed AND everything indexed beneath it.
            if !path.is_file() {
                let was_indexed = idx.stored_file_hash(&rel).ok().flatten().is_some();
                if was_indexed {
                    let _ = idx.remove_file(&rel);
                }
                let removed_under = idx.remove_files_under(&rel).unwrap_or(0);
                if was_indexed || removed_under > 0 {
                    changed += 1;
                    touched_rels.push(rel);
                }
                continue;
            }

            // Only files with a configured language matter.
            let Some(lang) = lang_for(&path, &snap.languages) else {
                continue;
            };
            if lang == Lang::Markdown && !snap.index_docs {
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
                Ok(()) => {
                    changed += 1;
                    touched_rels.push(rel);
                }
                Err(e) => debug!(file = %rel, error = %e, "graph: incremental index failed"),
            }
        }
        if changed > 0 {
            // Drop vectors for chunks this batch deleted or re-anchored.
            let _ = idx.prune_orphan_vectors();
            let _ = idx.prune_orphan_code_vectors();
            let _ = idx.prune_orphan_digests();
            // V12 Phase D: incremental churn refresh for just the touched
            // files — one small `git log -1` spawn per path, cheap at
            // watcher-batch scale. No-ops (empty result, no error) outside a
            // git repo.
            if let Ok(churn) = super::gitmeta::collect_for(root, &touched_rels) {
                let _ = idx.put_commit_touches(&churn);
            }
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
            // V12 Phase E: opportunistic idle-session distillation sweep
            // (no-op when the setting is off or nothing's idle).
            self.spawn_distillation_sweep(root.to_path_buf());
            // V12 Phase F (6c): same analyses-auto trigger as a full rebuild —
            // cheap on the just-updated index, already on the watcher thread.
            self.run_analyses_trigger(root);
            // V22 Phase D: same auto-configure trigger as a full rebuild so a
            // user who enables `checks_auto_configure` mid-session gets set up
            // on the next incremental reindex, not only on restart/rebuild
            // (no-op unless the setting is on and `checks` is empty).
            self.checks_auto_configure_trigger(root);
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

    /// Acquire the rebuild-serialization lock, tolerating a poisoned mutex (F9).
    /// `write_lock` guards nothing but `()` — it only serializes writers — so a
    /// panic in a prior holder (e.g. tree-sitter choking on pathological input
    /// mid-rebuild) leaves no corrupt state, and must NOT permanently wedge every
    /// future rebuild/backfill with a poison cascade through each `.unwrap()`.
    /// Recovering the guard from the poison error keeps writers serialized.
    fn write_guard(&self) -> std::sync::MutexGuard<'_, ()> {
        self.write_lock.lock().unwrap_or_else(|e| e.into_inner())
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
            let _w = this.write_guard();
            f()
        })
        .await
        .expect("graph write-lock task panicked")
    }

    /// Embed any doc chunks missing a current-epoch vector and store them. See
    /// [`embed_batch_isolated`] for the poison-chunk containment this relies on.
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
        let Some(mut embedder) = Embedder::new(&snap.embedding_endpoint, &snap.embedding_model)
        else {
            self.patch_status(root, |s| {
                s.embed_state = "degraded".into();
                s.embedder_ready = false;
                s.embed_error = Some("no embedding endpoint configured".into());
            });
            return;
        };

        // Resolve the per-input token budget BEFORE any embed call, so even
        // the dimension probe rides the same fit guarantee. Manual override
        // when set, else a one-per-process `/props` detection that the query
        // paths then inherit from the cache for free. `None` (a server with no
        // usable `/props`) keeps the pre-V31 "send unchanged" behavior.
        embedder.ensure_max_tokens(snap.embedding_max_tokens).await;

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
        // Pin the store dimension so a later model swap on the endpoint can't
        // feed wrong-length vectors into a store sized to `dim`.
        embedder.expect_dim(dim);

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
        // Chunk ids this run gave up on (the embedder rejected them even at the
        // token floor). They must stay OUT of every later selection, or the
        // very next `chunks_needing_vectors` call re-returns the same poison
        // rows and the backfill spins forever on them.
        let mut skipped: HashSet<String> = HashSet::new();
        loop {
            // Widen the request by the skip count so filtering them out still
            // leaves a full batch of embeddable rows behind them.
            let pending = match idx.chunks_needing_vectors(&epoch, batch + skipped.len()) {
                Ok(p) => p,
                Err(e) => {
                    self.patch_status(root, |s| {
                        s.embed_state = "error".into();
                        s.embed_error = Some(e.to_string());
                    });
                    return;
                }
            };
            let pending: Vec<(String, String, String)> = pending
                .into_iter()
                .filter(|(id, _, _)| !skipped.contains(id))
                .collect();
            if pending.is_empty() {
                break;
            }
            let rows = match embed_batch_isolated(&mut embedder, &pending, &mut skipped).await {
                Ok(rows) => rows,
                Err(e) => {
                    // Endpoint went away (or the model changed) mid-backfill —
                    // degrade, keep what we have.
                    self.patch_status(root, |s| {
                        s.embed_state = "degraded".into();
                        s.embedder_ready = false;
                        s.embed_error = Some(e);
                    });
                    return;
                }
            };
            if !rows.is_empty() {
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
        }

        // V11 Phase G: also embed pending symbol bodies for semantic *code*
        // search, sharing this pass's embedder/dim/epoch. Docs run first
        // (cheaper, and doc search stays useful even with code embedding off);
        // code chunks are typically far more numerous, so they ride the same
        // backfill but strictly after. Gated on its own setting — off by
        // default, since it multiplies the vector count.
        //
        // Code chunks get their own skip set (a different id space) but the
        // SAME isolation + adaptive-shrink helper, so a poison symbol body
        // can't stall that loop either.
        let mut code_skipped: HashSet<String> = HashSet::new();
        if snap.embed_code_bodies {
            {
                let idx = idx.clone();
                let model = snap.embedding_model.clone();
                let epoch = epoch.clone();
                if let Err(e) = self
                    .locked_write(move || idx.ensure_code_vector_store(dim, &model, &epoch))
                    .await
                {
                    self.patch_status(root, |s| {
                        s.embed_state = "error".into();
                        s.embed_error = Some(e.to_string());
                    });
                    return;
                }
            }
            loop {
                let pending = match idx.pending_code_chunks(&epoch, batch + code_skipped.len()) {
                    Ok(p) => p,
                    Err(e) => {
                        self.patch_status(root, |s| {
                            s.embed_state = "error".into();
                            s.embed_error = Some(e.to_string());
                        });
                        return;
                    }
                };
                let pending: Vec<(String, String, String)> = pending
                    .into_iter()
                    .filter(|(id, _, _)| !code_skipped.contains(id))
                    .collect();
                if pending.is_empty() {
                    break;
                }
                let rows =
                    match embed_batch_isolated(&mut embedder, &pending, &mut code_skipped).await {
                        Ok(rows) => rows,
                        Err(e) => {
                            // Endpoint went away mid-backfill — degrade, keep what we have.
                            self.patch_status(root, |s| {
                                s.embed_state = "degraded".into();
                                s.embedder_ready = false;
                                s.embed_error = Some(e);
                            });
                            return;
                        }
                    };
                if !rows.is_empty() {
                    let put = {
                        let idx = idx.clone();
                        let epoch = epoch.clone();
                        self.locked_write(move || idx.put_code_vectors(&epoch, &rows))
                            .await
                    };
                    if let Err(e) = put {
                        self.patch_status(root, |s| {
                            s.embed_state = "error".into();
                            s.embed_error = Some(e.to_string());
                        });
                        return;
                    }
                }
            }
            {
                let idx = idx.clone();
                self.locked_write(move || {
                    let _ = idx.prune_orphan_code_vectors();
                })
                .await;
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
        // Every quality signal needs a consumer: chunks the embedder refused
        // are silently missing from semantic search forever, so the count has
        // to reach the user. The run itself succeeded (the endpoint is up and
        // everything else embedded), so the state stays `idle` — the monitor
        // renders an `embed_error` alongside a non-error state as a WARNING,
        // not a failure.
        let skipped_total = skipped.len() + code_skipped.len();
        self.patch_status(root, move |s| {
            s.embed_state = "idle".into();
            s.embedder_ready = true;
            s.embed_error = (skipped_total > 0).then(|| {
                format!(
                    "{skipped_total} chunk{} skipped — the embedder rejected {} even at the \
                     minimum size; everything else is embedded",
                    if skipped_total == 1 { "" } else { "s" },
                    if skipped_total == 1 { "it" } else { "them" },
                )
            });
        });
    }

    /// Recompute `(embedded, total, pending)` for the status from the store.
    fn refresh_embed_coverage(&self, root: &Path, idx: &GraphIndex, epoch: &str, ready: bool) {
        let (embedded, total) = idx.embedding_coverage(epoch).unwrap_or((0, 0));
        // V11 Phase G/F: code-embedding coverage + cached-digest count for the
        // Index/Context readouts (both are cheap DB counts).
        let (code_embedded, code_total) = idx.code_embedding_coverage(epoch).unwrap_or((0, 0));
        let digests = idx.digest_count().unwrap_or(0);
        self.patch_status(root, |s| {
            s.embedded = embedded;
            s.embed_total = total;
            s.embed_pending = total.saturating_sub(embedded);
            s.code_embedded = code_embedded;
            s.code_embed_total = code_total;
            s.digests = digests;
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

/// V12 Phase F (6c): pure gate for [`GraphService::run_analyses_trigger`] —
/// whether the freshly computed counts (`cur`, the `"{dead},{cycles}"` string)
/// differ from what was last stored (`prev`). Factored out so "the event
/// fires only when counts changed" is testable without a `GraphIndex`/
/// `AppHandle`. `None` (nothing stored yet) always counts as a change — the
/// first successful pass this project has ever run IS new information.
fn analyses_changed(prev: Option<&str>, cur: &str) -> bool {
    prev != Some(cur)
}

/// V30 (review M2): who asked for a full rebuild
/// ([`GraphService::spawn_rebuild`]). The graph twin of the audit runner's
/// `Initiator` (`audit/runner.rs`): only a rebuild a human actually
/// requested may announce itself on the session-push bus, because delivering a
/// notice to an idle Claude tab **starts a model turn**. Everything automatic —
/// the startup build, the settings-enable watcher, the watcher's
/// channel-overflow recovery, the schema-migration repair, the moved-in-directory
/// escalation out of the incremental path — happens without anyone waiting, and
/// would otherwise start a turn in every armed tab on app launch or after a
/// large `git checkout`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RebuildOrigin {
    /// A user action: the Code Intelligence tab's Rebuild button
    /// (`graph_rebuild`) or a Settings language toggle
    /// (`graph_set_language_enabled`).
    User,
    /// Machinery: startup, runtime enable, watcher recovery, schema migration,
    /// incremental-walk escalation. Never pushes.
    Automatic,
}

/// V30 Phase C: the wall-clock floor for the index-completion push. Below this
/// the build was cheap enough that nobody was waiting on it, and the notice
/// would cost more (an idle Claude tab starts a model turn on delivery) than the
/// information is worth.
const GRAPH_PUSH_MIN_BUILD_MS: u64 = 30_000;

/// V30 Phase C: the complete gate for
/// [`GraphService::announce_index_complete`] — pure so "only expensive builds a
/// user asked for, and only while the feature is on, announce themselves" is
/// testable without an `AppHandle`, a store, or a push bus. The
/// full-vs-incremental half is structural (only `spawn_rebuild` calls the
/// producer at all).
///
/// `session_push` comes from a LIVE settings read at fire time (review M6): the
/// child-side declaration is latched per tab until restart, so the producer is
/// the half that can make "off" mean off immediately.
fn index_push_worthy(session_push: bool, origin: RebuildOrigin, elapsed_ms: u64) -> bool {
    session_push
        && matches!(origin, RebuildOrigin::User)
        && elapsed_ms >= GRAPH_PUSH_MIN_BUILD_MS
}

/// Make a `&str` `path` project-relative to `root` with `/` separators (empty in
/// → empty out). Delegates to [`rel_path`] so memory-event paths and the
/// indexer's stored file paths are relativized identically.
fn relativize_path(root: &Path, path: &str) -> String {
    if path.trim().is_empty() {
        return String::new();
    }
    rel_path(root, Path::new(path))
}

/// V17 Phase B5: whether a Bash command is a provable whole-file read the read
/// advisor already accounts for, so the bypass tap must skip it. Only meaningful
/// when the shell sub-toggle is on (otherwise the Bash hook isn't installed and
/// such a command really is an un-intercepted read the canary should score).
fn intercepted_whole_file_read(shell_on: bool, command: &str) -> bool {
    shell_on && super::shellread::whole_file_read(command).is_some()
}

/// V16 Feature 4: extract path-like candidate tokens from a shell command —
/// quoted segments (single or double) plus whitespace-split tokens that
/// contain a path separator. Deliberately NOT a shell parser (the milestone
/// spec's "simple heuristic"): the consumer only ever compares candidates
/// against a small set of just-reminded files, so false candidates cost
/// nothing and false negatives only under-count (events are labeled est.).
fn path_like_tokens(command: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut push = |t: &str| {
        let t = t
            .trim()
            .trim_matches(|c| c == ',' || c == ';' || c == ')' || c == '(');
        if t.len() > 1 && out.iter().all(|p| p != t) {
            out.push(t.to_string());
        }
    };
    // Quoted segments, both kinds — a path with spaces only survives here.
    for quote in ['"', '\''] {
        let mut parts = command.split(quote);
        parts.next(); // before the first quote
        while let (Some(inside), rest) = (parts.next(), parts.next()) {
            push(inside);
            if rest.is_none() {
                break;
            }
        }
    }
    for tok in command.split_whitespace() {
        if tok.contains('/') || tok.contains('\\') {
            push(tok.trim_matches(|c| c == '"' || c == '\''));
        }
    }
    out
}

/// V16 Feature 4: whether a command token plausibly refers to the reminded
/// file at (project-relative, `/`-separated) `rel`. Full-path match, a
/// longer path ENDING in the relative path (an absolute spelling of it), or
/// a bare basename match — normalized to `/` so `src\a.rs` and `src/a.rs`
/// compare equal.
fn token_matches_path(token: &str, rel: &str) -> bool {
    let norm = token.replace('\\', "/");
    let norm = norm.trim_end_matches('/');
    if norm.is_empty() || rel.is_empty() {
        return false;
    }
    if norm == rel {
        return true;
    }
    if norm.len() > rel.len() && norm.ends_with(rel) {
        // Require a boundary before the suffix so `notsrc/a.rs` doesn't
        // match `src/a.rs`.
        let boundary = norm.as_bytes()[norm.len() - rel.len() - 1];
        if boundary == b'/' {
            return true;
        }
    }
    let base = rel.rsplit('/').next().unwrap_or(rel);
    let tok_base = norm.rsplit('/').next().unwrap_or(norm);
    !base.is_empty() && tok_base == base
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
    // V11 Phase G: a simple project-wide count cap on `code_chunk` rows
    // (order-dependent on the walk order, which is acceptable for V1 — see
    // `GraphSettings::semantic_code_max_chunks`). Only enforced on a full
    // rebuild; the incremental watcher path doesn't re-check the running
    // total against the rest of the project.
    let mut code_chunk_budget = snap.semantic_code_max_chunks as usize;

    let mut indexed: u64 = 0;
    for entry in build_walker(root, snap) {
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
        let mut fg = parse_file(&rel, &src, lang);
        if fg.code_chunks.len() > code_chunk_budget {
            fg.code_chunks.truncate(code_chunk_budget);
        }
        code_chunk_budget = code_chunk_budget.saturating_sub(fg.code_chunks.len());
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
    let _ = idx.prune_orphan_code_vectors();
    // V11 Phase F: likewise drop cached digests for files that vanished.
    let _ = idx.prune_orphan_digests();

    // V12 Phase D: refresh git churn metadata for the ranking boost + digest
    // trailers. `commit_touch` is additive (outside `RELATIONS`, ensured by
    // `ensure_memory_relations`), so it survives the `reset()` above and just
    // gets repopulated here every full pass. `collect` itself degrades to an
    // empty vec (never an error) when `root` isn't a git repo, so this is
    // always safe to call — a non-git project just gets no churn boost.
    if let Ok(churn) = super::gitmeta::collect(root) {
        let _ = idx.put_commit_touches(&churn);
    }

    Ok((indexed, idx.stats()?))
}

/// The shared tree walker for a rebuild and the language census, so the two
/// agree exactly on what counts as "in the project" (gitignore + global +
/// exclude + parents, dotfiles included, plus the user's extra `ignore` globs).
/// The db-subdir and per-file size/language filtering are applied by callers.
///
/// V13 Phase D: the `<db_subdir>/` override below (default `.cimp/`) is
/// unconditional — not gated on the user's own `.gitignore` containing it —
/// so this walker never DESCENDS into it at all, rather than relying on
/// callers' post-hoc `path.components().any(|c| c.as_os_str() == db_subdir)`
/// filter alone. That filter is still correct (and kept, as defense in
/// depth), but a project that hasn't gitignored `.cimp/` would otherwise have
/// this walker step through the shadow checkpoint repo's whole object store
/// AND every worktree's full checkout under `.cimp/worktrees/<slug>/` on
/// every rebuild — the worktree case in particular can be as large as the
/// project itself, multiplied per open worktree.
fn build_walker(root: &Path, snap: &GraphSettings) -> ignore::Walk {
    let mut wb = WalkBuilder::new(root);
    wb.hidden(false) // index dotfiles like `.github/*.md`; the db dir is filtered by callers
        .git_ignore(true)
        .git_global(true)
        .git_exclude(true)
        .parents(true);
    // Honor the user's extra ignore globs (additive to `.gitignore`), plus
    // the always-on `<db_subdir>/` exclusion above. An `Override` whose
    // patterns are *ignore* globs needs each prefixed with `!` (overrides are
    // whitelists; a leading `!` flips one to a blacklist).
    let mut ob = ignore::overrides::OverrideBuilder::new(root);
    let _ = ob.add(&format!("!{}/", snap.effective_db_subdir()));
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
    wb.build()
}

/// Reconcile the store with the CURRENT `graph.ignore` globs without a full
/// rebuild: drop every indexed file the globs now exclude, then (hash-skip)
/// index every eligible file they no longer exclude. Unlike [`build_tree`]
/// there's no `reset()`, so unchanged files keep their rows and vectors.
/// Returns `(removed, added, added_rels)`.
///
/// Two passes because they answer different questions: the walker below can
/// only visit files that exist OUTSIDE ignored trees — it can never say "this
/// stored row is now ignored" — so pass 1 tests each stored path against the
/// glob matcher directly, and pass 2 is the same walk as a full rebuild
/// (which honors the new globs via its overrides) with a stored-hash check so
/// an already-indexed unchanged file costs one read+hash, not a re-parse.
fn resync_tree(
    idx: &GraphIndex,
    root: &Path,
    snap: &GraphSettings,
    db_subdir: &str,
) -> AppResult<(u64, u64, Vec<String>)> {
    let matcher = gitignore_from_globs(root, &snap.ignore);
    let mut removed = 0u64;
    for rel in idx.all_file_paths()? {
        let abs = root.join(&rel);
        if matcher.matched_path_or_any_parents(&abs, false).is_ignore()
            && idx.remove_file(&rel).is_ok()
        {
            removed += 1;
        }
    }

    let max_bytes = snap.max_file_bytes.max(1);
    let mut added = 0u64;
    let mut added_rels: Vec<String> = Vec::new();
    for entry in build_walker(root, snap) {
        let Ok(entry) = entry else { continue };
        if entry.file_type().map(|t| !t.is_file()).unwrap_or(true) {
            continue;
        }
        let path = entry.path();
        if path.components().any(|c| c.as_os_str() == db_subdir) {
            continue;
        }
        let Some(lang) = lang_for(path, &snap.languages) else {
            continue;
        };
        if lang == Lang::Markdown && !snap.index_docs {
            continue;
        }
        match entry.metadata() {
            Ok(m) if m.len() > max_bytes => continue,
            Ok(_) => {}
            Err(_) => continue,
        }
        let src = match std::fs::read_to_string(path) {
            Ok(s) => s,
            Err(_) => continue,
        };
        let rel = rel_path(root, path);
        if idx.stored_file_hash(&rel).ok().flatten().as_deref()
            == Some(super::model::fnv1a_hex(&src).as_str())
        {
            continue;
        }
        let fg = parse_file(&rel, &src, lang);
        if let Err(e) = idx.index_file_graph(&fg) {
            debug!(file = %rel, error = %e, "graph: ignore-resync index failed (skipped)");
            continue;
        }
        added += 1;
        added_rels.push(rel);
    }

    if removed > 0 || added > 0 {
        let _ = idx.prune_orphan_vectors();
        let _ = idx.prune_orphan_code_vectors();
        let _ = idx.prune_orphan_digests();
    }
    Ok((removed, added, added_rels))
}

/// Outcome of [`index_dir_tree`] for one moved-in/created directory.
enum DirWalk {
    /// Walked and indexed inline: how many files changed, and their rel paths
    /// (for the caller's incremental churn refresh).
    Indexed { indexed: u64, rels: Vec<String> },
    /// The subtree holds more eligible files than the incremental cap; the
    /// caller should fall back to a full rebuild.
    TooBig,
}

/// Index every eligible file under `dir` (a directory that just appeared in a
/// watcher batch). Windows reports an atomic directory rename as one
/// dir-level OLD/NEW event pair — the children are never re-reported — so
/// without this walk a renamed/moved-in subtree would stay missing from the
/// graph until an unrelated full rebuild. Uses the same tree walker as a full
/// rebuild (gitignore + user ignore globs + db-subdir exclusion), the same
/// language/size gates as the per-file incremental path, and skips children
/// whose stored content hash already matches (e.g. their own file events
/// landed in the same batch), so a redundant directory event costs one
/// read+hash per child, not a re-parse.
///
/// `gi` must cover `dir` itself: the walker below STARTS at `dir`, and the
/// `ignore` crate never matches a walk root against ignore rules — so a
/// gitignore rule that excludes the directory (e.g. `dist` written by a
/// frontend build) would silently not fire, and the whole artifact subtree
/// (minified bundles included) would be parsed into the graph. That exact
/// leak polluted this repo's own graph with `dist/assets/*.js`.
fn index_dir_tree(
    idx: &GraphIndex,
    root: &Path,
    dir: &Path,
    snap: &GraphSettings,
    sub: &str,
    max_bytes: u64,
    gi: &Gitignore,
) -> DirWalk {
    const MAX_DIR_WALK: usize = 4096;
    if gi.matched_path_or_any_parents(dir, true).is_ignore() {
        return DirWalk::Indexed {
            indexed: 0,
            rels: Vec::new(),
        };
    }
    let mut eligible = 0usize;
    let mut indexed = 0u64;
    let mut rels: Vec<String> = Vec::new();
    for entry in build_walker(dir, snap) {
        let Ok(entry) = entry else { continue };
        if !entry.file_type().is_some_and(|t| t.is_file()) {
            continue;
        }
        let child = entry.into_path();
        if child.components().any(|c| c.as_os_str() == sub) {
            continue;
        }
        let Some(lang) = lang_for(&child, &snap.languages) else {
            continue;
        };
        if lang == Lang::Markdown && !snap.index_docs {
            continue;
        }
        match std::fs::metadata(&child) {
            Ok(m) if m.len() > max_bytes => continue,
            Ok(_) => {}
            Err(_) => continue,
        }
        eligible += 1;
        if eligible > MAX_DIR_WALK {
            return DirWalk::TooBig;
        }
        let src = match std::fs::read_to_string(&child) {
            Ok(s) => s,
            Err(_) => continue,
        };
        let rel = rel_path(root, &child);
        if idx.stored_file_hash(&rel).ok().flatten().as_deref()
            == Some(super::model::fnv1a_hex(&src).as_str())
        {
            continue;
        }
        let fg = parse_file(&rel, &src, lang);
        match idx.index_file_graph(&fg) {
            Ok(()) => {
                indexed += 1;
                rels.push(rel);
            }
            Err(e) => debug!(file = %rel, error = %e, "graph: incremental index failed"),
        }
    }
    DirWalk::Indexed { indexed, rels }
}

/// One row of the project **language census**: a language present on disk, how
/// many files it has, and how the graph relates to it. Drives the Code Graph
/// tab's green/yellow/red language buttons.
///
/// - `supported && enabled` → green (indexed by the graph).
/// - `supported && !enabled` → yellow (the engine can index it, but it isn't in
///   `GraphSettings.languages`).
/// - `!supported` → red (a known-but-unsupported programming language, or the
///   catch-all "other" bucket).
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct LangCensus {
    /// Stable key: a supported [`Lang`] tag (`"rust"`), a known-unsupported
    /// programming-language slug (`"zig"`), or `"other"` for the catch-all.
    pub key: String,
    /// Human display label (`"Rust"`, `"Zig"`, `"Other"`).
    pub label: String,
    /// Number of files of this language found in the project tree.
    pub files: u64,
    /// The graph engine can index this language (a concrete `Lang` variant).
    pub supported: bool,
    /// The language's tag is currently in `GraphSettings.languages`.
    pub enabled: bool,
}

/// Group-and-files sort rank for the census: green (0) → yellow (1) → red-known
/// (2) → the "other" bucket (3, always last).
fn census_rank(e: &LangCensus) -> u8 {
    if e.key == "other" {
        3
    } else if !e.supported {
        2
    } else if e.enabled {
        0
    } else {
        1
    }
}

/// Walk `root` and tally every source file by detected language, *without* the
/// `languages` allowlist filter — so the result includes supported-but-not-
/// indexed languages (yellow) and unsupported ones (red), which the indexed
/// `file` relation never records. Reuses [`build_walker`] so the file set
/// matches a rebuild's exactly. Best-effort and non-fatal: unreadable entries
/// are skipped, never surfaced as errors.
fn language_census(root: &Path, snap: &GraphSettings, db_subdir: &str) -> Vec<LangCensus> {
    use std::collections::BTreeMap;

    let mut supported: BTreeMap<&'static str, u64> = BTreeMap::new();
    let mut known: BTreeMap<&'static str, (&'static str, u64)> = BTreeMap::new();
    let mut other: u64 = 0;

    for entry in build_walker(root, snap) {
        let Ok(entry) = entry else { continue };
        if entry.file_type().map(|t| !t.is_file()).unwrap_or(true) {
            continue;
        }
        let path = entry.path();
        // Never count our own store directory.
        if path.components().any(|c| c.as_os_str() == db_subdir) {
            continue;
        }
        let lang = Lang::from_path(path);
        if lang != Lang::Other {
            *supported.entry(lang.tag()).or_default() += 1;
        } else if let Some((slug, label)) = super::model::unsupported_lang_name(path) {
            let e = known.entry(slug).or_insert((label, 0));
            e.1 += 1;
        } else {
            other += 1;
        }
    }

    let langs = &snap.languages;
    let mut out: Vec<LangCensus> = Vec::new();
    for (tag, files) in supported {
        out.push(LangCensus {
            key: tag.to_string(),
            label: Lang::from_tag(tag).label().to_string(),
            files,
            supported: true,
            enabled: langs.iter().any(|l| l == tag),
        });
    }
    for (slug, (label, files)) in known {
        out.push(LangCensus {
            key: slug.to_string(),
            label: label.to_string(),
            files,
            supported: false,
            enabled: false,
        });
    }
    if other > 0 {
        out.push(LangCensus {
            key: "other".to_string(),
            label: "Other".to_string(),
            files: other,
            supported: false,
            enabled: false,
        });
    }

    out.sort_by(|a, b| {
        census_rank(a)
            .cmp(&census_rank(b))
            .then_with(|| b.files.cmp(&a.files))
            .then_with(|| a.label.cmp(&b.label))
    });
    out
}

/// Project-relative path with forward slashes, matching what the parser stores
/// and the MCP tools query against. The fs walk always strips cleanly; the
/// case-insensitive fallback exists for agent-supplied absolute paths (memory
/// events) that on Windows can differ from `root` only in drive/dir case — the
/// tail keeps its original casing, which matches the indexed file. Returns the
/// forward-slashed path unchanged when it isn't under `root`.
fn rel_path(root: &Path, path: &Path) -> String {
    // Fast, exact path (always taken by the indexer's own walk).
    if let Ok(rel) = path.strip_prefix(root) {
        return rel.to_string_lossy().replace('\\', "/");
    }
    // Case-insensitive root-prefix strip.
    let path_s = path.to_string_lossy().replace('\\', "/");
    let root_s = root.to_string_lossy().replace('\\', "/");
    let root_trim = root_s.trim_end_matches('/');
    let rl = root_trim.len();
    if !root_trim.is_empty()
        && path_s.len() > rl
        && path_s.is_char_boundary(rl)
        && path_s.as_bytes()[rl] == b'/'
        && path_s[..rl].eq_ignore_ascii_case(root_trim)
    {
        return path_s[rl + 1..].to_string();
    }
    path_s
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

/// Embed one batch with **per-item failure isolation**, shared by the doc and
/// code backfill loops.
///
/// The failure this exists for: one chunk the server refuses (typically
/// oversized) fails the *whole* batch with a non-2xx, and because the same
/// chunk is re-selected on the next pass, embedding stalls permanently. So a
/// batch failure is never fatal on its own — the items are retried one at a
/// time, and only the individual offender is dropped.
///
/// Returns the vectors that DID embed (possibly fewer than `pending`, possibly
/// none) and grows `skipped` with the chunk ids that were given up on.
/// `Err` is reserved for failures that mean the endpoint is gone or the model
/// behind it changed ([`embed::is_item_level_error`] draws the line) — those
/// must still degrade-and-stop, because retrying per item would fail
/// identically for every item and misreport an outage as skipped chunks.
async fn embed_batch_isolated(
    embedder: &mut Embedder,
    pending: &[(String, String, String)],
    skipped: &mut HashSet<String>,
) -> Result<Vec<(String, String, Vec<f32>)>, String> {
    let texts: Vec<String> = pending.iter().map(|(_, _, t)| t.clone()).collect();
    match embedder.embed(&texts).await {
        Ok(vectors) if vectors.len() == pending.len() => {
            return Ok(pending
                .iter()
                .zip(vectors)
                .map(|((id, hash, _), v)| (id.clone(), hash.clone(), v))
                .collect());
        }
        // `embed` already guarantees the count matches, so this is defensive:
        // treat a short response like any other per-request rejection.
        Ok(_) => {}
        Err(e) if !embed::is_item_level_error(&e) => return Err(e),
        Err(e) => {
            debug!(error = %e, items = pending.len(), "embed batch rejected — retrying per item");
        }
    }
    let mut rows = Vec::with_capacity(pending.len());
    for (id, hash, text) in pending {
        match embed_item_isolated(embedder, text).await {
            ItemOutcome::Ok(v) => rows.push((id.clone(), hash.clone(), v)),
            ItemOutcome::Down(e) => return Err(e),
            ItemOutcome::Skip(e) => {
                warn!(chunk = %id, error = %e, "embedder rejected chunk — skipping it this run");
                skipped.insert(id.clone());
            }
        }
    }
    Ok(rows)
}

/// What happened to one isolated item.
enum ItemOutcome {
    Ok(Vec<f32>),
    /// The endpoint (not the item) is the problem — abort the run.
    Down(String),
    /// The server refuses this item at any size we're willing to try.
    Skip(String),
}

/// Embed a single item, halving the token budget on failure down to
/// [`embed::MIN_TOKEN_LIMIT`].
///
/// Why shrink at all: `/props` reports `n_ctx`, but a llama-server's real
/// per-request bound for *pooled* embeddings can be the physical batch size
/// (`n_ubatch`), which `/props` does not report. Detection can therefore
/// overestimate, and the only way to find the true bound is to measure it. A
/// size that works is fed back via `lower_max_tokens`, so the run (and every
/// later handle in this process) self-heals to the real bound instead of
/// repeating the search for every item.
async fn embed_item_isolated(embedder: &mut Embedder, text: &str) -> ItemOutcome {
    let input = [text.to_string()];
    let first = match embedder.embed(&input).await {
        Ok(mut v) if v.len() == 1 => return ItemOutcome::Ok(v.pop().unwrap_or_default()),
        Ok(_) => "empty embedding response".to_string(),
        Err(e) if !embed::is_item_level_error(&e) => return ItemOutcome::Down(e),
        Err(e) => e,
    };
    // Nothing to shrink against (no detected window, no override): the server
    // dislikes this item for a reason we can't act on.
    let Some(start) = embedder.max_tokens() else {
        return ItemOutcome::Skip(first);
    };
    let mut limit = start;
    let mut last = first;
    while limit > embed::MIN_TOKEN_LIMIT {
        limit = (limit / 2).max(embed::MIN_TOKEN_LIMIT);
        // Trial on a clone so a failed attempt can't shrink the run's budget.
        let mut trial = embedder.clone();
        trial.set_max_tokens(limit);
        match trial.embed(&input).await {
            Ok(mut v) if v.len() == 1 => {
                embedder.lower_max_tokens(limit);
                return ItemOutcome::Ok(v.pop().unwrap_or_default());
            }
            Ok(_) => last = "empty embedding response".to_string(),
            Err(e) if !embed::is_item_level_error(&e) => return ItemOutcome::Down(e),
            Err(e) => last = e,
        }
    }
    ItemOutcome::Skip(last)
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
///
/// `extra` is the settings `graph.ignore` globs, appended AFTER the
/// `.gitignore` files (so, per last-match-wins, they take precedence).
/// Without them the watcher path disagreed with `build_walker`: a file
/// excluded only by the settings globs was re-indexed on its next save,
/// silently undoing the exclusion until the next full rebuild.
fn build_gitignore(root: &Path, paths: &[PathBuf], extra: &[String]) -> Gitignore {
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
    for pat in extra {
        let pat = pat.trim();
        if pat.is_empty() {
            continue;
        }
        let _ = b.add_line(None, pat);
    }
    b.build().unwrap_or_else(|_| Gitignore::empty())
}

/// A gitignore-semantics matcher built from the settings `graph.ignore` globs
/// alone (rooted at `root`) — the same lines `build_walker` feeds its
/// overrides, so the resync drop-pass and the walk agree on what's excluded.
/// `!` re-includes work natively (whitelist lines); invalid or empty globs are
/// skipped like everywhere else.
fn gitignore_from_globs(root: &Path, globs: &[String]) -> Gitignore {
    let mut b = ignore::gitignore::GitignoreBuilder::new(root);
    for pat in globs {
        let pat = pat.trim();
        if pat.is_empty() {
            continue;
        }
        let _ = b.add_line(None, pat);
    }
    b.build().unwrap_or_else(|_| Gitignore::empty())
}

/// The warm-handle cache core of [`GraphService::index_for`], free of the
/// `AppHandle` so the keying invariant is directly testable. Returns the
/// cached-or-freshly-opened handle plus whether THIS open migrated a stale
/// store. The lock is held across the whole check-open-insert (see
/// `index_for`'s doc for why), and the canonicalized root is used for both the
/// key and the open so one SQLite file never backs two cozo storages.
fn warm_index(
    indices: &StdMutex<HashMap<PathBuf, Arc<GraphIndex>>>,
    root: &Path,
    db_subdir: &str,
) -> AppResult<(Arc<GraphIndex>, bool)> {
    let key = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    let mut guard = indices.lock().unwrap();
    if let Some(idx) = guard.get(&key).cloned() {
        return Ok((idx, false));
    }
    let idx = Arc::new(GraphIndex::open(&key, db_subdir)?);
    // Read (once) whether this open had to reset a stale-schema store.
    let migrated = idx.take_schema_reset();
    guard.insert(key, idx.clone());
    Ok((idx, migrated))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::GraphSettings;

    /// One project dir reached under two spellings must yield the SAME warm
    /// handle. The loopback canonicalizes its root (`\\?\P:\…` on Windows)
    /// while IPC and the taps pass the plain spelling; keying the cache by the
    /// raw `PathBuf` opened a second cozo storage over the same `graph.db` in
    /// one process, with independent locks — the flap this guards against.
    #[test]
    fn one_root_two_spellings_share_one_warm_handle() {
        let dir = std::env::temp_dir().join(format!("ckg-key-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let indices = StdMutex::new(HashMap::new());
        let sub = ".ckg-test";

        // The plain spelling, then the canonicalized one (which on Windows is
        // the verbatim `\\?\` form — a different `PathBuf`, same directory).
        let (plain, _) = warm_index(&indices, &dir, sub).expect("open plain");
        let canon = std::fs::canonicalize(&dir).expect("canonicalize");
        assert_ne!(canon, dir, "the two spellings must actually differ");
        let (verbatim, _) = warm_index(&indices, &canon, sub).expect("open canonical");

        assert!(Arc::ptr_eq(&plain, &verbatim), "one file, one handle");
        assert_eq!(indices.lock().unwrap().len(), 1, "one cache entry");

        drop(plain);
        drop(verbatim);
        indices.lock().unwrap().clear();
        let _ = std::fs::remove_dir_all(&dir);
    }

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

    /// The `graph.ignore` resync (a Settings edit) in both directions: adding
    /// a glob drops the matching indexed file WITHOUT a reset (the untouched
    /// neighbor's rows survive), removing the glob indexes the file again —
    /// and only it, since the hash-skip spares the unchanged neighbor.
    #[test]
    fn ignore_resync_drops_and_restores() {
        let dir = std::env::temp_dir().join(format!("ckg-ign-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::create_dir_all(dir.join("gen")).unwrap();
        std::fs::write(dir.join("src/lib.rs"), "pub fn alpha() {}\n").unwrap();
        std::fs::write(dir.join("gen/out.rs"), "pub fn generated() {}\n").unwrap();

        let sub = ".ckg-test";
        let mut snap = GraphSettings::default();
        let idx = GraphIndex::open(&dir, sub).expect("open");
        build_tree(&idx, &dir, &snap, sub).expect("build");
        assert!(!idx.find_symbol("generated").unwrap().is_empty());

        // Ignore `/gen/`: its file's rows drop, the neighbor's survive.
        snap.ignore = vec!["/gen/".to_string()];
        let (removed, added, _) = resync_tree(&idx, &dir, &snap, sub).expect("resync drop");
        assert_eq!((removed, added), (1, 0));
        assert!(idx.find_symbol("generated").unwrap().is_empty());
        assert!(!idx.find_symbol("alpha").unwrap().is_empty());

        // Un-ignore: the file is indexed again — and ONLY it (hash-skip).
        snap.ignore.clear();
        let (removed2, added2, rels) = resync_tree(&idx, &dir, &snap, sub).expect("resync add");
        assert_eq!((removed2, added2), (0, 1));
        assert_eq!(rels, vec!["gen/out.rs".to_string()]);
        assert!(!idx.find_symbol("generated").unwrap().is_empty());

        drop(idx);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Regression for the directory-rename staleness bug: a moved-in directory
    /// arrives from the watcher as a single dir-level path (Windows never
    /// re-reports the children), and used to be a silent no-op — the subtree
    /// stayed missing until an unrelated full rebuild. `index_dir_tree` must
    /// walk and index it, and a second pass must skip unchanged children.
    #[test]
    fn moved_in_directory_is_walked_and_indexed() {
        let dir = std::env::temp_dir().join(format!("ckg-mvdir-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(dir.join("src/lib.rs"), "pub fn alpha() {}\n").unwrap();

        let sub = ".ckg-test";
        let snap = GraphSettings::default();
        let idx = GraphIndex::open(&dir, sub).expect("open");
        build_tree(&idx, &dir, &snap, sub).expect("initial build");
        assert!(!idx.find_symbol("alpha").unwrap().is_empty());

        // Simulate `mv src srcnew`: the watcher batch carries only the two
        // directory paths — the removal branch drops the old side...
        std::fs::rename(dir.join("src"), dir.join("srcnew")).unwrap();
        idx.remove_files_under("src").expect("remove old side");
        assert!(idx.find_symbol("alpha").unwrap().is_empty());

        // ...and the walk must index the new side.
        match index_dir_tree(
            &idx,
            &dir,
            &dir.join("srcnew"),
            &snap,
            sub,
            u64::MAX,
            &Gitignore::empty(),
        ) {
            DirWalk::Indexed { indexed, rels } => {
                assert_eq!(indexed, 1, "one child file indexed");
                assert_eq!(rels, vec!["srcnew/lib.rs".to_string()]);
            }
            DirWalk::TooBig => panic!("one file is not too big"),
        }
        let hits = idx.find_symbol("alpha").unwrap();
        assert!(
            hits.iter().any(|s| s.file == "srcnew/lib.rs"),
            "alpha lives under the new directory: {hits:?}"
        );

        // Idempotence: unchanged children are hash-skipped on a repeat event.
        match index_dir_tree(
            &idx,
            &dir,
            &dir.join("srcnew"),
            &snap,
            sub,
            u64::MAX,
            &Gitignore::empty(),
        ) {
            DirWalk::Indexed { indexed, .. } => assert_eq!(indexed, 0, "nothing re-indexed"),
            DirWalk::TooBig => panic!("one file is not too big"),
        }

        drop(idx);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Regression for the gitignored-directory leak: a dir event for an
    /// ignored directory (a frontend build recreating `dist/`) used to be
    /// walked anyway — `index_dir_tree`'s walker starts INSIDE the dir, so the
    /// parent `.gitignore` rule excluding the dir itself never fired, and the
    /// minified bundles were parsed into the graph (thousands of one-letter
    /// symbols + `new`/`get`/`set` hubs that then exploded the viz snapshot).
    #[test]
    fn ignored_directory_event_is_not_walked() {
        let dir = std::env::temp_dir().join(format!("ckg-igdir-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(dir.join("dist/assets")).unwrap();
        std::fs::write(dir.join(".gitignore"), "dist\n").unwrap();
        std::fs::write(
            dir.join("dist/assets/app.js"),
            "export function bundled() {}\n",
        )
        .unwrap();

        let sub = ".ckg-test";
        let snap = GraphSettings::default();
        let idx = GraphIndex::open(&dir, sub).expect("open");

        // Same matcher construction as `reindex_paths` for this batch.
        let gi = build_gitignore(&dir, &[dir.join("dist/assets")], &[]);

        // The dir itself and any subdir of it must both be no-ops.
        for target in ["dist", "dist/assets"] {
            match index_dir_tree(&idx, &dir, &dir.join(target), &snap, sub, u64::MAX, &gi) {
                DirWalk::Indexed { indexed, rels } => {
                    assert_eq!(indexed, 0, "{target}: nothing indexed");
                    assert!(rels.is_empty(), "{target}: no touched rels");
                }
                DirWalk::TooBig => panic!("{target}: ignored dir must not be walked at all"),
            }
        }
        assert!(
            idx.find_symbol("bundled").unwrap().is_empty(),
            "bundle symbol never indexed"
        );

        drop(idx);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The language census sees every language on disk (not just indexed ones)
    /// and classifies each: a supported+allowlisted lang is green (enabled), a
    /// supported-but-not-allowlisted lang is yellow, a known-but-unsupported
    /// programming language is a named red chip, and anything else folds into
    /// the single "other" bucket.
    #[test]
    fn language_census_classifies_green_yellow_red_and_other() {
        let dir = std::env::temp_dir().join(format!("ckg-census-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(dir.join("src/lib.rs"), "pub fn f() {}\n").unwrap(); // rust → green
        std::fs::write(dir.join("page.html"), "<h1>hi</h1>\n").unwrap(); // html → yellow (off by default)
        std::fs::write(dir.join("main.zig"), "pub fn main() void {}\n").unwrap(); // zig → red (named)
        std::fs::write(dir.join("data.bin"), "\0\0\0").unwrap(); // unknown → other
        std::fs::write(dir.join("notes.unknownext"), "x\n").unwrap(); // unknown → other

        let snap = GraphSettings::default(); // rust on, html off
        let census = language_census(&dir, &snap, ".ckg-test");

        let get = |key: &str| census.iter().find(|e| e.key == key).cloned();

        let rust = get("rust").expect("rust present");
        assert!(rust.supported && rust.enabled, "rust green: {rust:?}");
        assert_eq!(rust.files, 1);
        assert_eq!(rust.label, "Rust");

        let html = get("html").expect("html present");
        assert!(html.supported && !html.enabled, "html yellow: {html:?}");

        let zig = get("zig").expect("zig present");
        assert!(!zig.supported && !zig.enabled, "zig red: {zig:?}");
        assert_eq!(zig.label, "Zig");

        let other = get("other").expect("other bucket present");
        assert!(!other.supported, "other red: {other:?}");
        assert_eq!(other.files, 2, "bin + unknownext fold into other");

        // Green sorts ahead of the "other" bucket, which is always last.
        assert_eq!(census.first().map(|e| e.key.as_str()), Some("rust"));
        assert_eq!(census.last().map(|e| e.key.as_str()), Some("other"));

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
        let snap_off = GraphSettings {
            index_docs: false,
            ..GraphSettings::default()
        };
        build_tree(&idx, &dir, &snap_off, sub).expect("rebuild2");
        assert!(idx.search_docs("frobnicator", 10, 200).unwrap().is_empty());

        drop(idx);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn semantic_code_max_chunks_caps_total_across_a_rebuild() {
        // Two files, each with one chunk-eligible function. A budget of 1
        // must cap the project-wide `code_chunk` total at 1, regardless of
        // which file the walk visits first.
        let dir = std::env::temp_dir().join(format!("ckg-codecap-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(
            dir.join("src/a.rs"),
            "pub fn alpha(a: i32, b: i32) -> i32 {\n    let c = a + b;\n    c\n}\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("src/b.rs"),
            "pub fn beta(a: i32, b: i32) -> i32 {\n    let c = a * b;\n    c\n}\n",
        )
        .unwrap();

        let sub = ".ckg-test";
        let snap = GraphSettings {
            semantic_code_max_chunks: 1,
            ..GraphSettings::default()
        };
        let idx = GraphIndex::open(&dir, sub).expect("open");
        build_tree(&idx, &dir, &snap, sub).expect("rebuild");

        // `total` from `code_embedding_coverage` is epoch-independent (a plain
        // `count(*code_chunk{id})`), so any epoch string works here.
        let (_, total) = idx.code_embedding_coverage("any").expect("coverage");
        assert_eq!(
            total, 1,
            "the project-wide cap trims to the configured budget"
        );

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
        assert_eq!(
            lang_for(&PathBuf::from("a.rs"), &only_rust),
            Some(Lang::Rust)
        );
    }

    /// V12 Phase F (6c): the `graph-analyses` event only fires when the
    /// dead-exports/import-cycles counts actually changed since the last
    /// stored pass — first-ever pass (`None` stored) counts as a change, an
    /// identical repeat does not, and any different count does.
    #[test]
    fn analyses_changed_only_on_first_seen_or_different_counts() {
        assert!(
            analyses_changed(None, "3,1"),
            "first pass is always new information"
        );
        assert!(
            !analyses_changed(Some("3,1"), "3,1"),
            "identical counts: no event"
        );
        assert!(
            analyses_changed(Some("3,1"), "4,1"),
            "dead-export count grew"
        );
        assert!(
            analyses_changed(Some("3,1"), "3,0"),
            "cycle count shrank — still a change"
        );
    }

    // ── V30 Phase C: index-completion push gate ─────────────────────────────

    /// The duration half of `announce_index_complete`'s gate. Delivering a push
    /// to an idle Claude tab starts a model turn, so only builds expensive
    /// enough to have been worth waiting on may announce themselves.
    #[test]
    fn index_push_worthy_only_past_the_duration_floor() {
        let user = |ms| index_push_worthy(true, RebuildOrigin::User, ms);
        assert!(!user(0), "an instant rebuild is not news");
        assert!(
            !user(GRAPH_PUSH_MIN_BUILD_MS - 1),
            "just under the floor must stay silent"
        );
        assert!(user(GRAPH_PUSH_MIN_BUILD_MS), "the floor itself qualifies");
        assert!(
            user(GRAPH_PUSH_MIN_BUILD_MS * 10),
            "a five-minute build definitely qualifies"
        );
        assert_eq!(
            GRAPH_PUSH_MIN_BUILD_MS, 30_000,
            "the milestone fixes this floor at 30s — changing it is a spec decision"
        );
    }

    /// The ORIGIN half (review M2): an automatic rebuild never announces itself,
    /// however long it took. Four automatic paths reach `spawn_rebuild` —
    /// startup, the settings-enable watcher, watcher-overflow recovery, the
    /// schema-migration repair, and the incremental walk's `DirWalk::TooBig`
    /// escalation — so without this gate an app launch on a big repo (or a large
    /// `git checkout`) started a model turn in every channel-armed tab.
    #[test]
    fn index_push_worthy_rejects_automatic_rebuilds() {
        for ms in [0, GRAPH_PUSH_MIN_BUILD_MS, GRAPH_PUSH_MIN_BUILD_MS * 100] {
            assert!(
                !index_push_worthy(true, RebuildOrigin::Automatic, ms),
                "an automatic rebuild must never push (elapsed {ms}ms)"
            );
        }
        assert!(
            index_push_worthy(true, RebuildOrigin::User, GRAPH_PUSH_MIN_BUILD_MS),
            "…while the same build a user asked for still does"
        );
    }

    /// Review M6: "off means off" app-side. `offload.session_push` is read live
    /// at fire time and dominates every other input — the child-side capability
    /// declaration is latched until the tab restarts, so without this a
    /// toggled-off feature kept pushing into running tabs.
    #[test]
    fn index_push_worthy_honours_a_live_settings_toggle() {
        assert!(
            !index_push_worthy(false, RebuildOrigin::User, GRAPH_PUSH_MIN_BUILD_MS * 10),
            "session_push off ⇒ no push, however expensive or user-requested"
        );
    }

    // ── V17 Phase A: read-advisor verdict + snapshot LRU ────────────────────
    //
    // `should_read` itself needs an `AppHandle` (unmockable in a unit test), so
    // its verdict/re-arm/TTL/diff-threshold logic is factored into the pure
    // `read_verdict` and the snapshot store into `ReadSeenStore`; both are
    // exercised directly here. The post-compaction pass is an early return at
    // the top of `should_read` (unchanged from V11) and isn't re-tested.

    /// A `VerdictIn` with everything "neutral" — override per case.
    fn vin() -> VerdictIn {
        VerdictIn {
            seen: true,
            unchanged: false,
            ttl_expired: false,
            reminded: false,
            remind_count: 0,
            big_enough: true,
            diffs_on: true,
            have_snapshot: true,
            diff_worth_it: true,
        }
    }

    #[test]
    fn verdict_never_seen_records_and_passes() {
        let i = VerdictIn {
            seen: false,
            ..vin()
        };
        assert_eq!(read_verdict(&i), ReadAdvice::Pass { restamp: true });
    }

    #[test]
    fn verdict_unchanged_first_ask_reminds_with_outline() {
        let i = VerdictIn {
            unchanged: true,
            reminded: false,
            big_enough: true,
            ..vin()
        };
        assert_eq!(read_verdict(&i), ReadAdvice::Outline);
    }

    #[test]
    fn verdict_unchanged_small_or_reminded_passes_without_restamp() {
        // Below the min-lines floor.
        let small = VerdictIn {
            unchanged: true,
            big_enough: false,
            ..vin()
        };
        assert_eq!(read_verdict(&small), ReadAdvice::Pass { restamp: false });
        // The immediate-second-ask hatch: same file, same content, already reminded.
        let reasked = VerdictIn {
            unchanged: true,
            reminded: true,
            remind_count: 1,
            ..vin()
        };
        assert_eq!(read_verdict(&reasked), ReadAdvice::Pass { restamp: false });
    }

    #[test]
    fn verdict_unchanged_ttl_expired_restamps_and_passes() {
        let i = VerdictIn {
            unchanged: true,
            ttl_expired: true,
            ..vin()
        };
        assert_eq!(read_verdict(&i), ReadAdvice::Pass { restamp: true });
    }

    #[test]
    fn verdict_changed_with_snapshot_and_small_diff_reminds_with_diff() {
        let i = VerdictIn {
            unchanged: false,
            have_snapshot: true,
            diff_worth_it: true,
            ..vin()
        };
        assert_eq!(read_verdict(&i), ReadAdvice::Diff);
    }

    #[test]
    fn verdict_changed_but_diff_unusable_passes() {
        // Diff over 50% of the new content.
        let big = VerdictIn {
            unchanged: false,
            diff_worth_it: false,
            ..vin()
        };
        assert_eq!(read_verdict(&big), ReadAdvice::Pass { restamp: true });
        // Snapshot evicted.
        let gone = VerdictIn {
            unchanged: false,
            have_snapshot: false,
            ..vin()
        };
        assert_eq!(read_verdict(&gone), ReadAdvice::Pass { restamp: true });
        // Feature off.
        let off = VerdictIn {
            unchanged: false,
            diffs_on: false,
            ..vin()
        };
        assert_eq!(read_verdict(&off), ReadAdvice::Pass { restamp: true });
    }

    #[test]
    fn verdict_change_rearms_up_to_the_cap_then_passes() {
        // A changed re-read of an already-reminded file re-arms while under the
        // cap, then passes once at it.
        for count in 0..READ_REMIND_CAP {
            let i = VerdictIn {
                unchanged: false,
                reminded: true,
                remind_count: count,
                ..vin()
            };
            assert_eq!(
                read_verdict(&i),
                ReadAdvice::Diff,
                "count {count} still re-arms"
            );
        }
        let at_cap = VerdictIn {
            unchanged: false,
            reminded: true,
            remind_count: READ_REMIND_CAP,
            ..vin()
        };
        assert_eq!(
            read_verdict(&at_cap),
            ReadAdvice::Pass { restamp: true },
            "at cap ⇒ pass"
        );
    }

    /// V17 Phase B5: the bypass tap's skip-guard. A provable whole-file shell
    /// read is intercepted by the Bash hook (remind already recorded, or
    /// verdict-passed) so `check_bypass` must NOT also score it — but only when
    /// the shell sub-toggle is on, and only for a command the strict parser
    /// actually accepts. Residual escape routes (`sed -n`, `head`) still score.
    #[test]
    fn intercepted_whole_file_read_guards_only_provable_reads() {
        // Sub-toggle on + a provable whole-file read ⇒ skipped (intercepted).
        assert!(intercepted_whole_file_read(true, "cat src/a.rs"));
        assert!(intercepted_whole_file_read(true, "Get-Content \"a b.txt\""));
        // Sub-toggle OFF ⇒ never skipped (the Bash hook isn't installed, so the
        // command really is an un-intercepted read the canary should score).
        assert!(!intercepted_whole_file_read(false, "cat src/a.rs"));
        // Residual escape routes are not provable whole-file reads ⇒ still scored.
        assert!(!intercepted_whole_file_read(true, "sed -n 5,10p f"));
        assert!(!intercepted_whole_file_read(true, "head -50 f"));
        assert!(!intercepted_whole_file_read(true, "cat a | grep x"));
    }

    #[test]
    fn capture_snapshot_respects_min_lines_and_entry_cap() {
        // Below min-lines ⇒ no snapshot.
        assert!(capture_snapshot("a\nb\n", 10).is_none());
        // At/above min-lines and under the byte cap ⇒ snapshot kept.
        let content: String = "line\n".repeat(20);
        assert!(capture_snapshot(&content, 10).is_some());
        // Over the per-entry byte cap ⇒ no snapshot even with enough lines.
        let huge = "x\n".repeat(SNAP_ENTRY_MAX); // ~2·SNAP_ENTRY_MAX bytes
        assert!(capture_snapshot(&huge, 1).is_none());
    }

    #[test]
    fn read_seen_lru_bounds_snapshot_bytes_and_keeps_the_observation() {
        let mut store = ReadSeenStore::default();
        // ~1 MiB per snapshot; 20 of them (~20 MiB) overruns SNAP_TOTAL_MAX (16 MiB).
        let blob: Arc<str> = Arc::from("y".repeat(1024 * 1024));
        let n = 20u64;
        for k in 0..n {
            let key = ("s".to_string(), format!("f{k}.rs"));
            store.insert(key, format!("h{k}"), k as u32, Some(blob.clone()), k);
        }
        let seen = &store.map;
        // All observations survive (nothing forgot the hash/turn); only content evicted.
        assert_eq!(seen.len() as u64, n, "every observation is retained");
        assert!(
            snapshot_bytes(seen) <= SNAP_TOTAL_MAX,
            "snapshot bytes held under budget: {}",
            snapshot_bytes(seen)
        );
        // Running total matches the O(n) ground truth.
        assert_eq!(
            store.snap_bytes,
            snapshot_bytes(seen),
            "running total tracks snapshot_bytes"
        );
        // The oldest-touched entry lost its snapshot but kept its hash/turn.
        let oldest = seen
            .get(&("s".to_string(), "f0.rs".to_string()))
            .expect("oldest present");
        assert!(oldest.snapshot.is_none(), "oldest snapshot evicted");
        assert_eq!(oldest.hash, "h0", "evicted entry keeps its hash");
        assert_eq!(oldest.turn, 0, "evicted entry keeps its turn");
        // The newest still has its snapshot.
        let newest = seen
            .get(&("s".to_string(), format!("f{}.rs", n - 1)))
            .unwrap();
        assert!(newest.snapshot.is_some(), "newest snapshot retained");
    }

    #[test]
    fn read_seen_entry_backstop_bounds_row_count() {
        let mut store = ReadSeenStore::default();
        // Snapshot-less rows: only the entry backstop bounds these.
        for k in 0..(READ_SEEN_MAX_ENTRIES as u64 + 50) {
            let key = ("s".to_string(), format!("f{k}.rs"));
            store.insert(key, format!("h{k}"), k as u32, None, k);
        }
        let seen = &store.map;
        assert!(
            seen.len() <= READ_SEEN_MAX_ENTRIES,
            "row count bounded by the backstop: {}",
            seen.len()
        );
        // The most-recent key survives; the oldest was evicted.
        let last = READ_SEEN_MAX_ENTRIES as u64 + 49;
        assert!(seen.contains_key(&("s".to_string(), format!("f{last}.rs"))));
        assert!(!seen.contains_key(&("s".to_string(), "f0.rs".to_string())));
    }

    #[test]
    fn read_seen_running_total_matches_ground_truth_across_all_mutations() {
        // Drives the store through insert / replace / entry-cap eviction /
        // byte-budget eviction / session clear / whole clear and asserts the
        // incrementally-maintained `snap_bytes` equals the O(n) `snapshot_bytes`
        // ground truth at every step (V22: the running total must never drift).
        let mut store = ReadSeenStore::default();
        let mut touch = 0u64;
        let mut bump = || {
            let t = touch;
            touch += 1;
            t
        };
        let check = |store: &ReadSeenStore| {
            assert_eq!(
                store.snap_bytes,
                snapshot_bytes(&store.map),
                "running total drifted from snapshot_bytes"
            );
        };

        // Small (no snapshot) and large (snapshot) inserts across two sessions.
        let small: Arc<str> = Arc::from("x".repeat(64));
        let big: Arc<str> = Arc::from("y".repeat(2 * 1024 * 1024)); // 2 MiB each
        for k in 0..8u64 {
            let sid = if k % 2 == 0 { "a" } else { "b" };
            let snap = if k % 3 == 0 {
                Some(big.clone())
            } else {
                Some(small.clone())
            };
            store.insert(
                (sid.to_string(), format!("f{k}.rs")),
                format!("h{k}"),
                k as u32,
                snap,
                bump(),
            );
            check(&store);
        }
        assert!(store.snap_bytes > 0, "snapshots were recorded");

        // Replace an existing key: with a bigger snapshot, then with none.
        store.insert(
            ("a".to_string(), "f0.rs".to_string()),
            "h0b".into(),
            99,
            Some(big.clone()),
            bump(),
        );
        check(&store);
        store.insert(
            ("a".to_string(), "f0.rs".to_string()),
            "h0c".into(),
            100,
            None,
            bump(),
        );
        check(&store);

        // Force the byte-budget eviction path: pile on enough 2 MiB snapshots to
        // cross SNAP_TOTAL_MAX (16 MiB).
        for k in 100..120u64 {
            store.insert(
                ("c".to_string(), format!("f{k}.rs")),
                format!("h{k}"),
                k as u32,
                Some(big.clone()),
                bump(),
            );
            check(&store);
        }
        assert!(store.snap_bytes <= SNAP_TOTAL_MAX, "byte budget enforced");

        // Force the entry-cap eviction path: cross READ_SEEN_MAX_ENTRIES rows.
        for k in 0..(READ_SEEN_MAX_ENTRIES as u64 + 20) {
            store.insert(
                ("d".to_string(), format!("g{k}.rs")),
                format!("h{k}"),
                k as u32,
                None,
                bump(),
            );
        }
        check(&store);
        assert!(
            store.map.len() <= READ_SEEN_MAX_ENTRIES,
            "entry cap enforced"
        );

        // Session clear (drops one session's rows, some snapshotted).
        store.clear_session(Some("c"));
        check(&store);
        assert!(
            !store.map.keys().any(|(sid, _)| sid == "c"),
            "session c cleared"
        );

        // Whole clear.
        store.clear_session(None);
        check(&store);
        assert_eq!(store.snap_bytes, 0, "whole clear zeroes the running total");
        assert!(store.map.is_empty(), "whole clear empties the map");
    }

    // ── V17 Phase C: first-read tier eligibility (pure gate) ──────────────
    //
    // The digest lookup + enqueue + remind wiring in `should_read` needs an
    // `AppHandle` (unmockable), so — like Phase A's `read_verdict` — the tier's
    // GATE is factored into the pure `first_read_eligible` and exercised here;
    // the reminder TEXT is covered by `context::tests::first_read_advice_*`.

    /// A `FirstReadIn` that qualifies (300 KiB non-code whole-file read, tier at
    /// 256 KiB) — override one field per case.
    fn fin() -> FirstReadIn {
        FirstReadIn {
            first_read_kb: 256,
            content_len: 300 * 1024,
            slice: false,
            is_code: false,
        }
    }

    #[test]
    fn first_read_qualifying_is_eligible() {
        assert!(first_read_eligible(&fin()));
    }

    #[test]
    fn first_read_disabled_short_circuits() {
        // kb == 0 ⇒ tier off, regardless of everything else.
        assert!(!first_read_eligible(&FirstReadIn {
            first_read_kb: 0,
            ..fin()
        }));
    }

    #[test]
    fn first_read_under_threshold_passes() {
        // 200 KiB content vs a 256 KiB floor.
        assert!(!first_read_eligible(&FirstReadIn {
            content_len: 200 * 1024,
            ..fin()
        }));
        // Exactly at the threshold qualifies (>=).
        assert!(first_read_eligible(&FirstReadIn {
            content_len: 256 * 1024,
            ..fin()
        }));
    }

    #[test]
    fn first_read_code_file_passes() {
        assert!(!first_read_eligible(&FirstReadIn {
            is_code: true,
            ..fin()
        }));
    }

    #[test]
    fn first_read_slice_passes() {
        // offset OR limit present ⇒ deliberate slice ⇒ never substituted.
        assert!(!first_read_eligible(&FirstReadIn {
            slice: true,
            ..fin()
        }));
    }

    // ── V24 Phase B: live-session registry → active_session_ids ─────────────

    /// A minimal session row carrying just the id + `last_ms` the active-set
    /// logic reads (the rest is irrelevant to the decision).
    fn urow(id: &str, last_ms: i64) -> SessionUsageRow {
        SessionUsageRow {
            session_id: id.to_string(),
            agent: "claude".to_string(),
            totals: UsageTotals::default(),
            tool_chars: 0,
            cache_hit_ratio: 0.0,
            est_only: false,
            started_ms: 0,
            last_ms,
            models: Vec::new(),
        }
    }

    fn live(session_id: &str, last_seen_ms: i64) -> LiveSession {
        LiveSession {
            agent: "claude".to_string(),
            session_id: session_id.to_string(),
            last_seen_ms,
        }
    }

    #[test]
    fn active_session_ids_unions_registry_and_recency_and_dedups() {
        let now = 10_000_000i64;
        let sessions = vec![
            urow("recent", now - 1_000), // recency-fresh
            urow("idle-but-open", now - LIVE_SESSION_RECENCY_MS - 60_000), // stale activity
            urow("stale", now - LIVE_SESSION_RECENCY_MS - 60_000), // stale, no live entry
        ];
        let mut reg = HashMap::new();
        // A still-ticking tab whose last activity fell out of the recency window
        // — the registry keeps it active (the point of the union).
        reg.insert("tabA".to_string(), live("idle-but-open", now - 1_000));
        // An expired registry entry does NOT keep its session active.
        reg.insert(
            "tabB".to_string(),
            live("stale", now - LIVE_SESSION_TTL_MS - 1_000),
        );
        // A fresh entry whose session isn't in THIS root's list is ignored
        // (the registry is process-wide; the output is root-scoped).
        reg.insert("tabC".to_string(), live("other-project", now));
        // "recent" is BOTH recency-fresh and registry-fresh → appears once.
        reg.insert("tabD".to_string(), live("recent", now));

        let active = compute_active_session_ids(&reg, &sessions, now);
        assert_eq!(
            active,
            vec!["idle-but-open".to_string(), "recent".to_string()],
            "sorted, deduped, TTL-gated and root-scoped"
        );
    }

    #[test]
    fn active_session_ids_registry_ttl_boundary() {
        let now = 10_000_000i64;
        // A single session with stale activity, so only the registry can mark it.
        let sessions = vec![urow("s", now - LIVE_SESSION_RECENCY_MS - 1)];
        // Exactly at the TTL edge is still live...
        let mut at_edge = HashMap::new();
        at_edge.insert("t".to_string(), live("s", now - LIVE_SESSION_TTL_MS));
        assert_eq!(
            compute_active_session_ids(&at_edge, &sessions, now),
            vec!["s".to_string()]
        );
        // ...one ms past it has expired.
        let mut past = HashMap::new();
        past.insert("t".to_string(), live("s", now - LIVE_SESSION_TTL_MS - 1));
        assert!(compute_active_session_ids(&past, &sessions, now).is_empty());
    }

    // ── V28 (issue #13): tab → session resolution ─────────────────────────

    /// A registry entry for an arbitrary agent (the `live` helper pins Claude).
    fn live_for(agent: &str, session_id: &str, last_seen_ms: i64) -> LiveSession {
        LiveSession {
            agent: agent.to_string(),
            session_id: session_id.to_string(),
            last_seen_ms,
        }
    }

    fn v28_registry(now: i64) -> HashMap<String, LiveSession> {
        let mut reg = HashMap::new();
        reg.insert(
            "claude".to_string(),
            live_for("claude", "ses_a", now - 1_000),
        );
        reg.insert(
            "claude-local".to_string(),
            live_for("claude", "ses_b", now - 1_000),
        );
        reg.insert(
            "opencode".to_string(),
            live_for("opencode", "ses_oc", now - 1_000),
        );
        reg.insert(
            "claude-stale".to_string(),
            live_for("claude", "ses_old", now - LIVE_SESSION_TTL_MS - 1),
        );
        reg
    }

    /// No running-tab roots registered: every V28 lookup behaves exactly as it
    /// did before the H1 fix (the pre-existing tests all use this).
    fn no_roots() -> HashMap<String, LiveTabRoot> {
        HashMap::new()
    }

    #[test]
    fn live_session_for_tab_returns_that_tabs_own_session() {
        // The whole point of V28: two tabs of the SAME agent resolve to their
        // OWN sessions, not to whichever was most recently active.
        let now = 10_000_000i64;
        let reg = v28_registry(now);
        assert_eq!(
            lookup_live_session_for_tab(&reg, &no_roots(), "claude", "claude", now),
            Some("ses_a".to_string())
        );
        assert_eq!(
            lookup_live_session_for_tab(&reg, &no_roots(), "claude-local", "claude", now),
            Some("ses_b".to_string())
        );
        assert_eq!(
            lookup_live_session_for_tab(&reg, &no_roots(), "opencode", "opencode", now),
            Some("ses_oc".to_string())
        );
    }

    #[test]
    fn live_session_for_tab_rejects_an_agent_mismatch() {
        // The key exists but was stamped by the other harness — resolving it
        // would hand a Claude call an OpenCode session. Fail open instead.
        let now = 10_000_000i64;
        let reg = v28_registry(now);
        assert_eq!(
            lookup_live_session_for_tab(&reg, &no_roots(), "opencode", "claude", now),
            None
        );
        assert_eq!(
            lookup_live_session_for_tab(&reg, &no_roots(), "claude", "opencode", now),
            None
        );
    }

    #[test]
    fn live_session_for_tab_rejects_a_ttl_stale_entry() {
        let now = 10_000_000i64;
        let reg = v28_registry(now);
        assert_eq!(
            lookup_live_session_for_tab(&reg, &no_roots(), "claude-stale", "claude", now),
            None,
            "past the TTL the tab's reported session is no longer proof"
        );
        // Exactly at the TTL edge still counts (same boundary as the registry
        // half of `compute_active_session_ids` and the eviction sweep).
        let mut edge = HashMap::new();
        edge.insert(
            "claude".to_string(),
            live_for("claude", "ses_edge", now - LIVE_SESSION_TTL_MS),
        );
        assert_eq!(
            lookup_live_session_for_tab(&edge, &no_roots(), "claude", "claude", now),
            Some("ses_edge".to_string())
        );
    }

    #[test]
    fn live_session_for_tab_never_guesses_on_an_unknown_key() {
        // No prefix/fuzzy matching, no "only one Claude entry, must be it".
        let now = 10_000_000i64;
        let reg = v28_registry(now);
        for tab in ["", "claude2", "clau", "claude ", "CLAUDE"] {
            assert_eq!(
                lookup_live_session_for_tab(&reg, &no_roots(), tab, "claude", now),
                None,
                "tab key {tab:?} must not resolve"
            );
        }
        assert!(
            lookup_live_session_for_tab(&HashMap::new(), &no_roots(), "claude", "claude", now)
                .is_none()
        );
    }

    // ── H1 (2026-08-05 review): same-root ambiguity degrades to unscoped ──

    fn root_at(agent: &str, root: &str, last_seen_ms: i64) -> LiveTabRoot {
        LiveTabRoot {
            agent: agent.to_string(),
            root: PathBuf::from(root),
            last_seen_ms,
        }
    }

    /// `n` running Claude tabs, all tailing the SAME transcript root.
    fn roots_sharing(tabs: &[&str], now: i64) -> HashMap<String, LiveTabRoot> {
        tabs.iter()
            .map(|t| {
                (
                    (*t).to_string(),
                    root_at("claude", "/home/u/.claude/projects/P--proj", now - 100),
                )
            })
            .collect()
    }

    #[test]
    fn ambiguity_predicate_counts_running_tabs_sharing_a_root() {
        let now = 10_000_000i64;
        // 0 running tabs registered → nothing to conflate.
        assert!(!tab_binding_is_ambiguous(&no_roots(), "claude", "claude", now));
        // 1 running tab on the root → the common case, NOT ambiguous.
        let one = roots_sharing(&["claude"], now);
        assert!(!tab_binding_is_ambiguous(&one, "claude", "claude", now));
        // 2 running tabs on the SAME root → both are ambiguous.
        let two = roots_sharing(&["claude", "claude-local"], now);
        assert!(tab_binding_is_ambiguous(&two, "claude", "claude", now));
        assert!(tab_binding_is_ambiguous(&two, "claude-local", "claude", now));
        // 2 running tabs on DIFFERENT roots → each keeps its own identity.
        let mut split = HashMap::new();
        split.insert(
            "claude".to_string(),
            root_at("claude", "/home/u/.claude/projects/P--one", now),
        );
        split.insert(
            "claude-local".to_string(),
            root_at("claude", "/home/u/.claude/projects/P--two", now),
        );
        assert!(!tab_binding_is_ambiguous(&split, "claude", "claude", now));
        assert!(!tab_binding_is_ambiguous(
            &split,
            "claude-local",
            "claude",
            now
        ));
    }

    #[test]
    fn ambiguity_predicate_ignores_other_agents_and_stale_co_tenants() {
        let now = 10_000_000i64;
        let shared = "/home/u/.claude/projects/P--proj";
        let mut reg = HashMap::new();
        reg.insert("claude".to_string(), root_at("claude", shared, now));
        // A different agent on the same root is not a co-tenant: OpenCode binds
        // per-tab off its own stream (and never registers here anyway).
        reg.insert("opencode".to_string(), root_at("opencode", shared, now));
        assert!(!tab_binding_is_ambiguous(&reg, "claude", "claude", now));
        // A CLOSED tab whose entry outlived the TTL is not a co-tenant either —
        // otherwise a leaked entry would disable scoping forever.
        reg.insert(
            "claude-local".to_string(),
            root_at("claude", shared, now - LIVE_SESSION_TTL_MS - 1),
        );
        assert!(!tab_binding_is_ambiguous(&reg, "claude", "claude", now));
        // Refresh it (the tab is running again) and ambiguity returns.
        reg.insert("claude-local".to_string(), root_at("claude", shared, now));
        assert!(tab_binding_is_ambiguous(&reg, "claude", "claude", now));
        // A tab with no root entry at all is never degraded (OpenCode's path).
        assert!(!tab_binding_is_ambiguous(&reg, "opencode-2", "opencode", now));
    }

    #[test]
    fn live_session_for_tab_is_unscoped_under_same_root_ambiguity() {
        // The H1 case: two Claude tabs on one project. The registry answers
        // "whichever session wrote last" for BOTH keys, so honoring it would
        // put tab A's memory notes in tab B's scope. Fail open to unscoped.
        let now = 10_000_000i64;
        let reg = v28_registry(now);
        let two = roots_sharing(&["claude", "claude-local"], now);
        assert_eq!(
            lookup_live_session_for_tab(&reg, &two, "claude", "claude", now),
            None
        );
        assert_eq!(
            lookup_live_session_for_tab(&reg, &two, "claude-local", "claude", now),
            None
        );
        // The single-running-tab case is untouched.
        let one = roots_sharing(&["claude"], now);
        assert_eq!(
            lookup_live_session_for_tab(&reg, &one, "claude", "claude", now),
            Some("ses_a".to_string())
        );
        // ...and an OpenCode tab never registers a root, so it never degrades.
        assert_eq!(
            lookup_live_session_for_tab(&reg, &two, "opencode", "opencode", now),
            Some("ses_oc".to_string())
        );
    }

    #[test]
    fn live_claude_tab_sessions_drops_ambiguous_tabs_only() {
        let now = 10_000_000i64;
        let reg = v28_registry(now);
        // Single running tab per root: the permission resolver still gets the
        // mapping it needs (TTL-stale entries filtered as before).
        let one = roots_sharing(&["claude"], now);
        let mut got = live_claude_tab_sessions(&reg, &one, now);
        got.sort();
        assert_eq!(
            got,
            vec![
                ("claude".to_string(), "ses_a".to_string()),
                ("claude-local".to_string(), "ses_b".to_string()),
            ],
            "only `claude` is registered as running, so nothing is ambiguous"
        );
        // Both tabs running on one root — including the launch-order window in
        // which A's tap already rotated onto B's fresh session and marked it
        // live UNIQUELY: no pair survives, so the resolver has nothing to
        // attribute with and refuses.
        let two = roots_sharing(&["claude", "claude-local"], now);
        assert!(live_claude_tab_sessions(&reg, &two, now).is_empty());
        let mut window = HashMap::new();
        window.insert(
            "claude".to_string(),
            live_for("claude", "ses_b", now - 10), // A's tap, B's session
        );
        assert!(live_claude_tab_sessions(&window, &two, now).is_empty());
        // Two tabs on DIFFERENT roots keep their pairs.
        let mut split = HashMap::new();
        split.insert(
            "claude".to_string(),
            root_at("claude", "/home/u/.claude/projects/P--one", now),
        );
        split.insert(
            "claude-local".to_string(),
            root_at("claude", "/home/u/.claude/projects/P--two", now),
        );
        assert_eq!(live_claude_tab_sessions(&reg, &split, now).len(), 2);
    }

    /// H1-R5: the root is a normalized comparison key, so two tabs whose cwds
    /// were typed with different separators/trailing slashes (and, on Windows,
    /// different case) are still recognized as co-tenants. Before the fix these
    /// produced different `PathBuf` keys and the predicate silently answered
    /// "not ambiguous" — i.e. confident-wrong scoping for both tabs.
    #[test]
    fn tab_root_keys_are_normalized_at_the_mark_site() {
        let now = 10_000_000i64;
        let mut reg = HashMap::new();
        upsert_live_tab_root(
            &mut reg,
            "claude",
            "claude",
            Path::new(r"C:\Users\u\.claude\projects\P--proj"),
            now,
        );
        upsert_live_tab_root(
            &mut reg,
            "claude-local",
            "claude",
            Path::new("C:/Users/u/.claude/projects/P--proj/"),
            now,
        );
        assert!(
            tab_binding_is_ambiguous(&reg, "claude", "claude", now),
            "separator/trailing-slash variants of one dir must conflate"
        );
        assert!(tab_binding_is_ambiguous(&reg, "claude-local", "claude", now));
        // Windows paths are case-insensitive, so a case variant is the SAME dir.
        if cfg!(windows) {
            let mut cased = HashMap::new();
            upsert_live_tab_root(
                &mut cased,
                "claude",
                "claude",
                Path::new(r"C:\Users\u\.claude\projects\P--Proj"),
                now,
            );
            upsert_live_tab_root(
                &mut cased,
                "claude-local",
                "claude",
                Path::new(r"c:\users\u\.claude\projects\p--proj"),
                now,
            );
            assert!(
                tab_binding_is_ambiguous(&cased, "claude", "claude", now),
                "case variants of one Windows dir must conflate"
            );
        }
        // Genuinely different dirs still don't conflate.
        let mut split = HashMap::new();
        upsert_live_tab_root(&mut split, "claude", "claude", Path::new("/u/p/one"), now);
        upsert_live_tab_root(&mut split, "claude-local", "claude", Path::new("/u/p/two"), now);
        assert!(!tab_binding_is_ambiguous(&split, "claude", "claude", now));
    }

    /// H1-R2: the property the tap's heartbeat depends on — a refresh restores a
    /// claim that had aged past the TTL, so a tab stalled inside TTS
    /// backpressure keeps counting as a co-tenant instead of letting its sibling
    /// become "unique" (and confidently bind to the stalled tab's transcript).
    #[test]
    fn refreshing_a_tab_root_restores_a_ttl_stale_claim() {
        let now = 10_000_000i64;
        let shared = Path::new("/home/u/.claude/projects/P--proj");
        let mut reg = HashMap::new();
        // Tab A marked long ago (its drain loop is parked in `speak`), tab B
        // ticking normally.
        upsert_live_tab_root(
            &mut reg,
            "claude",
            "claude",
            shared,
            now - LIVE_SESSION_TTL_MS - 1,
        );
        upsert_live_tab_root(&mut reg, "claude-local", "claude", shared, now);
        // The starvation symptom: A aged out, so B looks unique and would get a
        // confident (wrong) binding.
        assert!(!reg.contains_key("claude"), "stale claim is evicted");
        assert!(!tab_binding_is_ambiguous(&reg, "claude-local", "claude", now));
        // A heartbeat tick re-marks A — independent of A's drain loop — and the
        // co-tenancy is visible again for BOTH tabs.
        upsert_live_tab_root(&mut reg, "claude", "claude", shared, now);
        assert!(tab_binding_is_ambiguous(&reg, "claude", "claude", now));
        assert!(tab_binding_is_ambiguous(&reg, "claude-local", "claude", now));
    }

    #[test]
    fn evict_stale_live_sessions_drops_only_ttl_stale_entries() {
        // V24 code-review: the opportunistic eviction `mark_live_session` runs
        // keeps entries within the registry TTL (which the registry half still
        // uses) and drops only those past it, so OpenCode keys can't accumulate.
        let now = 10_000_000i64;
        let mut reg = HashMap::new();
        reg.insert("fresh".to_string(), live("s_fresh", now - 1_000));
        reg.insert(
            "edge".to_string(),
            live("s_edge", now - LIVE_SESSION_TTL_MS),
        );
        reg.insert(
            "stale".to_string(),
            live("s_stale", now - LIVE_SESSION_TTL_MS - 1),
        );
        evict_stale_live_sessions(&mut reg, now);
        assert!(reg.contains_key("fresh"), "within TTL kept");
        assert!(reg.contains_key("edge"), "exactly at TTL kept");
        assert!(!reg.contains_key("stale"), "past TTL evicted");
        assert_eq!(reg.len(), 2);
    }
}
