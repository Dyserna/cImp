//! V23 Phase B — the concurrent **audit runner** and its managed state.
//!
//! One scan spawns every enabled + resolvable tool concurrently against the
//! project root (`launch_cwd`); results stream per tool as each finishes (no
//! barrier — gitleaks returns in seconds, semgrep can take minutes). A single
//! scan is in flight at a time; a re-trigger while one runs is rejected. Cancel
//! kills the children (mirrors the offload/pty [`CancellationToken`] precedent);
//! on Windows the whole process *tree* is killed (`taskkill /T`) — scanners like
//! semgrep fork workers that would otherwise survive and hold the stdio pipes
//! open. A per-tool wall-clock timeout (`timeout_secs`) fails just that tool.
//!
//! State lives only in [`AuditState`] (managed) — not persisted across restarts
//! (spec). Every transition emits [`AUDIT_STATUS_EVENT`] with a snapshot; the
//! event payload caps each tool's findings ([`EVENT_FINDINGS_PER_TOOL_CAP`]) so
//! a pathological repo can't bloat the wire, and the frontend fetches the full
//! set via `audit_snapshot`.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, Instant};

use tauri::{AppHandle, Emitter};
use tokio_util::sync::CancellationToken;

use crate::activity::{self, now_ms, ActivityEntry, ActivityKind, ActivityRecord};
use crate::checks::{parsers, Diag};
use crate::offload::service::PushNotice;
use crate::settings::{AuditToolConfig, AuditToolId, SettingsHandle};

use super::adapters::{self, Adapter, Category, ExitClass, Transport};
use super::census;

/// Tauri event emitted on every per-tool transition, carrying a (findings-
/// capped) [`AuditSnapshot`]. Phase C subscribes to this.
pub const AUDIT_STATUS_EVENT: &str = "audit-status";

/// Per-tool captured-output cap. SARIF for a large scan is sizable but bounded;
/// 16 MiB is generous headroom without letting a runaway tool exhaust memory.
const MAX_OUTPUT_BYTES: usize = 16 * 1024 * 1024;

/// The event payload caps each tool's findings at this many; past it, the
/// event sets [`AuditSnapshot::truncated`] and the frontend pulls the full set
/// via `audit_snapshot`. The full snapshot IPC is never capped.
const EVENT_FINDINGS_PER_TOOL_CAP: usize = 500;

/// One tool's lifecycle within a scan. Serialized kebab-case, so the wire
/// strings are exactly `idle | running | done | failed | not-installed |
/// path-invalid | skipped-not-applicable`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ToolStatus {
    /// Configured but not part of / not yet started in the current scan.
    Idle,
    /// Resolved and its child is running.
    Running,
    /// Ran to completion (exit 0 = clean, or a findings exit code) — `findings`
    /// is authoritative even when empty.
    Done,
    /// A tool error: non-findings exit code, spawn failure, timeout, or cancel.
    Failed,
    /// The binary could not be resolved with NO path configured (not in ebin,
    /// not on PATH) — the scan proceeds with the remaining tools.
    NotInstalled,
    /// The binary could not be resolved at the path the user CONFIGURED —
    /// distinct from [`NotInstalled`](Self::NotInstalled) so the UI/report can
    /// say "your configured path is wrong" instead of the misleading "not
    /// installed" (a stale per-project path or a path from another machine,
    /// e.g. an overlay copied between projects).
    PathInvalid,
    /// V25 Phase C: the tool is enabled but does not apply to this project's
    /// [`census`](super::census) (no PMD in a Rust repo) — never launched.
    /// Distinct from [`Idle`](Self::Idle) (disabled) and
    /// [`NotInstalled`](Self::NotInstalled) (binary missing) so the UI can hide
    /// it in the tab while Settings still explains why.
    SkippedNotApplicable,
}

/// One raw audit finding: a [`Diag`] (from the SARIF parser, project-relative
/// path) tagged with the tool that produced it. `Diag.code` carries the SARIF
/// rule id; `Diag.severity` the level.
#[derive(Clone, Debug, serde::Serialize)]
pub struct AuditFinding {
    /// Serializes to the tool's kebab wire id (`osv-scanner` | `gitleaks` |
    /// `semgrep`).
    pub tool: AuditToolId,
    pub diag: Diag,
}

/// One tool's live state within the current (or last) scan.
#[derive(Clone, Debug, serde::Serialize)]
pub struct ToolState {
    /// Kebab wire id.
    pub id: AuditToolId,
    /// V25 Phase C: the tool's [`Category`] (`"security"` / `"quality"`). A scan
    /// runs one category, so every `ToolState` in a snapshot shares it; the field
    /// lets the split UI filter the shared snapshot to its own tab's tools.
    pub category: Category,
    pub status: ToolStatus,
    pub findings: Vec<AuditFinding>,
    pub duration_ms: u64,
    /// Error detail when `status == failed` / `not-installed`; `null` otherwise.
    pub error: Option<String>,
    /// The resolved binary path (once resolution succeeds); serializes as a
    /// string. `null` before resolution / when not installed.
    pub resolved: Option<PathBuf>,
    /// Lockfiles / manifests the tool reported *scanning* (SARIF
    /// `runs[].artifacts`), project-relative. Populated for **osv-scanner only**
    /// (a second best-effort pass over the raw SARIF — see
    /// [`parse_scanned_artifacts`]); empty for the other tools, an older
    /// osv-scanner that omits `artifacts`, or a killed child. The tab's
    /// scan-coverage line reads this so a "0 findings" run from an unscannable
    /// ecosystem doesn't read as a clean bill of health.
    pub scanned_artifacts: Vec<String>,
}

impl ToolState {
    /// An enabled + applicable tool about to be resolved and run: `running`.
    fn fresh(id: AuditToolId, category: Category) -> Self {
        Self {
            id,
            category,
            status: ToolStatus::Running,
            findings: Vec::new(),
            duration_ms: 0,
            error: None,
            resolved: None,
            scanned_artifacts: Vec::new(),
        }
    }

    /// A configured-but-disabled tool: shown as an `idle` chip, never scanned.
    fn idle(id: AuditToolId, category: Category) -> Self {
        Self {
            status: ToolStatus::Idle,
            ..Self::fresh(id, category)
        }
    }

    /// V25 Phase C: an enabled tool that doesn't apply to this project's census —
    /// reported `skipped-not-applicable`, never launched.
    fn skipped_not_applicable(id: AuditToolId, category: Category) -> Self {
        Self {
            status: ToolStatus::SkippedNotApplicable,
            ..Self::fresh(id, category)
        }
    }
}

/// V25 Phase C: the language census of the scanned root, serialized into every
/// snapshot so the split UI can gate chips (hide a tool the project doesn't
/// apply to) without a second IPC. Empty (both lists) before the first scan of
/// this runner — the last scan's census is retained afterward.
#[derive(Clone, Debug, Default, serde::Serialize)]
pub struct CensusBlock {
    /// Lowercase, dot-less file extensions seen (sorted).
    pub extensions: Vec<String>,
    /// [`census::MARKERS`] tokens seen (sorted).
    pub markers: Vec<String>,
}

/// The whole runner snapshot — the `audit-status` event payload and the
/// `audit_snapshot` return. Identical shape in both; the event's `tools`
/// findings are capped and `truncated` may be set, the IPC's never are.
#[derive(Clone, Debug, serde::Serialize)]
pub struct AuditSnapshot {
    /// Absolute project root (display string).
    pub root: String,
    /// Whether a scan is in flight right now.
    pub scanning: bool,
    /// Epoch millis when the last scan started; `null` before the first scan.
    pub last_scan_at: Option<u64>,
    /// Per-tool state, in configured order. Contains only the tools of the last
    /// scanned [`Category`] (a scan runs one category); empty before the first
    /// scan. The split UI filters by [`ToolState::category`] and renders from
    /// settings until its own category's tools appear here.
    pub tools: Vec<ToolState>,
    /// V25 Phase C: the scanned root's language census — drives chip visibility.
    pub census: CensusBlock,
    /// True total findings across all tools, BEFORE any wire cap — so the UI
    /// can render "showing N of M".
    pub total_findings: usize,
    /// Set when this payload dropped findings to the per-tool cap (event only).
    /// The frontend should fetch the full set via `audit_snapshot`.
    pub truncated: bool,
}

struct Inner {
    root: PathBuf,
    scanning: bool,
    last_scan_at: Option<u64>,
    tools: Vec<ToolState>,
    /// V25 Phase C: the census taken at the last scan start — serialized into
    /// every snapshot so the UI can gate chips. Default (empty) before any scan.
    census: CensusBlock,
    /// The current scan's cancel token (present iff `scanning`).
    cancel: Option<CancellationToken>,
}

impl Inner {
    /// Build a snapshot. `cap = Some(n)` caps each tool's findings at `n` (the
    /// event path); `None` returns the full set (the `audit_snapshot` IPC).
    fn snapshot(&self, cap: Option<usize>) -> AuditSnapshot {
        let total_findings = self.tools.iter().map(|t| t.findings.len()).sum();
        let mut truncated = false;
        let tools = self
            .tools
            .iter()
            .map(|t| {
                let mut ts = t.clone();
                if let Some(n) = cap {
                    if ts.findings.len() > n {
                        ts.findings.truncate(n);
                        truncated = true;
                    }
                }
                ts
            })
            .collect();
        AuditSnapshot {
            root: self.root.display().to_string(),
            scanning: self.scanning,
            last_scan_at: self.last_scan_at,
            tools,
            census: self.census.clone(),
            total_findings,
            truncated,
        }
    }
}

/// V30 Phase C: who asked for this scan. The completion push exists for the
/// human who clicked Scan and then went back to a Claude tab — an
/// agent-triggered run is already returning its full report through the open
/// `tools/call` (the loopback `/audit/run` route, or the offload worker's native
/// audit tool), and pushing there would duplicate it into the same session.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Initiator {
    /// The Code audit tab's Scan button (`audit_start_scan` →
    /// [`AuditState::start_scan`]). Nothing is awaiting a return value.
    Gui,
    /// An agent, via [`AuditState::run_scan_and_wait`] — i.e. the V26 MCP
    /// surface (`security_audit` / `quality_audit`), whether it arrived over the
    /// loopback `/audit/run` route from a stdio child or from the offload
    /// worker's native tool. The caller holds a call open and gets the report as
    /// its result.
    Agent,
}

/// V30 Phase C: whether a completed scan of this [`Initiator`] announces itself
/// on the session-push bus. Pure so the gate is unit-testable, and separate from
/// the send so the *decision* is one named place: an agent-initiated run must
/// never push, because its result is already being returned synchronously (and
/// Claude Code's native auto-backgrounding delivers even a long one in full —
/// V30 Phase 0 T4).
fn initiator_pushes(initiator: Initiator) -> bool {
    matches!(initiator, Initiator::Gui)
}

/// V30 (review LOW): the wall-clock floor for the scan-completion push, the
/// twin of the graph producer's `GRAPH_PUSH_MIN_BUILD_MS`. A 200 ms scan is not
/// news, and a notice delivered to an idle Claude tab **starts a model turn**.
const AUDIT_PUSH_MIN_SCAN_MS: u64 = 30_000;

/// V30: the complete decision for "does this finished scan announce itself?" —
/// pure, so every arm is testable without a runner, an `AppHandle`, or a bus.
///
/// - **Settings** (review M6): `offload.session_push`, passed in from a LIVE
///   read at fire time. The child-side gate is latched per tab until restart,
///   so this is the half that makes "off" mean off without one.
/// - **Initiator** (see [`initiator_pushes`]): an agent-initiated run already
///   returns this report through its own open call.
/// - **Cancelled** (review M3): the user aborted the scan. The notice would say
///   "cImp finished a … audit … Call `security_audit` for the full report (it
///   re-runs the same scan)" — i.e. invite every armed agent to re-run the very
///   scan the user just stopped. A cancelled run's counts are partial anyway.
/// - **Duration**: below [`AUDIT_PUSH_MIN_SCAN_MS`] nobody walked away waiting.
fn scan_push_worthy(
    session_push: bool,
    initiator: Initiator,
    cancelled: bool,
    elapsed_ms: u64,
) -> bool {
    session_push
        && initiator_pushes(initiator)
        && !cancelled
        && elapsed_ms >= AUDIT_PUSH_MIN_SCAN_MS
}

/// The managed audit runner. Constructed once in the Tauri setup hook and
/// `app.manage`d as `Arc<AuditState>`.
pub struct AuditState {
    app: AppHandle,
    settings: SettingsHandle,
    inner: StdMutex<Inner>,
    /// V30 Phase C: the session-push bus, when this process has one. `None` in
    /// tests and any standalone construction without an `OffloadService`; a push
    /// is best-effort by contract, so its absence is a silent no-op. Send half
    /// only (see
    /// [`OffloadService::push_registry`](crate::offload::OffloadService::push_registry)),
    /// so there is no Arc cycle back into the offload service.
    pushes: Option<Arc<crate::offload::service::PushRegistry>>,
}

impl AuditState {
    /// `root` is the launch project root (`launch_cwd`) — the directory every
    /// scan runs against. `pushes` is the V30 session-push bus; `None` disables
    /// the completion push and changes nothing else.
    pub fn new(
        app: AppHandle,
        settings: SettingsHandle,
        root: PathBuf,
        pushes: Option<Arc<crate::offload::service::PushRegistry>>,
    ) -> Arc<Self> {
        Arc::new(Self {
            app,
            settings,
            pushes,
            inner: StdMutex::new(Inner {
                root,
                scanning: false,
                last_scan_at: None,
                tools: Vec::new(),
                census: CensusBlock::default(),
                cancel: None,
            }),
        })
    }

    /// The full (uncapped) snapshot for tab mount.
    pub fn snapshot(&self) -> AuditSnapshot {
        self.inner.lock().unwrap().snapshot(None)
    }

    /// The launch project root every scan runs against — the loopback
    /// `/audit/run` route compares it against the requesting child's cwd to
    /// reject misrouted requests (multi-instance wrong-instance guard).
    pub fn root(&self) -> PathBuf {
        self.inner.lock().unwrap().root.clone()
    }

    /// Whether the per-consumer expose toggle for `consumer` is on —
    /// `"opencode"` / `"offload"` map to their own flags; anything else
    /// (including the child's default, `"claude"`) maps to `expose_claude`.
    ///
    /// The loopback `/audit/run` route re-checks this on every run so that
    /// unchecking an expose toggle takes effect for already-running tabs —
    /// advertisement is gated separately at spawn/injection time
    /// (`tabs::config`), and a child spawned while its consumer was opted in
    /// outlives that gate. The master `enabled` switch is enforced by
    /// [`begin_scan`](Self::begin_scan), not here.
    pub fn consumer_exposed(&self, consumer: &str) -> bool {
        let ca = self.settings.current().code_audit;
        match consumer {
            "opencode" => ca.expose_opencode,
            "offload" => ca.expose_offload,
            _ => ca.expose_claude,
        }
    }

    /// Emit the current state as a (findings-capped) `audit-status` event.
    /// Built under the lock, emitted after dropping it (the graph-service
    /// discipline — a same-thread listener must not re-lock `inner`).
    fn emit_event(&self) {
        let snap = self
            .inner
            .lock()
            .unwrap()
            .snapshot(Some(EVENT_FINDINGS_PER_TOOL_CAP));
        let _ = self.app.emit(AUDIT_STATUS_EVENT, &snap);
    }

    /// Mutate one tool's state under the lock, then emit. No-op (still emits) if
    /// the id isn't in the current scan set.
    fn patch_tool<F: FnOnce(&mut ToolState)>(&self, id: AuditToolId, f: F) {
        {
            let mut inner = self.inner.lock().unwrap();
            if let Some(ts) = inner.tools.iter_mut().find(|t| t.id == id) {
                f(ts);
            }
        }
        self.emit_event();
    }

    /// Plan and *arm* a scan of `category`, but do not drive it — the shared
    /// prologue of [`start_scan`](Self::start_scan) (fire-and-forget, UI) and
    /// [`run_scan_and_wait`](Self::run_scan_and_wait) (awaited, the V26 code-audit
    /// MCP surface). Everything the two callers do *identically* lives here so
    /// they can never drift: master-switch enforcement, the census, quality
    /// auto-selection, the category + "nothing enabled" guard, `plan_scan`, and
    /// the busy check + `scanning` state transition under the lock, followed by
    /// the first `audit-status` emit.
    ///
    /// On success returns `(to_run, root, global_timeout, cancel)` — the
    /// enabled+applicable subset to launch, the scan root, the resolved global
    /// wall-clock budget, and this scan's cancel token — leaving the runner in
    /// the `scanning` state with its chips already emitted. The caller's only
    /// remaining job is to drive `run(to_run, root, global_timeout, cancel)`
    /// (spawned or awaited) which clears `scanning` when it finishes.
    ///
    /// Rejects (leaving state untouched) exactly as before: the master switch is
    /// off (enforced here, not just by tab visibility — the IPC commands and the
    /// MCP surface are registered unconditionally, so the graph/offload gating
    /// discipline applies), no tool of this category is enabled, or a scan of
    /// *either* category is already in flight (one scan at a time, globally).
    fn begin_scan(
        self: &Arc<Self>,
        category: Category,
    ) -> Result<(Vec<AuditToolConfig>, PathBuf, Duration, CancellationToken), String> {
        let cfg = self.settings.current().code_audit;
        // The master switch is enforced here, not just by tab visibility —
        // the IPC commands are registered unconditionally (the offload/graph
        // services gate the same way).
        if !cfg.enabled {
            return Err("Code Audit is disabled — enable it in cImp settings".to_string());
        }

        // Take the census ONCE per scan, off the lock (the walk can take up to a
        // couple seconds) — the root is fixed for the runner's lifetime, so a
        // quick read to fetch it and then walk is safe. Taken BEFORE the
        // enabled check below: quality auto-selection may flip flags.
        let root = self.inner.lock().unwrap().root.clone();
        let census = census::cached(&root);
        let census_block = CensusBlock {
            extensions: census.extensions(),
            markers: census.markers(),
        };

        // Auto mode: sync the QUALITY tools' enabled flags to the fresh census
        // (no-op in manual mode), then re-read the config so the plan below
        // uses the synced flags. Only for a Quality scan: auto-select never
        // touches security tools, and with the V26 MCP surface a scan can be
        // agent-triggered in the background — a `security_audit` call must not
        // rewrite persisted quality checkboxes as a side effect. (A Quality
        // scan keeps the sync by design: the report is documented to match the
        // auto-selected Code Audit view 1:1, and `refresh_census` already
        // applies the same sync on tab/Settings open.)
        let cfg = if category == Category::Quality && self.apply_quality_auto_select(&census) {
            self.settings.current().code_audit
        } else {
            cfg
        };

        // Only this category's tools; the other category belongs to the other
        // sub-tab.
        let in_category: Vec<&AuditToolConfig> = cfg
            .tools
            .iter()
            .filter(|t| adapters::adapter(t.id).category == category)
            .collect();
        if !in_category.iter().any(|t| t.enabled) {
            return Err(format!(
                "no {} audit tools are enabled",
                category_label(category)
            ));
        }

        // The chips (one per this-category tool) and the enabled+applicable
        // subset to launch — pure, so the filter is unit-tested directly.
        let (chips, to_run) = plan_scan(&cfg.tools, category, &census);

        let (root, cancel, global_timeout) = {
            let mut inner = self.inner.lock().unwrap();
            if inner.scanning {
                return Err("a scan is already in progress".to_string());
            }
            inner.scanning = true;
            inner.last_scan_at = Some(now_ms());
            inner.census = census_block;
            inner.tools = chips;
            let cancel = CancellationToken::new();
            inner.cancel = Some(cancel.clone());
            (inner.root.clone(), cancel, cfg.timeout_secs.max(1))
        };
        self.emit_event();

        Ok((to_run, root, Duration::from_secs(global_timeout), cancel))
    }

    /// Begin a scan of `category` (V25 Phase C). Only tools of that category are
    /// considered; of those, only the `enabled && applicable(&census)` set is
    /// launched. Rejects (clear error) if a scan of *either* category is already
    /// in flight (one scan at a time globally) or no tool of this category is
    /// enabled. Returns immediately; work runs on a background task and streams
    /// progress via `audit-status`.
    pub fn start_scan(self: &Arc<Self>, category: Category) -> Result<(), String> {
        let (to_run, root, global_timeout, cancel) = self.begin_scan(category)?;
        let this = self.clone();
        tauri::async_runtime::spawn(async move {
            // V30 Phase C: `Initiator::Gui` — nobody is awaiting this scan, so
            // its completion is exactly the kind of fact the session-push bus
            // exists for.
            this.run(
                to_run,
                root,
                category,
                global_timeout,
                cancel,
                Initiator::Gui,
            )
            .await;
        });
        Ok(())
    }

    /// Run a scan of `category` to completion and return its final (uncapped)
    /// snapshot — the awaited twin of [`start_scan`](Self::start_scan), added for
    /// the V26 code-audit MCP surface. An MCP tool call (`security_audit` /
    /// `quality_audit`, via the loopback `/audit/run` route) needs *one* value it
    /// can format and return, so unlike the UI's fire-and-forget path this drives
    /// [`run`](Self::run) inline instead of spawning it, and returns the snapshot
    /// `run` captured under the same lock acquisition that cleared the `scanning`
    /// flag — a separate `snapshot()` read here would race a scan that starts in
    /// the gap and replaces `inner.tools`.
    ///
    /// The `audit-status` stream is unaffected — [`run`](Self::run) emits per-tool
    /// exactly as it does for `start_scan` (the runner holds the `AppHandle`), so
    /// an MCP-triggered scan animates live in the Code audit view. Errors pass
    /// through from [`begin_scan`](Self::begin_scan) unchanged: master switch off,
    /// no tool of this category enabled, or `"a scan is already in progress"` when
    /// a UI (or other MCP) scan is mid-flight.
    pub async fn run_scan_and_wait(
        self: &Arc<Self>,
        category: Category,
    ) -> Result<AuditSnapshot, String> {
        let (to_run, root, global_timeout, cancel) = self.begin_scan(category)?;
        // V30 Phase C: `Initiator::Agent` — the snapshot returned below IS the
        // caller's tool result, so this path never pushes (it would duplicate
        // the report into the very session that asked for it).
        Ok(self
            .clone()
            .run(
                to_run,
                root,
                category,
                global_timeout,
                cancel,
                Initiator::Agent,
            )
            .await)
    }

    /// Sync every QUALITY tool's persisted `enabled` flag to `census` when
    /// `quality_auto_select` is on (see [`auto_select_quality`] for the rule).
    /// The write goes through the settings handle by id (broadcast + debounced
    /// save), so the Settings checkboxes follow live. Returns whether anything
    /// changed. No-op in manual mode.
    fn apply_quality_auto_select(&self, census: &census::Census) -> bool {
        let cfg = self.settings.current().code_audit;
        if !cfg.quality_auto_select {
            return false;
        }
        let mut tools = cfg.tools;
        if !auto_select_quality(&mut tools, census) {
            return false;
        }
        // Copy the recomputed flags back BY ID under the settings lock — the
        // live struct may have moved since the snapshot above, so never
        // replace the whole vec.
        self.settings.mutate(|s| {
            for t in &tools {
                if let Some(cur) = s.code_audit.tools.iter_mut().find(|c| c.id == t.id) {
                    cur.enabled = t.enabled;
                }
            }
        });
        true
    }

    /// Take the project census outside a scan (the ≤60s cache makes repeat
    /// calls cheap): apply quality auto-selection, store the census block so
    /// chip gating and the Settings "not applicable" hints work before the
    /// first scan, then emit and return the snapshot. Called from the
    /// `audit_refresh_census` IPC on tab mount and Settings open. While the
    /// feature is disabled, or a scan is in flight (whose start just did all
    /// of this), the state is returned untouched.
    pub fn refresh_census(self: &Arc<Self>) -> AuditSnapshot {
        let cfg = self.settings.current().code_audit;
        {
            let inner = self.inner.lock().unwrap();
            if !cfg.enabled || inner.scanning {
                return inner.snapshot(None);
            }
        }
        let root = self.inner.lock().unwrap().root.clone();
        let census = census::cached(&root);
        self.apply_quality_auto_select(&census);
        let stored = {
            let mut inner = self.inner.lock().unwrap();
            // A scan that started mid-walk owns the census/tools state now.
            if inner.scanning {
                false
            } else {
                inner.census = CensusBlock {
                    extensions: census.extensions(),
                    markers: census.markers(),
                };
                true
            }
        };
        if stored {
            self.emit_event();
        }
        self.snapshot()
    }

    /// Cancel the in-flight scan (kills running children). Errors if none.
    pub fn cancel_scan(&self) -> Result<(), String> {
        let cancel = self.inner.lock().unwrap().cancel.clone();
        match cancel {
            Some(c) => {
                c.cancel();
                Ok(())
            }
            None => Err("no scan is in progress".to_string()),
        }
    }

    /// The orchestration body: resolve each enabled tool, mark unresolvable ones
    /// `not-installed`, spawn the resolvable ones concurrently, await them all,
    /// then clear the scanning flag. Returns the final (uncapped) snapshot,
    /// captured under the same lock acquisition that clears `scanning`, so an
    /// awaiting caller ([`run_scan_and_wait`](Self::run_scan_and_wait)) reads
    /// *this* scan's result — never the state of a next scan that squeezes in
    /// after the flag clears. The fire-and-forget path drops it.
    async fn run(
        self: Arc<Self>,
        tools: Vec<AuditToolConfig>,
        root: PathBuf,
        category: Category,
        global_timeout: Duration,
        cancel: CancellationToken,
        initiator: Initiator,
    ) -> AuditSnapshot {
        // V30: wall clock for the completion push's duration floor. Started
        // here (not in `begin_scan`) so it measures the scan itself, not the
        // census walk and settings sync that precede it.
        let started = Instant::now();
        let git_repo = root.join(".git").exists();
        let mut handles = Vec::new();

        for tool in tools {
            // V25 Phase C: per-tool timeout override (`None` = the global
            // `code_audit.timeout_secs`). A build-style tool (dotnet-analyzers)
            // wants a longer budget than a linter.
            let timeout = effective_tool_timeout(tool.timeout_secs, global_timeout);
            // The same resolver Detect uses — override → project-local
            // `node_modules/.bin` (eslint/knip) → ebin/PATH — so a Detect ✓
            // can't disagree with what a scan launches.
            match super::resolve_audit_binary(tool.id, &tool.path, Some(&root)) {
                Err(_) => {
                    // Distinguish "no path configured and not discoverable"
                    // from "the configured path itself is broken" — the first
                    // is fixed by installing or setting a path, the second by
                    // correcting the path in Settings.
                    let (status, error) = if tool.path.trim().is_empty() {
                        (
                            ToolStatus::NotInstalled,
                            "not found on PATH or ebin — install it or set its path in Settings"
                                .to_string(),
                        )
                    } else {
                        (
                            ToolStatus::PathInvalid,
                            format!(
                                "configured path not found: {} — fix it in Settings",
                                tool.path.trim()
                            ),
                        )
                    };
                    self.patch_tool(tool.id, |ts| {
                        ts.status = status;
                        ts.error = Some(error);
                        ts.resolved = None;
                    });
                }
                Ok(resolved) => {
                    let path = resolved.clone();
                    self.patch_tool(tool.id, |ts| {
                        ts.status = ToolStatus::Running;
                        ts.resolved = Some(path);
                    });
                    let this = self.clone();
                    let cancel = cancel.clone();
                    let root = root.clone();
                    handles.push(tauri::async_runtime::spawn(async move {
                        this.run_one(tool, resolved, root, git_repo, timeout, cancel)
                            .await;
                    }));
                }
            }
        }

        for h in handles {
            let _ = h.await;
        }

        let snap = {
            let mut inner = self.inner.lock().unwrap();
            inner.scanning = false;
            inner.cancel = None;
            inner.snapshot(None)
        };
        self.emit_event();
        // V30 (review M3): `cancel` is this scan's own token — read it BEFORE
        // announcing, because `run` is reached on every exit path including the
        // cancelled one (`Outcome::Cancelled` classifies as `Failed`, so the
        // snapshot alone cannot tell the two apart).
        self.announce_scan_complete(
            &snap,
            category,
            initiator,
            cancel.is_cancelled(),
            started.elapsed().as_millis() as u64,
        );
        snap
    }

    /// V30 Phase C producer: announce a finished **GUI-initiated** scan into
    /// every channel-armed session of this instance.
    ///
    /// **Pull twin (milestone invariant 2): the V26 audit tools + the report.**
    /// Anything this notice states is re-derivable by calling `security_audit` /
    /// `quality_audit` (which re-runs and formats the same snapshot) and is
    /// already rendered in the Code audit tab and the `audit_snapshot` IPC. A
    /// dropped push costs timeliness, never information; no new tool is added.
    ///
    /// Gated on [`scan_push_worthy`] — GUI-initiated, not cancelled, and long
    /// enough to have been worth waiting for — plus a LIVE read of
    /// `offload.session_push`, so turning the feature off stops app-side pushes
    /// immediately (the child-side latch is per-tab-until-restart; this is the
    /// half that can react at once).
    ///
    /// Best-effort and non-blocking (`try_send` under the hood): no bus, no
    /// channel-armed child, or a full queue all mean "not delivered", and the
    /// scan neither retries nor fails because of it.
    fn announce_scan_complete(
        &self,
        snap: &AuditSnapshot,
        category: Category,
        initiator: Initiator,
        cancelled: bool,
        elapsed_ms: u64,
    ) {
        let Some(pushes) = self.pushes.as_ref() else {
            return;
        };
        // "Off means off": the gate is read LIVE here, so the producer stops the
        // moment the user unticks it — the child-side latch cannot.
        let session_push = self.settings.current().offload.session_push;
        if !scan_push_worthy(session_push, initiator, cancelled, elapsed_ms) {
            return;
        }
        let notice = audit_push_notice(snap, category);
        let delivered = pushes.push_broadcast(notice);
        tracing::debug!(
            root = %snap.root,
            category = ?category,
            delivered,
            "audit: pushed scan-complete notice"
        );
    }

    /// Run one resolved tool end to end: spawn, capture, classify, parse SARIF,
    /// record the result, emit. Independent of the other tools.
    async fn run_one(
        self: Arc<Self>,
        tool: AuditToolConfig,
        resolved: PathBuf,
        root: PathBuf,
        git_repo: bool,
        timeout: Duration,
        cancel: CancellationToken,
    ) {
        let adapter = adapters::adapter(tool.id);
        let started = Instant::now();
        let report_path = match adapter.transport {
            Transport::ReportFile => Some(temp_report_path(tool.id)),
            Transport::Stdout => None,
        };
        let argv = adapter.full_argv(
            &root,
            report_path.as_deref(),
            git_repo,
            &tool.extra_args,
            &tool.ruleset,
        );

        let cap = spawn_and_capture(&resolved, &argv, adapter.env, &root, timeout, &cancel).await;
        let duration_ms = started.elapsed().as_millis() as u64;

        // SARIF is only meaningful for a completed (non-killed) child; a
        // cancelled/timed-out gitleaks may have left a half-written report.
        let sarif = match &cap.outcome {
            Outcome::Exited(_) => {
                read_sarif(adapter.transport, &cap.stdout, report_path.as_deref()).await
            }
            _ => String::new(),
        };
        // A truncated stdout only invalidates the SARIF when stdout IS the
        // SARIF; for report-file tools it merely truncates captured logs.
        let sarif_truncated = adapter.transport == Transport::Stdout && cap.stdout_truncated;
        let (status, findings, error) = finalize_outcome(
            tool.id,
            adapter,
            cap.outcome,
            &sarif,
            sarif_truncated,
            &cap.stdout,
            &cap.stderr,
            &root,
            timeout,
        );

        // Scan-coverage: the lockfiles/manifests osv-scanner reports scanning,
        // pulled from the same SARIF in a second best-effort pass (osv-scanner
        // only — its `runs[].artifacts` are the audit-only coverage signal).
        let scanned_artifacts = if tool.id == AuditToolId::OsvScanner && status == ToolStatus::Done
        {
            parsers::sarif_scanned_artifacts(&sarif, &root)
        } else {
            Vec::new()
        };

        // Clean up the temp report regardless of outcome.
        if let Some(p) = &report_path {
            let _ = tokio::fs::remove_file(p).await;
        }

        let ok = status == ToolStatus::Done;
        let findings_count = findings.len();
        self.patch_tool(tool.id, |ts| {
            ts.status = status;
            ts.findings = findings;
            ts.duration_ms = duration_ms;
            ts.error = error;
            ts.scanned_artifacts = scanned_artifacts;
        });

        record_audit_run(tool.id, &root, findings_count, duration_ms, ok);
    }
}

/// A temp SARIF report path under the app's temp scratch dir (same
/// `std::env::temp_dir()` root as `attach`/`fsutil`). Parent is created; the
/// file is removed after parse.
fn temp_report_path(id: AuditToolId) -> PathBuf {
    let dir = std::env::temp_dir().join("cimp-audit");
    let _ = std::fs::create_dir_all(&dir);
    dir.join(format!(
        "{}-{}.sarif",
        id.command_name(),
        uuid::Uuid::new_v4()
    ))
}

/// Read the tool's SARIF from wherever its adapter delivers it.
async fn read_sarif(transport: Transport, stdout: &str, report: Option<&Path>) -> String {
    match transport {
        Transport::Stdout => stdout.to_string(),
        Transport::ReportFile => match report {
            // A clean gitleaks run may not write the file at all → empty SARIF,
            // which the parser turns into zero findings.
            Some(p) => tokio::fs::read_to_string(p).await.unwrap_or_default(),
            None => String::new(),
        },
    }
}

/// Turn a completed [`Capture`] into the tool's terminal state. Pure — the
/// `sarif` text (a tool's stdout or report-file contents; still named `sarif`
/// for continuity though a quality tool may emit JSON/text) has already been
/// resolved from the adapter's transport by the caller (empty for a killed
/// child), and `sarif_truncated` says whether that text is known-incomplete
/// (stdout blew the capture cap / drain timed out). This is the one place the
/// audit runner's findings-vs-error exit semantics are applied.
///
/// V25 Phase C decision table (output always parsed via the adapter's parser):
///
/// | outcome | parsed findings | result |
/// |---|---|---|
/// | spawn error / cancel / timeout | — | `failed` |
/// | [`ExitClass::Error`] (non-findings non-zero code) | — | `failed` (code + tail) |
/// | Clean/Findings but output known-truncated | — | `failed` (incomplete) |
/// | [`ExitClass::Clean`] (exit 0), any parsed findings | ≥ 0 | `done` (findings authoritative) |
/// | [`ExitClass::Findings`] code, ≥ 1 parsed finding | ≥ 1 | `done` with findings |
/// | [`ExitClass::Findings`] code, 0 parsed findings | 0 | `failed` (report lost) |
///
/// The Clean row is the V25 correction: cppcheck ALWAYS exits 0 (findings only
/// in its report) and eslint exits 0 when it has warnings-only — both must be
/// read from their parsed output on a clean exit, not assumed empty. A clean
/// exit with genuinely empty/absent output still parses to zero findings and
/// stays `done`-no-findings (gitleaks writes no report on a clean run). The
/// "report lost" guard fires only for a *findings exit code* whose output didn't
/// parse — never for a clean exit, whose empty output is a legitimate clean bill.
#[allow(clippy::too_many_arguments)]
fn finalize_outcome(
    id: AuditToolId,
    adapter: &Adapter,
    outcome: Outcome,
    sarif: &str,
    sarif_truncated: bool,
    stdout: &str,
    stderr: &str,
    root: &Path,
    timeout: Duration,
) -> (ToolStatus, Vec<AuditFinding>, Option<String>) {
    match outcome {
        Outcome::SpawnError(e) => (
            ToolStatus::Failed,
            Vec::new(),
            Some(format!("failed to launch: {e}")),
        ),
        Outcome::Cancelled => (
            ToolStatus::Failed,
            Vec::new(),
            Some("scan cancelled".to_string()),
        ),
        Outcome::TimedOut => (
            ToolStatus::Failed,
            Vec::new(),
            Some(format!("timed out after {}s", timeout.as_secs())),
        ),
        Outcome::Exited(code) => {
            let class = adapter.classify_exit(code);
            match class {
                ExitClass::Error => (
                    ToolStatus::Failed,
                    Vec::new(),
                    Some(exit_error_message(code, stderr, stdout)),
                ),
                ExitClass::Clean | ExitClass::Findings => {
                    // A known-incomplete SARIF must never parse into a
                    // reassuring "0 findings": fail loudly instead.
                    if sarif_truncated {
                        return (
                            ToolStatus::Failed,
                            Vec::new(),
                            Some(format!(
                                "SARIF output exceeded the {} MiB capture cap — results are incomplete and were discarded",
                                MAX_OUTPUT_BYTES / (1024 * 1024)
                            )),
                        );
                    }
                    // `cwd = root` so SARIF paths normalize project-relative.
                    let findings = parse_findings(id, sarif, root);
                    // A findings exit code with zero parsed findings means the
                    // report was lost (missing/unreadable temp file, malformed
                    // JSON) — the one thing this feature must not present as a
                    // clean pass.
                    if class == ExitClass::Findings && findings.is_empty() {
                        let code_str = code
                            .map(|c| c.to_string())
                            .unwrap_or_else(|| "unknown".to_string());
                        let tail = diag_tail(stderr, stdout);
                        let mut msg = format!(
                            "exit code {code_str} reports findings, but the SARIF report was empty or unreadable — findings were lost"
                        );
                        if !tail.is_empty() {
                            msg.push_str(": ");
                            msg.push_str(&tail);
                        }
                        return (ToolStatus::Failed, Vec::new(), Some(msg));
                    }
                    (ToolStatus::Done, findings, None)
                }
            }
        }
    }
}

/// Parse a tool's captured output into tagged findings (project-relative paths).
/// V25 Phase C: dispatches to the tool's adapter [`AuditParser`] — SARIF for the
/// security trio + most linters, else the tool-specific JSON/JSONL/text decoder
/// (eslint, typos, knip, cargo-machete) — rather than assuming SARIF. `output`
/// is the tool's stdout (stdout transport) or report-file contents (report-file
/// transport), already resolved by the caller.
fn parse_findings(id: AuditToolId, output: &str, root: &Path) -> Vec<AuditFinding> {
    adapters::adapter(id)
        .parser
        .parse(output, root)
        .into_iter()
        .map(|diag| AuditFinding { tool: id, diag })
        .collect()
}

/// V25 Phase C: the wall-clock timeout for one tool. A per-tool
/// [`AuditToolConfig::timeout_secs`](crate::settings::AuditToolConfig) override
/// (clamped to ≥ 1s) wins; `None` falls back to the global
/// `code_audit.timeout_secs` (`global`, already clamped ≥ 1s by the caller).
fn effective_tool_timeout(tool_secs: Option<u64>, global: Duration) -> Duration {
    tool_secs
        .map(|s| Duration::from_secs(s.max(1)))
        .unwrap_or(global)
}

/// V30 Phase C: the one-line `<channel>` notice for a finished GUI scan. Pure
/// (snapshot in, notice out) so the wording and the counts are testable without
/// a runner, an `AppHandle`, or a push bus. Deliberately short and factual —
/// this text costs a model turn in every armed tab that receives it — and it
/// names its pull twin so the receiving agent knows where the full report is.
///
/// Locked decision 9 (as a type since #47): the two shapes below are `&'static
/// str` templates and every slot carries an app-owned value — counts of
/// done/failed tools, the category word, the configured scan root and the fixed
/// pull-twin tool name. **No finding message is ever quoted**: a finding's text
/// is scanner output about attacker-influenced source, and a push starts a turn
/// on an idle session. The conditional clause is a second template rather than
/// a `push_str`, because `PushNotice::new` will not take a composed `String` at
/// all — which is the point.
fn audit_push_notice(snap: &AuditSnapshot, category: Category) -> PushNotice {
    let done = snap
        .tools
        .iter()
        .filter(|t| t.status == ToolStatus::Done)
        .count();
    let failed = snap
        .tools
        .iter()
        .filter(|t| t.status == ToolStatus::Failed)
        .count();
    let tool = match category {
        Category::Security => "security_audit",
        Category::Quality => "quality_audit",
    };
    let findings = snap.total_findings.to_string();
    let done = done.to_string();
    let meta = [("kind", "audit")];
    if failed > 0 {
        PushNotice::new(
            "cImp finished a {} audit of {} (started from the cImp UI): {} findings from {} tool(s), {} tool(s) failed. Call {} for the full report (it re-runs the same scan).",
            &[
                category_label(category),
                &snap.root,
                &findings,
                &done,
                &failed.to_string(),
                tool,
            ],
            meta,
        )
    } else {
        PushNotice::new(
            "cImp finished a {} audit of {} (started from the cImp UI): {} findings from {} tool(s). Call {} for the full report (it re-runs the same scan).",
            &[category_label(category), &snap.root, &findings, &done, tool],
            meta,
        )
    }
}

/// The lowercase category word used in the "no … tools are enabled" error.
fn category_label(category: Category) -> &'static str {
    match category {
        Category::Security => "security",
        Category::Quality => "quality",
    }
}

/// Recompute each QUALITY tool's `enabled` flag to its automatic value:
/// factory-default-enabled AND applicable to `census`. Deriving from
/// [`crate::settings::default_audit_tools`] keeps the default-disabled
/// heavyweights (dotnet-analyzers — runs a real build; semgrep-quality —
/// network rulesets) opt-in even when applicable, and security tools are
/// never in scope. Returns whether any flag changed. Pure — the settings
/// write and the auto/manual mode gate live in
/// [`AuditState::apply_quality_auto_select`].
pub(crate) fn auto_select_quality(tools: &mut [AuditToolConfig], census: &census::Census) -> bool {
    let defaults = crate::settings::default_audit_tools();
    let default_enabled = |id: AuditToolId| {
        defaults
            .iter()
            .find(|d| d.id == id)
            .is_some_and(|d| d.enabled)
    };
    let mut changed = false;
    for t in tools.iter_mut() {
        if adapters::adapter(t.id).category != Category::Quality {
            continue;
        }
        let want = default_enabled(t.id) && adapters::adapter(t.id).applicable(census);
        if t.enabled != want {
            t.enabled = want;
            changed = true;
        }
    }
    changed
}

/// V25 Phase C: pure scan planning. From the configured tools, the target
/// `category`, and the project `census`, produce `(chips, to_run)`:
///
/// - `chips`: one [`ToolState`] per tool **of this category** (the other
///   category belongs to the other tab), in configured order — `idle` when
///   disabled, `skipped-not-applicable` when enabled but gated off by the
///   census, `running` (about to resolve) when enabled + applicable.
/// - `to_run`: the enabled + applicable subset actually launched.
///
/// Split out from [`AuditState::start_scan`] so the category + applicability
/// filter is unit-testable without a Tauri `AppHandle`.
fn plan_scan(
    tools: &[AuditToolConfig],
    category: Category,
    census: &census::Census,
) -> (Vec<ToolState>, Vec<AuditToolConfig>) {
    let mut chips = Vec::new();
    let mut to_run = Vec::new();
    for t in tools
        .iter()
        .filter(|t| adapters::adapter(t.id).category == category)
    {
        if !t.enabled {
            chips.push(ToolState::idle(t.id, category));
        } else if adapters::adapter(t.id).applicable(census) {
            chips.push(ToolState::fresh(t.id, category));
            to_run.push(t.clone());
        } else {
            chips.push(ToolState::skipped_not_applicable(t.id, category));
        }
    }
    (chips, to_run)
}

/// A concise `failed` message for a tool-error exit, appending a short tail of
/// the tool's own diagnostics (stderr preferred, else stdout) so an offline /
/// misconfigured run surfaces the tool's reason, not a bare code.
fn exit_error_message(code: Option<i32>, stderr: &str, stdout: &str) -> String {
    let code_str = code
        .map(|c| c.to_string())
        .unwrap_or_else(|| "unknown".to_string());
    let tail = diag_tail(stderr, stdout);
    if tail.is_empty() {
        format!("exited with code {code_str}")
    } else {
        format!("exited with code {code_str}: {tail}")
    }
}

/// The last 3 lines of the tool's own diagnostics (stderr preferred, else
/// stdout) — the short "why" tail appended to failure messages.
fn diag_tail(stderr: &str, stdout: &str) -> String {
    let detail = if stderr.trim().is_empty() {
        stdout.trim()
    } else {
        stderr.trim()
    };
    let lines: Vec<&str> = detail.lines().collect();
    lines[lines.len().saturating_sub(3)..].join("\n")
}

/// Record one tool run in the persistent tool-activity store (kind `audit`).
/// `chars` carries the finding count for audit entries.
fn record_audit_run(id: AuditToolId, root: &Path, findings: usize, ms: u64, ok: bool) {
    let rec = ActivityRecord {
        entry: ActivityEntry::new(
            ActivityKind::Audit,
            now_ms(),
            activity::root_key(root),
            "audit".to_string(),
            id.command_name().to_string(),
            root.display().to_string(),
            findings,
            ms,
            ok,
        ),
        request: format!("audit scan: {}", id.command_name()),
        response: format!("{findings} findings"),
    };
    activity::record_bg(rec);
}

// ── child spawn + capture ──────────────────────────────────────────────────

/// How a captured child ended.
enum Outcome {
    /// Exited on its own; `Option<i32>` is the exit code.
    Exited(Option<i32>),
    /// Exceeded the per-tool wall clock (child killed).
    TimedOut,
    /// The scan was cancelled (child killed).
    Cancelled,
    /// The child never spawned.
    SpawnError(String),
}

struct Capture {
    stdout: String,
    /// True when stdout exceeded [`MAX_OUTPUT_BYTES`] (or its drain timed out):
    /// whatever was kept is incomplete and must not be read as a full SARIF.
    stdout_truncated: bool,
    stderr: String,
    outcome: Outcome,
}

/// Spawn `resolved` with `argv` (cwd = `root`, `env` forced, console-suppressed
/// on Windows), capturing stdout/stderr on their own tasks so a killed child
/// still yields what it printed. Honors the per-tool `timeout` and the scan
/// `cancel` token — both kill the child's whole process tree (see
/// [`kill_tree`]).
async fn spawn_and_capture(
    resolved: &Path,
    argv: &[String],
    env: &[(&str, &str)],
    root: &Path,
    timeout: Duration,
    cancel: &CancellationToken,
) -> Capture {
    let mut cmd = tokio::process::Command::new(resolved);
    cmd.args(argv)
        .current_dir(root)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    for (k, v) in env {
        cmd.env(k, v);
    }
    // Don't flash a console window per spawned scanner on Windows.
    #[cfg(windows)]
    cmd.creation_flags(crate::procutil::CREATE_NO_WINDOW);

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            return Capture {
                stdout: String::new(),
                stdout_truncated: false,
                stderr: String::new(),
                outcome: Outcome::SpawnError(e.to_string()),
            }
        }
    };
    // Backstop reaper if cImp dies hard before kill_on_drop fires.
    crate::process_guard::guard_child(&child);

    let out_task = tokio::spawn(crate::procutil::read_capped(
        child.stdout.take(),
        MAX_OUTPUT_BYTES,
    ));
    let err_task = tokio::spawn(crate::procutil::read_capped(
        child.stderr.take(),
        MAX_OUTPUT_BYTES,
    ));

    let sleep = tokio::time::sleep(timeout);
    tokio::pin!(sleep);
    // Only the `child.wait()` branch borrows `child`, so `kill_tree` below is
    // free to use it once the select resolves.
    let outcome = tokio::select! {
        _ = cancel.cancelled() => Outcome::Cancelled,
        _ = &mut sleep => Outcome::TimedOut,
        res = child.wait() => match res {
            Ok(status) => Outcome::Exited(status.code()),
            Err(e) => Outcome::SpawnError(e.to_string()),
        },
    };
    if matches!(outcome, Outcome::Cancelled | Outcome::TimedOut) {
        // Whole-tree kill: semgrep's forked workers must not survive holding
        // the pipe write ends (they'd keep scanning and stall the drains).
        crate::procutil::kill_tree(&mut child).await;
    }

    let (stdout, stdout_truncated) = crate::procutil::drain_capture(out_task).await;
    let (stderr, _) = crate::procutil::drain_capture(err_task).await;
    Capture {
        stdout,
        stdout_truncated,
        stderr,
        outcome,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::checks::Severity;

    // ── SARIF fixtures ─────────────────────────────────────────────────────
    //
    // NOTE: osv-scanner / gitleaks / semgrep are not installed in this
    // environment (checked at implementation time), so these are faithful
    // fixtures constructed from each tool's documented SARIF 2.1.0 output —
    // LIVE CAPTURE IS PENDING (the V23 live-verify recipe replaces them with
    // real captures once the binaries are dropped in `ebin/`). Each pins the
    // fields the findings table consumes: rule id → `Diag.code`, SARIF level →
    // `Diag.severity`, and a project-relative path.

    /// osv-scanner `scan source --format sarif`: a `Cargo.lock` vuln, rule id =
    /// the OSV/GHSA id, level `warning` (osv-scanner's default result level).
    const OSV_SARIF: &str = r#"{
      "version": "2.1.0",
      "runs": [{
        "tool": { "driver": { "name": "osv-scanner" } },
        "results": [{
          "ruleId": "GHSA-r8w9-5wcg-vfj7",
          "level": "warning",
          "message": { "text": "tokio 1.38.0 is affected by GHSA-r8w9-5wcg-vfj7" },
          "locations": [{
            "physicalLocation": {
              "artifactLocation": { "uri": "Cargo.lock" },
              "region": { "startLine": 1 }
            }
          }]
        }]
      }]
    }"#;

    /// gitleaks `--report-format sarif`: a secret hit, rule id = the gitleaks
    /// rule, level `error`, absolute `file://` URI that must relativize to the
    /// scan root.
    const GITLEAKS_SARIF: &str = r#"{
      "version": "2.1.0",
      "runs": [{
        "tool": { "driver": { "name": "gitleaks" } },
        "results": [{
          "ruleId": "generic-api-key",
          "level": "error",
          "message": { "text": "generic-api-key detected" },
          "locations": [{
            "physicalLocation": {
              "artifactLocation": { "uri": "file:///proj/root/src/lib/foo.ts" },
              "region": { "startLine": 42, "startColumn": 7 }
            }
          }]
        }]
      }]
    }"#;

    /// semgrep `--sarif`: a SAST hit, rule id = the semgrep rule, level `error`.
    const SEMGREP_SARIF: &str = r#"{
      "version": "2.1.0",
      "runs": [{
        "tool": { "driver": { "name": "semgrep" } },
        "results": [{
          "ruleId": "javascript.lang.security.audit.detect-non-literal-fs-filename",
          "level": "error",
          "message": { "text": "Detected non-literal fs filename" },
          "locations": [{
            "physicalLocation": {
              "artifactLocation": { "uri": "src/SettingsApp.svelte" },
              "region": { "startLine": 1291, "startColumn": 3 }
            }
          }]
        }]
      }]
    }"#;

    fn root() -> PathBuf {
        PathBuf::from("/proj/root")
    }

    #[test]
    fn osv_sarif_fixture_maps_to_findings() {
        let f = parse_findings(AuditToolId::OsvScanner, OSV_SARIF, &root());
        assert_eq!(f.len(), 1);
        assert_eq!(f[0].tool, AuditToolId::OsvScanner);
        assert_eq!(f[0].diag.code.as_deref(), Some("GHSA-r8w9-5wcg-vfj7"));
        assert_eq!(f[0].diag.severity, Severity::Warning);
        assert_eq!(f[0].diag.file, "Cargo.lock");
        assert_eq!(f[0].diag.line, 1);
    }

    #[test]
    fn gitleaks_sarif_fixture_relativizes_path() {
        let f = parse_findings(AuditToolId::Gitleaks, GITLEAKS_SARIF, &root());
        assert_eq!(f.len(), 1);
        assert_eq!(f[0].diag.code.as_deref(), Some("generic-api-key"));
        assert_eq!(f[0].diag.severity, Severity::Error);
        // The absolute file:// URI normalized project-relative against the root.
        assert_eq!(f[0].diag.file, "src/lib/foo.ts");
        assert_eq!(f[0].diag.line, 42);
        assert_eq!(f[0].diag.col, Some(7));
    }

    #[test]
    fn semgrep_sarif_fixture_maps_to_findings() {
        let f = parse_findings(AuditToolId::Semgrep, SEMGREP_SARIF, &root());
        assert_eq!(f.len(), 1);
        assert_eq!(
            f[0].diag.code.as_deref(),
            Some("javascript.lang.security.audit.detect-non-literal-fs-filename")
        );
        assert_eq!(f[0].diag.severity, Severity::Error);
        assert_eq!(f[0].diag.file, "src/SettingsApp.svelte");
        assert_eq!(f[0].diag.line, 1291);
    }

    // ── scan-coverage artifacts (osv-scanner) ───────────────────────────────

    /// osv-scanner SARIF carrying `runs[].artifacts`: a relative lockfile, an
    /// absolute `file://` manifest (must relativize to the root), and a
    /// duplicate (must dedupe).
    const OSV_ARTIFACTS_SARIF: &str = r#"{
      "version": "2.1.0",
      "runs": [{
        "tool": { "driver": { "name": "osv-scanner" } },
        "artifacts": [
          { "location": { "uri": "Cargo.lock" } },
          { "location": { "uri": "file:///proj/root/package-lock.json" } },
          { "location": { "uri": "Cargo.lock" } }
        ],
        "results": []
      }]
    }"#;

    // (These exercise the shared `parsers::sarif_scanned_artifacts` against the
    // audit fixtures — the coverage-line contract belongs to this runner even
    // though the extraction now lives with the SARIF parser. The
    // sibling-prefix `relativize` guard and `read_capped` truncation tests live
    // with their helpers: `checks::parsers` / `procutil`.)

    #[test]
    fn osv_artifacts_extract_relative_deduped() {
        let a = parsers::sarif_scanned_artifacts(OSV_ARTIFACTS_SARIF, &root());
        assert_eq!(
            a,
            vec!["Cargo.lock".to_string(), "package-lock.json".to_string()]
        );
    }

    #[test]
    fn absent_or_malformed_artifacts_yield_empty() {
        // No `artifacts` key at all (the findings-only fixtures / older tools).
        assert!(parsers::sarif_scanned_artifacts(OSV_SARIF, &root()).is_empty());
        // Malformed SARIF is best-effort empty, never an error.
        assert!(parsers::sarif_scanned_artifacts("not json", &root()).is_empty());
        // An empty-uri artifact is skipped.
        let empty_uri = r#"{"runs":[{"artifacts":[{"location":{"uri":""}}]}]}"#;
        assert!(parsers::sarif_scanned_artifacts(empty_uri, &root()).is_empty());
    }

    // ── finalize_outcome: the findings-vs-error exit semantics ──────────────

    #[test]
    fn findings_exit_is_done_with_findings() {
        let a = adapters::adapter(AuditToolId::OsvScanner);
        let (status, findings, error) = finalize_outcome(
            AuditToolId::OsvScanner,
            a,
            Outcome::Exited(Some(1)), // findings-present code
            OSV_SARIF,
            false,
            "",
            "",
            &root(),
            Duration::from_secs(600),
        );
        assert_eq!(status, ToolStatus::Done);
        assert_eq!(findings.len(), 1);
        assert!(error.is_none());
    }

    #[test]
    fn clean_exit_is_done_no_findings() {
        let a = adapters::adapter(AuditToolId::Gitleaks);
        let (status, findings, error) = finalize_outcome(
            AuditToolId::Gitleaks,
            a,
            Outcome::Exited(Some(0)),
            "", // clean run wrote no report
            false,
            "",
            "",
            &root(),
            Duration::from_secs(600),
        );
        assert_eq!(status, ToolStatus::Done);
        assert!(findings.is_empty());
        assert!(error.is_none());
    }

    /// A findings exit code whose SARIF turned out empty/unparseable (missing
    /// temp report, mid-JSON truncation upstream) must be a loud failure, never
    /// a clean "0 findings" pass.
    #[test]
    fn findings_exit_with_empty_sarif_is_failed() {
        let a = adapters::adapter(AuditToolId::Gitleaks);
        for sarif in ["", "not json at all"] {
            let (status, findings, error) = finalize_outcome(
                AuditToolId::Gitleaks,
                a,
                Outcome::Exited(Some(1)), // "leaks found"
                sarif,
                false,
                "",
                "report write failed: permission denied",
                &root(),
                Duration::from_secs(600),
            );
            assert_eq!(status, ToolStatus::Failed, "sarif = {sarif:?}");
            assert!(findings.is_empty());
            let msg = error.unwrap();
            assert!(msg.contains("findings were lost"), "{msg}");
            assert!(msg.contains("permission denied"), "{msg}");
        }
    }

    /// A capped (known-incomplete) stdout SARIF is discarded as a failure even
    /// on a "clean" exit — a truncated document must not read as a clean bill.
    #[test]
    fn truncated_sarif_is_failed_not_clean() {
        let a = adapters::adapter(AuditToolId::OsvScanner);
        for code in [0, 1] {
            let (status, findings, error) = finalize_outcome(
                AuditToolId::OsvScanner,
                a,
                Outcome::Exited(Some(code)),
                OSV_SARIF, // even a parseable prefix is untrustworthy
                true,      // stdout blew the capture cap
                "",
                "",
                &root(),
                Duration::from_secs(600),
            );
            assert_eq!(status, ToolStatus::Failed, "exit {code}");
            assert!(findings.is_empty());
            assert!(error.unwrap().contains("incomplete"), "exit {code}");
        }
    }

    #[test]
    fn tool_error_exit_is_failed_with_message() {
        let a = adapters::adapter(AuditToolId::Semgrep);
        let (status, findings, error) = finalize_outcome(
            AuditToolId::Semgrep,
            a,
            Outcome::Exited(Some(2)), // neither 0 nor a findings code
            "",
            false,
            "",
            "network unreachable while downloading rules",
            &root(),
            Duration::from_secs(600),
        );
        assert_eq!(status, ToolStatus::Failed);
        assert!(findings.is_empty());
        let msg = error.unwrap();
        assert!(msg.contains("code 2"), "{msg}");
        assert!(msg.contains("network unreachable"), "{msg}");
    }

    #[test]
    fn timeout_and_cancel_and_spawn_error_map_to_failed() {
        let a = adapters::adapter(AuditToolId::Semgrep);
        let f = |o: Outcome| {
            finalize_outcome(
                AuditToolId::Semgrep,
                a,
                o,
                "",
                false,
                "",
                "",
                &root(),
                Duration::from_secs(5),
            )
        };
        let (s, _, e) = f(Outcome::TimedOut);
        assert_eq!(s, ToolStatus::Failed);
        assert!(e.unwrap().contains("timed out after 5s"));

        let (s, _, e) = f(Outcome::Cancelled);
        assert_eq!(s, ToolStatus::Failed);
        assert_eq!(e.as_deref(), Some("scan cancelled"));

        let (s, _, e) = f(Outcome::SpawnError("boom".into()));
        assert_eq!(s, ToolStatus::Failed);
        assert!(e.unwrap().contains("boom"));
    }

    // ── V25 finalize: clean-exit-with-findings semantics ────────────────────

    /// cppcheck ALWAYS exits 0 (no `--error-exitcode`); its findings live only
    /// in the report. A clean (exit-0) run with a populated report must be
    /// `done`-WITH-findings — the V25 correction to V23's "clean = empty".
    #[test]
    fn cppcheck_clean_exit_with_report_yields_findings() {
        const CPPCHECK_SARIF: &str = r#"{
          "version": "2.1.0",
          "runs": [{
            "tool": { "driver": { "name": "cppcheck" } },
            "results": [{
              "ruleId": "nullPointer",
              "level": "error",
              "message": { "text": "Null pointer dereference" },
              "locations": [{
                "physicalLocation": {
                  "artifactLocation": { "uri": "src/main.c" },
                  "region": { "startLine": 10 }
                }
              }]
            }]
          }]
        }"#;
        let a = adapters::adapter(AuditToolId::Cppcheck);
        let (status, findings, error) = finalize_outcome(
            AuditToolId::Cppcheck,
            a,
            Outcome::Exited(Some(0)), // cppcheck's normal "findings present" path
            CPPCHECK_SARIF,
            false,
            "",
            "",
            &root(),
            Duration::from_secs(600),
        );
        assert_eq!(status, ToolStatus::Done);
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].diag.code.as_deref(), Some("nullPointer"));
        assert_eq!(findings[0].diag.file, "src/main.c");
        assert!(error.is_none());
    }

    /// eslint exits 0 when it has warnings-only, yet its JSON still carries them.
    /// The [`AuditParser::EslintJson`](super::parsers::AuditParser) decoder runs
    /// on the clean-exit output, so those warnings surface as `done`-with-
    /// findings rather than a false clean bill.
    #[test]
    fn eslint_clean_exit_with_warnings_yields_findings() {
        const ESLINT_JSON: &str = r#"[
          { "filePath": "/proj/root/src/app.ts",
            "messages": [
              { "ruleId": "eqeqeq", "severity": 1, "message": "use ===", "line": 4, "column": 3 }
            ]
          }
        ]"#;
        let a = adapters::adapter(AuditToolId::Eslint);
        let (status, findings, error) = finalize_outcome(
            AuditToolId::Eslint,
            a,
            Outcome::Exited(Some(0)), // warnings-only ⇒ exit 0
            ESLINT_JSON,
            false,
            ESLINT_JSON, // eslint's output is on stdout
            "",
            &root(),
            Duration::from_secs(600),
        );
        assert_eq!(status, ToolStatus::Done);
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].diag.severity, Severity::Warning);
        assert_eq!(findings[0].diag.code.as_deref(), Some("eqeqeq"));
        assert_eq!(findings[0].diag.file, "src/app.ts");
        assert!(error.is_none());
    }

    /// A genuinely clean run (empty/absent output) stays `done`-no-findings for
    /// every parser — the "report lost" guard is for a *findings exit code* whose
    /// output didn't parse, never for a clean exit whose empty output is a real
    /// clean bill.
    #[test]
    fn clean_exit_with_empty_output_is_done_no_findings() {
        for id in [
            AuditToolId::Cppcheck,
            AuditToolId::Eslint,
            AuditToolId::Oxlint,
        ] {
            let a = adapters::adapter(id);
            let (status, findings, error) = finalize_outcome(
                id,
                a,
                Outcome::Exited(Some(0)),
                "",
                false,
                "",
                "",
                &root(),
                Duration::from_secs(600),
            );
            assert_eq!(status, ToolStatus::Done, "{id:?}");
            assert!(findings.is_empty(), "{id:?}");
            assert!(error.is_none(), "{id:?}");
        }
    }

    // ── begin_scan / run_scan_and_wait guard coverage ──────────────────────
    //
    // `begin_scan` (shared by `start_scan` and the V26 `run_scan_and_wait` MCP
    // surface) has three reject paths — master switch off, "no <category> audit
    // tools are enabled", and "a scan is already in progress". Its ONLY pure,
    // AppHandle-free logic is the census + `plan_scan` planning, which the
    // `plan_scan_*` and `auto_select_quality_*` tests below already pin
    // directly. The three rejects themselves live behind `&Arc<AuditState>`,
    // which needs a Tauri `AppHandle`: this crate builds `tauri` WITHOUT the
    // `test` feature and has no `tauri::test` mock anywhere, so no `AuditState`
    // is constructible in a unit test. Rather than bolt on a mock runtime (or
    // extract the two one-line guards purely just to satisfy a test), the guard
    // behavior is verified live per the V26 MCP verification recipe (busy →
    // "scan already in progress" tool error; disabled master switch → refused).
    // `run_scan_and_wait` adds no new guard logic of its own — it shares
    // `begin_scan` verbatim with the already-exercised `start_scan` and only
    // awaits `run` inline instead of spawning it.

    // ── V25 plan_scan: category + applicability + disabled filter ────────────

    fn tool_cfg(id: AuditToolId, enabled: bool) -> AuditToolConfig {
        AuditToolConfig {
            id,
            enabled,
            path: String::new(),
            extra_args: Vec::new(),
            ruleset: String::new(),
            timeout_secs: None,
        }
    }

    /// A Quality scan never launches a Security tool, hides a disabled tool as
    /// `idle`, and reports an enabled-but-inapplicable tool `skipped-not-
    /// applicable`; only enabled + applicable tools land in `to_run`.
    #[test]
    fn plan_scan_quality_filters_category_disabled_and_applicability() {
        // A Rust + JS project: no `.py`, no `.go`.
        let census = census::Census::from_parts(&["ts", "rs"], &["Cargo.toml", "package.json"]);
        let tools = vec![
            tool_cfg(AuditToolId::OsvScanner, true), // security — excluded here
            tool_cfg(AuditToolId::Oxlint, true),     // quality, applicable (ts)
            tool_cfg(AuditToolId::Ruff, true),       // quality, NOT applicable (no py)
            tool_cfg(AuditToolId::CargoMachete, false), // quality, disabled
            tool_cfg(AuditToolId::Typos, true),      // quality, always applicable
        ];
        let (chips, to_run) = plan_scan(&tools, Category::Quality, &census);

        // No security tool leaks into a quality scan.
        assert!(chips.iter().all(|c| c.category == Category::Quality));
        assert!(!chips.iter().any(|c| c.id == AuditToolId::OsvScanner));

        let status = |id: AuditToolId| chips.iter().find(|c| c.id == id).unwrap().status;
        assert_eq!(status(AuditToolId::Oxlint), ToolStatus::Running);
        assert_eq!(status(AuditToolId::Ruff), ToolStatus::SkippedNotApplicable);
        assert_eq!(status(AuditToolId::CargoMachete), ToolStatus::Idle);
        assert_eq!(status(AuditToolId::Typos), ToolStatus::Running);

        // to_run is exactly the enabled + applicable set, in configured order.
        let run_ids: Vec<AuditToolId> = to_run.iter().map(|t| t.id).collect();
        assert_eq!(run_ids, vec![AuditToolId::Oxlint, AuditToolId::Typos]);
    }

    /// A Security scan excludes every Quality tool; the always-applicable trio
    /// runs even against an empty census.
    #[test]
    fn plan_scan_security_excludes_quality_tools() {
        let census = census::Census::default();
        let tools = vec![
            tool_cfg(AuditToolId::OsvScanner, true),
            tool_cfg(AuditToolId::Gitleaks, true),
            tool_cfg(AuditToolId::Oxlint, true), // quality — must not appear
        ];
        let (chips, to_run) = plan_scan(&tools, Category::Security, &census);
        assert!(chips.iter().all(|c| c.category == Category::Security));
        assert!(!chips.iter().any(|c| c.id == AuditToolId::Oxlint));
        let run_ids: Vec<AuditToolId> = to_run.iter().map(|t| t.id).collect();
        assert_eq!(
            run_ids,
            vec![AuditToolId::OsvScanner, AuditToolId::Gitleaks]
        );
    }

    /// Quality auto-selection: each quality tool's `enabled` becomes
    /// factory-default-enabled AND census-applicable; the default-disabled
    /// heavyweights stay opt-in even when applicable; security tools are
    /// never touched; a second pass is a no-op.
    #[test]
    fn auto_select_quality_follows_census_and_keeps_heavyweights_opt_in() {
        // A Rust + TS project: no `.py` / `.go` / `.java` / C / eslint config.
        let census = census::Census::from_parts(&["ts", "rs"], &["Cargo.toml", "package.json"]);
        // Start from an everything-flipped manual state so every rule below
        // proves auto-select actually rewrote (or deliberately skipped) it.
        let mut tools = crate::settings::default_audit_tools();
        for t in tools.iter_mut() {
            t.enabled = !t.enabled;
        }
        assert!(auto_select_quality(&mut tools, &census));
        let enabled = |id: AuditToolId| tools.iter().find(|t| t.id == id).unwrap().enabled;

        // Default-on + applicable → selected.
        assert!(enabled(AuditToolId::Oxlint)); // .ts
        assert!(enabled(AuditToolId::CargoMachete)); // Cargo.toml
        assert!(enabled(AuditToolId::Knip)); // package.json
        assert!(enabled(AuditToolId::Typos)); // ungated
                                              // Default-on but not applicable → deselected.
        assert!(!enabled(AuditToolId::Ruff));
        assert!(!enabled(AuditToolId::GolangciLint));
        assert!(!enabled(AuditToolId::Cppcheck));
        assert!(!enabled(AuditToolId::Pmd));
        assert!(!enabled(AuditToolId::Eslint));
        // Default-disabled heavyweights stay opt-in — semgrep-quality is
        // ungated (always applicable) and still must NOT be auto-enabled.
        assert!(!enabled(AuditToolId::SemgrepQuality));
        assert!(!enabled(AuditToolId::DotnetAnalyzers));
        // Security tools keep their (flipped-off) state — out of scope.
        assert!(!enabled(AuditToolId::OsvScanner));
        assert!(!enabled(AuditToolId::Gitleaks));
        assert!(!enabled(AuditToolId::Semgrep));

        // Idempotent: the flags now ARE the automatic values.
        assert!(!auto_select_quality(&mut tools, &census));
    }

    /// V25 Phase C: a per-tool `timeout_secs` override wins over the global; a
    /// `None` override falls back to the global; a 0 override clamps to 1s.
    #[test]
    fn effective_tool_timeout_prefers_override_else_global() {
        let global = Duration::from_secs(600);
        assert_eq!(effective_tool_timeout(None, global), global);
        assert_eq!(
            effective_tool_timeout(Some(1200), global),
            Duration::from_secs(1200)
        );
        // A 0 override is clamped to ≥ 1s (never an instant timeout).
        assert_eq!(
            effective_tool_timeout(Some(0), global),
            Duration::from_secs(1)
        );
    }

    // ── snapshot wire cap ───────────────────────────────────────────────────

    #[test]
    fn event_snapshot_caps_findings_and_flags_truncated() {
        let mut ts = ToolState::fresh(AuditToolId::Gitleaks, Category::Security);
        ts.status = ToolStatus::Done;
        let one = || AuditFinding {
            tool: AuditToolId::Gitleaks,
            diag: Diag {
                severity: Severity::Error,
                code: Some("generic-api-key".into()),
                message: "secret".into(),
                file: "a.ts".into(),
                line: 1,
                col: None,
            },
        };
        ts.findings = (0..EVENT_FINDINGS_PER_TOOL_CAP + 10)
            .map(|_| one())
            .collect();
        let inner = Inner {
            root: root(),
            scanning: false,
            last_scan_at: Some(123),
            tools: vec![ts],
            census: CensusBlock::default(),
            cancel: None,
        };
        // Full snapshot (IPC): everything, never truncated.
        let full = inner.snapshot(None);
        assert_eq!(full.total_findings, EVENT_FINDINGS_PER_TOOL_CAP + 10);
        assert!(!full.truncated);
        assert_eq!(
            full.tools[0].findings.len(),
            EVENT_FINDINGS_PER_TOOL_CAP + 10
        );
        // Event snapshot: capped, truncated flag set, total still true.
        let evt = inner.snapshot(Some(EVENT_FINDINGS_PER_TOOL_CAP));
        assert!(evt.truncated);
        assert_eq!(evt.tools[0].findings.len(), EVENT_FINDINGS_PER_TOOL_CAP);
        assert_eq!(evt.total_findings, EVENT_FINDINGS_PER_TOOL_CAP + 10);
    }

    /// V25 Phase C: the snapshot serializes the census block (both cap modes),
    /// so the split UI can gate chips off a single IPC/event payload.
    #[test]
    fn snapshot_carries_census_block() {
        let inner = Inner {
            root: root(),
            scanning: false,
            last_scan_at: None,
            tools: vec![ToolState::fresh(AuditToolId::Oxlint, Category::Quality)],
            census: CensusBlock {
                extensions: vec!["rs".into(), "ts".into()],
                markers: vec!["Cargo.toml".into()],
            },
            cancel: None,
        };
        for cap in [None, Some(EVENT_FINDINGS_PER_TOOL_CAP)] {
            let snap = inner.snapshot(cap);
            assert_eq!(
                snap.census.extensions,
                vec!["rs".to_string(), "ts".to_string()]
            );
            assert_eq!(snap.census.markers, vec!["Cargo.toml".to_string()]);
            assert_eq!(snap.tools[0].category, Category::Quality);
        }
    }

    // ── Rust↔TS wire tripwire (runtime types) ──────────────────────────────

    /// The runtime wire shapes (`AuditSnapshot`/`ToolState`/`AuditFinding`/
    /// `AuditDiag` — including `checks::Diag`, which crosses the wire verbatim
    /// inside `AuditFinding`) must stay mirrored in codeAudit/types.ts. The
    /// settings-side audit types have their own tripwire in `settings::schema`;
    /// without this one, renaming a `Diag`/`ToolState` field keeps cargo green
    /// while the Code Audit table silently reads `undefined`.
    const AUDIT_RUNTIME_TS: &str = include_str!("../../../src/lib/codeAudit/types.ts");

    #[test]
    fn runtime_wire_shapes_mirrored_in_code_audit_types_ts() {
        // A fully-populated snapshot — every Option is Some, so every wire
        // field key appears in the serialized JSON and gets checked.
        let snap = AuditSnapshot {
            root: "/proj/root".into(),
            scanning: true,
            last_scan_at: Some(123),
            tools: vec![ToolState {
                id: AuditToolId::Gitleaks,
                category: Category::Security,
                status: ToolStatus::Done,
                findings: vec![AuditFinding {
                    tool: AuditToolId::Gitleaks,
                    diag: Diag {
                        severity: Severity::Error,
                        code: Some("generic-api-key".into()),
                        message: "secret".into(),
                        file: "a.ts".into(),
                        line: 1,
                        col: Some(2),
                    },
                }],
                duration_ms: 5,
                error: Some("boom".into()),
                resolved: Some(PathBuf::from("C:/ebin/gitleaks.exe")),
                scanned_artifacts: vec!["Cargo.lock".into()],
            }],
            census: CensusBlock {
                extensions: vec!["rs".into(), "ts".into()],
                markers: vec!["Cargo.toml".into()],
            },
            total_findings: 1,
            truncated: false,
        };
        fn assert_keys(v: &serde_json::Value, ts: &str) {
            match v {
                serde_json::Value::Object(m) => {
                    for (k, val) in m {
                        assert!(
                            ts.contains(&format!("{k}:")),
                            "wire field `{k}` is missing from src/lib/codeAudit/types.ts — \
                             update the TS mirror together with the Rust type",
                        );
                        assert_keys(val, ts);
                    }
                }
                serde_json::Value::Array(a) => a.iter().for_each(|x| assert_keys(x, ts)),
                _ => {}
            }
        }
        assert_keys(
            &serde_json::to_value(&snap).expect("snapshot serializes"),
            AUDIT_RUNTIME_TS,
        );
    }

    #[test]
    fn status_and_severity_wire_strings_mirrored_in_code_audit_types_ts() {
        // Exhaustive matches are the Rust-side half of the tripwire: a new
        // variant that isn't added to these lists is a compile error.
        let statuses = [
            ToolStatus::Idle,
            ToolStatus::Running,
            ToolStatus::Done,
            ToolStatus::Failed,
            ToolStatus::NotInstalled,
            ToolStatus::PathInvalid,
            ToolStatus::SkippedNotApplicable,
        ];
        fn _statuses_exhaustive(s: ToolStatus) {
            match s {
                ToolStatus::Idle
                | ToolStatus::Running
                | ToolStatus::Done
                | ToolStatus::Failed
                | ToolStatus::NotInstalled
                | ToolStatus::PathInvalid
                | ToolStatus::SkippedNotApplicable => {}
            }
        }
        for s in statuses {
            let wire = serde_json::to_value(s)
                .unwrap()
                .as_str()
                .unwrap()
                .to_string();
            assert!(
                AUDIT_RUNTIME_TS.contains(&format!("'{wire}'")),
                "ToolStatus wire `{wire}` is missing from the TS `AuditToolStatus` union",
            );
        }

        // V25 Phase C: the two Category wire strings must be in the TS
        // `AuditCategory` union (a scan is dispatched by, and every ToolState
        // tagged with, this value).
        let categories = [Category::Security, Category::Quality];
        fn _categories_exhaustive(c: Category) {
            match c {
                Category::Security | Category::Quality => {}
            }
        }
        for c in categories {
            let wire = serde_json::to_value(c)
                .unwrap()
                .as_str()
                .unwrap()
                .to_string();
            assert!(
                AUDIT_RUNTIME_TS.contains(&format!("'{wire}'")),
                "Category wire `{wire}` is missing from the TS `AuditCategory` union",
            );
        }

        let severities = [Severity::Error, Severity::Warning, Severity::Note];
        fn _severities_exhaustive(s: Severity) {
            match s {
                Severity::Error | Severity::Warning | Severity::Note => {}
            }
        }
        for sev in severities {
            let wire = serde_json::to_value(sev)
                .unwrap()
                .as_str()
                .unwrap()
                .to_string();
            assert!(
                AUDIT_RUNTIME_TS.contains(&format!("'{wire}'")),
                "Severity wire `{wire}` is missing from the TS `AuditSeverity` union",
            );
        }
    }

    // A portable long-running child for timeout/cancel tests.
    fn sleeper() -> (PathBuf, Vec<String>) {
        #[cfg(windows)]
        {
            // `ping -n 30 127.0.0.1` blocks ~30s without extra tooling.
            let p = which::which("ping").expect("ping on PATH");
            (p, vec!["-n".into(), "30".into(), "127.0.0.1".into()])
        }
        #[cfg(not(windows))]
        {
            let p = which::which("sleep").expect("sleep on PATH");
            (p, vec!["30".into()])
        }
    }

    #[tokio::test]
    async fn timeout_kills_child_and_reports_timed_out() {
        let (prog, argv) = sleeper();
        let cancel = CancellationToken::new();
        let started = Instant::now();
        let cap = spawn_and_capture(
            &prog,
            &argv,
            &[],
            &std::env::temp_dir(),
            Duration::from_millis(300),
            &cancel,
        )
        .await;
        // Returns promptly (child killed), not after the ~30s sleep.
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "child was not killed on timeout"
        );
        assert!(
            matches!(cap.outcome, Outcome::TimedOut),
            "expected TimedOut"
        );
    }

    #[tokio::test]
    async fn cancel_kills_child() {
        let (prog, argv) = sleeper();
        let cancel = CancellationToken::new();
        let c2 = cancel.clone();
        // Cancel shortly after the child starts.
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(200)).await;
            c2.cancel();
        });
        let started = Instant::now();
        let cap = spawn_and_capture(
            &prog,
            &argv,
            &[],
            &std::env::temp_dir(),
            Duration::from_secs(60),
            &cancel,
        )
        .await;
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "child was not killed on cancel"
        );
        assert!(
            matches!(cap.outcome, Outcome::Cancelled),
            "expected Cancelled"
        );
    }

    #[tokio::test]
    async fn spawn_error_for_missing_binary() {
        let cancel = CancellationToken::new();
        let cap = spawn_and_capture(
            Path::new("cimp-definitely-not-a-real-binary-xyz"),
            &[],
            &[],
            &std::env::temp_dir(),
            Duration::from_secs(5),
            &cancel,
        )
        .await;
        assert!(matches!(cap.outcome, Outcome::SpawnError(_)));
    }

    // ── V30 Phase C: completion-push gate + payload ────────────────────────

    /// A finished snapshot with the given per-tool statuses.
    fn done_snapshot(
        statuses: &[(AuditToolId, ToolStatus)],
        total_findings: usize,
    ) -> AuditSnapshot {
        AuditSnapshot {
            root: "/proj/root".to_string(),
            scanning: false,
            last_scan_at: Some(1_700_000_000_000),
            tools: statuses
                .iter()
                .map(|(id, status)| ToolState {
                    status: *status,
                    ..ToolState::fresh(*id, adapters::adapter(*id).category)
                })
                .collect(),
            census: CensusBlock::default(),
            total_findings,
            truncated: false,
        }
    }

    /// The gate itself: only a GUI-initiated scan announces itself. An
    /// agent-initiated run returns the same report through its own open
    /// `tools/call`, so pushing would duplicate it into that session — and a
    /// push into an idle tab costs a model turn.
    #[test]
    fn only_gui_initiated_scans_push() {
        assert!(
            initiator_pushes(Initiator::Gui),
            "the Scan button has no other completion path"
        );
        assert!(
            !initiator_pushes(Initiator::Agent),
            "an MCP/offload-initiated run already returns its report"
        );
    }

    /// The full gate. `run` is reached on EVERY exit path, so the producer —
    /// not the caller — is where cancellation and triviality are filtered out.
    #[test]
    fn scan_push_worthy_filters_cancelled_agent_and_trivial_scans() {
        let long = AUDIT_PUSH_MIN_SCAN_MS;
        assert!(
            scan_push_worthy(true, Initiator::Gui, false, long),
            "a real GUI scan announces itself"
        );

        // Review M6: "off means off" app-side. The child-side declaration is
        // latched until the tab restarts, so this is the half that can react to
        // the toggle at once — and it dominates everything else.
        assert!(
            !scan_push_worthy(false, Initiator::Gui, false, long),
            "offload.session_push off ⇒ no producer fires, restart or not"
        );

        // Review M3: cancelling must not broadcast "cImp finished a … audit …
        // Call security_audit for the full report (it re-runs the same scan)" —
        // that invites every armed agent to re-run what the user just aborted.
        // `Outcome::Cancelled` classifies as `Failed`, so the snapshot alone
        // cannot distinguish the two: the cancel token is the only signal.
        assert!(
            !scan_push_worthy(true, Initiator::Gui, true, long),
            "a cancelled scan must never push"
        );

        // Review LOW: the duration floor the graph twin already had.
        assert!(
            !scan_push_worthy(true, Initiator::Gui, false, 200),
            "a 200ms scan is not worth a model turn in every armed session"
        );
        assert!(
            !scan_push_worthy(true, Initiator::Gui, false, AUDIT_PUSH_MIN_SCAN_MS - 1),
            "just under the floor stays silent"
        );

        // The echo guard still dominates every other input.
        for cancelled in [false, true] {
            for ms in [0, long, long * 10] {
                assert!(
                    !scan_push_worthy(true, Initiator::Agent, cancelled, ms),
                    "an agent-initiated run never pushes (cancelled={cancelled}, {ms}ms)"
                );
            }
        }

        assert_eq!(
            AUDIT_PUSH_MIN_SCAN_MS, 30_000,
            "same floor as the graph twin's GRAPH_PUSH_MIN_BUILD_MS by design — \
             the two producers cost the same model turn"
        );
    }

    /// The pushed line is short, factual, and names its pull twin (milestone
    /// invariant 2) rather than inlining the report.
    #[test]
    fn audit_push_notice_states_counts_and_its_pull_twin() {
        let snap = done_snapshot(
            &[
                (AuditToolId::Gitleaks, ToolStatus::Done),
                (AuditToolId::OsvScanner, ToolStatus::Done),
            ],
            7,
        );
        let notice = audit_push_notice(&snap, Category::Security);
        let line = notice.content();
        assert_eq!(
            notice.meta.get("kind").map(String::as_str),
            Some("audit"),
            "the notice keeps its channel attribute"
        );
        assert!(line.contains("security"), "names the category: {line}");
        assert!(line.contains("/proj/root"), "names the scope: {line}");
        assert!(line.contains("7 findings"), "carries the count: {line}");
        assert!(line.contains("2 tool(s)"), "counts completed tools: {line}");
        assert!(
            line.contains("security_audit"),
            "names the pull twin, never inlines the report: {line}"
        );
        assert!(
            !line.contains("failed"),
            "no failure clause when nothing failed: {line}"
        );
        assert!(line.len() < 400, "stays a one-liner: {line}");
    }

    /// A failed tool is surfaced, so "0 findings" from a broken scan can't read
    /// as a clean bill of health, and a Quality scan points at `quality_audit`.
    #[test]
    fn audit_push_notice_reports_failures_and_the_quality_twin() {
        let snap = done_snapshot(
            &[
                (AuditToolId::Ruff, ToolStatus::Done),
                (AuditToolId::Eslint, ToolStatus::Failed),
                (AuditToolId::Pmd, ToolStatus::NotInstalled),
            ],
            0,
        );
        let notice = audit_push_notice(&snap, Category::Quality);
        let line = notice.content();
        assert!(line.contains("quality"), "names the category: {line}");
        assert!(line.contains("0 findings"), "carries the count: {line}");
        assert!(
            line.contains("1 tool(s)"),
            "only `done` tools count: {line}"
        );
        assert!(
            line.contains("1 tool(s) failed"),
            "surfaces failure: {line}"
        );
        assert!(
            line.contains("quality_audit"),
            "names the quality pull twin: {line}"
        );
    }
}
