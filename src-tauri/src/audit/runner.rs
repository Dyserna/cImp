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
use crate::plugins::manifest::SandboxReq;
use crate::plugins::registry::EffectiveTool;
use crate::settings::SettingsHandle;

use super::adapters::{self, Category, ExitClass, Transport};
use super::census;
use super::runnable::{AuditParser, RunnableAudit, ToolKey};

/// Tauri event emitted on every per-tool transition, carrying a (findings-
/// capped) [`AuditSnapshot`]. Phase C subscribes to this.
pub const AUDIT_STATUS_EVENT: &str = "audit-status";

/// Per-tool captured-output cap. SARIF for a large scan is sizable but bounded;
/// 16 MiB is generous headroom without letting a runaway tool exhaust memory.
// `pub(crate)` so `plugins::spec` can pin it against docs/TOOL-PLUGINS.md.
pub(crate) const MAX_OUTPUT_BYTES: usize = 16 * 1024 * 1024;

/// The event payload caps each tool's findings at this many; past it, the
/// event sets [`AuditSnapshot::truncated`] and the frontend pulls the full set
/// via `audit_snapshot`. The full snapshot IPC is never capped.
// `pub(crate)` so `plugins::spec` can pin it against docs/TOOL-PLUGINS.md.
pub(crate) const EVENT_FINDINGS_PER_TOOL_CAP: usize = 500;

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
    /// Serializes to the tool's wire id: a built-in's kebab id (`osv-scanner` |
    /// `gitleaks` | `semgrep`) or, since V38, a plugin tool's
    /// `name@version/tool-id`. Attribution is always the REGISTRY entry that was
    /// spawned — never a name the tool printed inside its own output.
    pub tool: ToolKey,
    pub diag: Diag,
}

/// One tool's live state within the current (or last) scan.
#[derive(Clone, Debug, serde::Serialize)]
pub struct ToolState {
    /// Wire id — a built-in's kebab id, or a plugin tool's key (see
    /// [`ToolKey`]). The two namespaces cannot collide, so every consumer that
    /// used to key off a built-in id keeps working and simply sees ids it does
    /// not recognize for the plugin population.
    pub id: ToolKey,
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
    ///
    /// `impl Into<ToolKey>` so a built-in call site still reads
    /// `ToolState::fresh(id, category)` — the two populations differ in what
    /// they are keyed BY, not in how a chip is made.
    fn fresh(id: impl Into<ToolKey>, category: Category) -> Self {
        Self {
            id: id.into(),
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
    fn idle(id: impl Into<ToolKey>, category: Category) -> Self {
        Self {
            status: ToolStatus::Idle,
            ..Self::fresh(id, category)
        }
    }

    /// V25 Phase C: an enabled tool that doesn't apply to this project's census —
    /// reported `skipped-not-applicable`, never launched.
    fn skipped_not_applicable(id: impl Into<ToolKey>, category: Category) -> Self {
        Self {
            status: ToolStatus::SkippedNotApplicable,
            ..Self::fresh(id, category)
        }
    }

    /// V38: a tool that will never be launched because planning refused it —
    /// a manifest that cannot produce a runnable tool. A `failed` chip carrying
    /// the reason, because the alternative (dropping it from the fan-out) makes
    /// a tool the user enabled vanish from the report without a word.
    fn failed_to_plan(id: impl Into<ToolKey>, category: Category, error: String) -> Self {
        Self {
            status: ToolStatus::Failed,
            error: Some(error),
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

    /// A fresh snapshot of the app's live settings.
    ///
    /// #48 M-6: the offload worker's audit path reaches the runner through the
    /// process-global handle ([`crate::audit::global`]) and has no `AppHandle`
    /// of its own, so this is how it resolves the injection hierarchy for the
    /// report it is about to deliver. Same store the loopback route reads
    /// through `live_settings`; a snapshot, so nothing downstream can observe a
    /// half-applied edit.
    pub fn settings_now(&self) -> crate::settings::Settings {
        self.settings.current()
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
    fn patch_tool<F: FnOnce(&mut ToolState)>(&self, id: &ToolKey, f: F) {
        {
            let mut inner = self.inner.lock().unwrap();
            if let Some(ts) = inner.tools.iter_mut().find(|t| &t.id == id) {
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
    /// On success returns `(to_run, root, global_timeout, cancel, sandbox)` —
    /// the enabled+applicable subset to launch, the scan root, the resolved
    /// global wall-clock budget, this scan's cancel token, and the V33
    /// OS-sandbox config — leaving the runner in the `scanning` state with its
    /// chips already emitted. The caller's only remaining job is to drive
    /// `run(..)` (spawned or awaited) which clears `scanning` when it finishes.
    ///
    /// The sandbox config is resolved HERE, once, from the same settings
    /// snapshot everything else in this scan comes from: a scan that started
    /// under one boundary must not have half its tools run under another
    /// because the user toggled the switch mid-scan.
    ///
    /// Rejects (leaving state untouched) exactly as before: the master switch is
    /// off (enforced here, not just by tab visibility — the IPC commands and the
    /// MCP surface are registered unconditionally, so the graph/offload gating
    /// discipline applies), no tool of this category is enabled, or a scan of
    /// *either* category is already in flight (one scan at a time, globally).
    #[allow(clippy::type_complexity)]
    fn begin_scan(
        self: &Arc<Self>,
        category: Category,
    ) -> Result<
        (
            Vec<RunnableAudit>,
            PathBuf,
            Duration,
            CancellationToken,
            crate::sandbox::SandboxCfg,
        ),
        String,
    > {
        let settings = self.settings.current();
        let sandbox = crate::sandbox::SandboxCfg::from_settings(&settings);
        // V38: resolved from the SAME settings snapshot as everything else in
        // this scan — a scan that started under one configuration must not run
        // half its tools under another.
        let tool_plugins = settings.tool_plugins.clone();
        let cfg = settings.code_audit;
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
        // Auto-selection writes into the tool-plugins container, so re-read
        // THAT (not `code_audit`, whose per-tool array is gone) before planning
        // — otherwise the scan would run the selection the census just replaced.
        let tool_plugins = if category == Category::Quality
            && self.apply_quality_auto_select(&census, &tool_plugins, &root)
        {
            self.settings.current().tool_plugins
        } else {
            tool_plugins
        };

        // ONE population since Phase E. The fourteen tools cImp ships are
        // embedded manifests living in the same registry a dropped-in plugin
        // lands in, so the fan-out reads one list and applies one set of rules.
        //
        // Two properties hold by construction and are pinned by test:
        //
        // * cImp's own plugins are laid down FIRST by `loader::scan_all`, and
        //   nothing below reorders or filters by provenance, so no user plugin
        //   can remove a built-in scanner from a fan-out (the security floor);
        // * the project root handed to the registry is the runner's own `root`,
        //   which `main.rs` sets from `current_dir()` — THE LAUNCH CWD, the same
        //   value `plugins_project_key` hands the settings pane. A per-project
        //   binary path is stored under that key, so resolving against anything
        //   else (a graph root found by an ancestor walk, say) would silently
        //   miss every project override.
        let tools = registry_tools(&tool_plugins, &root);
        let (chips, to_run) = plan_scan(&tools, category, &census);

        // "Nothing to run" spans both provenances: a plugin-only category (this
        // milestone's "add language X in one drop") must not be rejected with
        // "no tools are enabled" while the user is looking at an enabled,
        // path-configured tool. The wording is unchanged, because from the
        // user's side the fact is the same one.
        if !tools
            .iter()
            .any(|t| t.enabled && audit_category(t) == Some(category))
        {
            return Err(format!(
                "no {} audit tools are enabled",
                category_label(category)
            ));
        }

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

        Ok((
            to_run,
            root,
            Duration::from_secs(global_timeout),
            cancel,
            sandbox,
        ))
    }

    /// Begin a scan of `category` (V25 Phase C). Only tools of that category are
    /// considered; of those, only the `enabled && applicable(&census)` set is
    /// launched. Rejects (clear error) if a scan of *either* category is already
    /// in flight (one scan at a time globally) or no tool of this category is
    /// enabled. Returns immediately; work runs on a background task and streams
    /// progress via `audit-status`.
    pub fn start_scan(self: &Arc<Self>, category: Category) -> Result<(), String> {
        let (to_run, root, global_timeout, cancel, sandbox) = self.begin_scan(category)?;
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
                sandbox,
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
        let (to_run, root, global_timeout, cancel, sandbox) = self.begin_scan(category)?;
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
                sandbox,
            )
            .await)
    }

    /// Sync every QUALITY tool's persisted `enabled` flag to `census` when
    /// `quality_auto_select` is on (see [`auto_select_quality`] for the rule).
    /// The write goes through the settings handle by id (broadcast + debounced
    /// save), so the Settings checkboxes follow live. Returns whether anything
    /// changed. No-op in manual mode.
    fn apply_quality_auto_select(
        &self,
        census: &census::Census,
        cfg: &crate::settings::ToolPluginsSettings,
        root: &Path,
    ) -> bool {
        if !self.settings.current().code_audit.quality_auto_select {
            return false;
        }
        let Some(store) = crate::plugins::global() else {
            return false;
        };
        let tools = crate::plugins::registry::effective_tools(&store.snapshot(), cfg, Some(root));
        let changes = auto_select_quality(&tools, census);
        if changes.is_empty() {
            return false;
        }
        // Written BY TOOL ID under the settings lock — the live container may
        // have moved since the snapshot above, so never replace a subtree.
        self.settings.mutate(|s| {
            let plugin = s
                .tool_plugins
                .plugins
                .entry(crate::plugins::builtin::AUDIT_PLUGIN_KEY.to_string())
                .or_default();
            for (id, want) in &changes {
                plugin.tools.entry(id.clone()).or_default().enabled = *want;
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
        self.apply_quality_auto_select(&census, &self.settings.current().tool_plugins, &root);
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

    /// V38 Phase D: the tools a scan of `category` **would** run right now —
    /// the built-in roster as configured, plus this project's runnable plugin
    /// tools — as `idle` chips, so the Code Audit panel can render the real
    /// roster BEFORE any scan has produced one.
    ///
    /// # Why this is an IPC and not more frontend logic
    ///
    /// The pre-scan chip list used to be derived in TypeScript from
    /// `code_audit.tools`, which is the built-in roster and nothing else. After
    /// Phase C a plugin tool therefore appeared only once a scan had started —
    /// enabled, path-configured, and invisible until it ran. The join that
    /// answers "what would run" (plugin set ⋈ user state ⋈ project ⋈ census)
    /// exists exactly once, here on the Rust side; re-deriving half of it in the
    /// browser is how the two lists start disagreeing.
    ///
    /// Read-only and cheap by construction: the CACHED census (never a walk —
    /// `audit_refresh_census` owns that), the live settings snapshot, and the
    /// in-memory registry join. Nothing is spawned, resolved or written.
    ///
    /// Built-ins are reported for every configured entry INCLUDING disabled ones
    /// (the pre-V38 contract this replaces — the panel greys them). Plugin tools
    /// are the runnable set: a plugin tool that is disabled or has no path is
    /// configuration the Tool Plugins pane shows, not a chip promising a scan.
    pub fn effective_roster(&self, category: Category) -> Vec<ToolState> {
        let settings = self.settings.current();
        let (root, block) = {
            let inner = self.inner.lock().unwrap();
            (inner.root.clone(), inner.census.clone())
        };
        // An empty census means "not walked yet", not "this project has no
        // languages" — `partitionChips` makes the same distinction, and gating
        // on an unknown census would hide every language-specific tool on a cold
        // tab.
        let census_known = !(block.extensions.is_empty() && block.markers.is_empty());
        let census = census::Census::from_block(&block.extensions, &block.markers);
        plan_roster(
            &registry_tools(&settings.tool_plugins, &root),
            category,
            &census,
            census_known,
        )
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
    #[allow(clippy::too_many_arguments)]
    async fn run(
        self: Arc<Self>,
        tools: Vec<RunnableAudit>,
        root: PathBuf,
        category: Category,
        global_timeout: Duration,
        cancel: CancellationToken,
        initiator: Initiator,
        sandbox: crate::sandbox::SandboxCfg,
    ) -> AuditSnapshot {
        // V30: wall clock for the completion push's duration floor. Started
        // here (not in `begin_scan`) so it measures the scan itself, not the
        // census walk and settings sync that precede it.
        let started = Instant::now();
        let git_repo = root.join(".git").exists();
        let mut handles = Vec::new();

        for tool in tools {
            // Per-tool timeout override (`None` = the global
            // `code_audit.timeout_secs`). A build-style tool
            // (`dotnet-analyzers`) wants a longer budget than a linter.
            let timeout = effective_tool_timeout(tool.timeout_secs, global_timeout);
            // ONE resolution rule, branching only where the two provenances
            // genuinely differ: cImp resolves a bare command name for a tool it
            // shipped, and never for one it did not (decision 7). See
            // [`resolve_runnable`].
            match resolve_runnable(&tool, &root) {
                Err((status, error)) => {
                    self.patch_tool(&tool.key, |ts| {
                        ts.status = status;
                        ts.error = Some(error);
                        ts.resolved = None;
                    });
                }
                Ok(resolved) => {
                    let key = tool.key.clone();
                    let shown = resolved.clone();
                    self.patch_tool(&key, |ts| {
                        ts.status = ToolStatus::Running;
                        ts.resolved = Some(shown);
                    });
                    let this = self.clone();
                    let cancel = cancel.clone();
                    let root = root.clone();
                    let sandbox = sandbox.clone();
                    handles.push(tauri::async_runtime::spawn(async move {
                        this.run_one(tool, resolved, root, git_repo, timeout, cancel, sandbox)
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

    /// Run one resolved tool end to end: spawn, capture, classify, decode,
    /// record the result, emit. Independent of the other tools.
    ///
    /// **One function for both provenances since Phase E.** Before it there
    /// were two — an adapter twin and a plugin twin — and every rule about
    /// timeouts, cancellation, output caps, report cleanup and status events had
    /// to be written twice. What actually differs between a scanner cImp ships
    /// and one somebody dropped in a folder is entirely in the manifest that
    /// reached here (its posture, its ingest gate, whether it declared a `dir`
    /// argv), and a manifest is data. The one branch left is the coverage pass,
    /// which is a fact about `osv-scanner` specifically rather than about
    /// built-ins in general.
    #[allow(clippy::too_many_arguments)]
    async fn run_one(
        self: Arc<Self>,
        tool: RunnableAudit,
        resolved: PathBuf,
        root: PathBuf,
        git_repo: bool,
        timeout: Duration,
        cancel: CancellationToken,
        sandbox: crate::sandbox::SandboxCfg,
    ) {
        let started = Instant::now();
        // What the sandbox lane and the activity row call this run. For a
        // built-in that is its command name (`audit:semgrep`), which is what
        // those rows have said since V33 and what a user grepping them expects;
        // for a plugin it is the namespaced key, which is the only name it has.
        let subject = tool.spawn_subject();
        let report_path = match tool.transport {
            Transport::ReportFile => Some(temp_report_path(&subject)),
            Transport::Stdout => None,
        };
        let argv = tool.full_argv(&root, report_path.as_deref(), git_repo);

        // V33: a report-file tool writes its SARIF to the absolute path that is
        // already inside `argv`; the sandbox has to be able to let it. Derived
        // from the SAME `report_path` value, so the granted directory and the
        // argument cannot drift apart.
        let full_dirs = sandbox_full_dirs(tool.transport, report_path.as_deref());

        // The manifest's sandbox posture, resolved by the rules every plugin
        // seam shares (`plugins::posture`) rather than by a copy that lives
        // here: `run_check` and `run_command` read the same three fields, and
        // three spellings of what `required` means is three chances for one of
        // them to mean something else.
        //
        // `boundary_expected` is B-C1: a refusal row promises "this path was not
        // granted, every other grant was", which is false when nothing is being
        // granted at all. Screening still happens — a refused path never reaches
        // a `GrantRow` — only the row is withheld.
        let seam = crate::sandbox::audit_seam(&subject);
        let select = tool.runtime_select();
        let boundary_expected = sandbox.enabled && tool.sandbox != SandboxReq::Unsupported;
        let rows = crate::plugins::posture::screen_extra_grants(
            &seam,
            &root,
            &tool.extra_grants,
            boundary_expected,
        );
        crate::plugins::posture::runtime_canary(&seam, &root, &subject, &select, &resolved);

        let cap = spawn_and_capture(
            &resolved,
            &argv,
            &tool.env,
            &root,
            timeout,
            &cancel,
            &sandbox,
            &subject,
            &SpawnPosture {
                full_dirs,
                rows,
                runtime: select,
                sandbox_req: tool.sandbox,
            },
        )
        .await;
        let duration_ms = started.elapsed().as_millis() as u64;

        // Output is only meaningful for a completed (non-killed) child; a
        // cancelled/timed-out gitleaks may have left a half-written report.
        let sarif = match &cap.outcome {
            Outcome::Exited(_) => {
                read_sarif(tool.transport, &cap.stdout, report_path.as_deref()).await
            }
            _ => String::new(),
        };
        // A truncated stdout only invalidates the report when stdout IS the
        // report; for report-file tools it merely truncates captured logs.
        let sarif_truncated = tool.transport == Transport::Stdout && cap.stdout_truncated;
        let (status, findings, error) = finalize(
            &Finalize {
                key: tool.key.clone(),
                findings_exit_codes: &tool.findings_exit_codes,
                parser: tool.parser,
                gate: tool.gate,
            },
            cap.outcome,
            &sarif,
            sarif_truncated,
            &cap.stdout,
            &cap.stderr,
            &root,
            timeout,
        );

        // Scan-coverage: the lockfiles/manifests osv-scanner reports scanning,
        // pulled from the same SARIF in a second best-effort pass. `osv-scanner`
        // only — its `runs[].artifacts` are the audit-only coverage signal, and
        // no other tool emits them.
        let scanned_artifacts = if tool.key.is_builtin("osv-scanner") && status == ToolStatus::Done
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
        self.patch_tool(&tool.key, |ts| {
            ts.status = status;
            ts.findings = findings;
            ts.duration_ms = duration_ms;
            ts.error = error;
            ts.scanned_artifacts = scanned_artifacts;
        });

        record_audit_run(&subject, &root, findings_count, duration_ms, ok);
    }
}

/// Resolve one runnable tool to an on-disk binary, or say which kind of
/// misconfiguration this is.
///
/// The two provenances differ in exactly one way, and it is the amended
/// decision 10: cImp resolves a bare command NAME for a tool it shipped, and
/// never for one a user dropped in a folder.
///
/// * **Built-in.** A configured path wins verbatim (the V23 override contract:
///   "use exactly this binary" is a deliberate choice project-local resolution
///   must not second-guess). Otherwise the project's own `node_modules/.bin`
///   shim, if the manifest names one, then `ebin` then `PATH` on the declared
///   command. The two failure messages stay distinct: nothing configured and
///   nothing found is *not installed* (fixed by installing it), while a
///   configured path that does not resolve is *misconfigured* (fixed in
///   Settings). Presenting the second as the first is what sends a user
///   installing a tool they already have.
/// * **Plugin.** The configured path, and only that. Nothing is bundled and
///   cImp never guesses a binary for a definition it does not vouch for, so
///   there is no "not installed" state here — a tool with no path was never
///   runnable and never reached this list.
fn resolve_runnable(tool: &RunnableAudit, root: &Path) -> Result<PathBuf, (ToolStatus, String)> {
    let configured = tool.program.trim();
    let Some(command) = tool.command.as_deref() else {
        // A user plugin: verbatim, and it must be a real file. `resolve_command`
        // is deliberately not used — a PATH search on a plugin's behalf is the
        // guess decision 10 forbids.
        let path = PathBuf::from(configured);
        return if path.is_file() {
            Ok(path)
        } else {
            Err((
                ToolStatus::PathInvalid,
                format!(
                    "{}: configured path not found: {configured} — fix it in Settings, Tool \
                     Plugins",
                    tool.label
                ),
            ))
        };
    };
    if !configured.is_empty() {
        return crate::pty::resolve_command(configured).map_err(|_| {
            (
                ToolStatus::PathInvalid,
                format!("configured path not found: {configured} — fix it in Settings"),
            )
        });
    }
    if let Some(bin) = tool.project_local_bin.as_deref() {
        if let Some(local) = super::resolve_project_local_bin(root, bin) {
            return Ok(local);
        }
    }
    crate::pty::resolve_command(command).map_err(|_| {
        (
            ToolStatus::NotInstalled,
            "not found on PATH or ebin — install it or set its path in Settings".to_string(),
        )
    })
}

/// cImp's own scratch directory for audit report files, created if absent.
///
/// Split out from [`temp_report_path`] because V33 needs to name it twice: once
/// to build the path handed to the scanner, and once to grant the sandbox
/// container write access to it. It must EXIST before the grant — an ACE
/// cannot be written to a directory that is not there — and this is the one
/// place that is guaranteed.
fn audit_report_dir() -> PathBuf {
    let dir = std::env::temp_dir().join("cimp-audit");
    let _ = std::fs::create_dir_all(&dir);
    dir
}

/// A temp SARIF report path under the app's temp scratch dir (same
/// `std::env::temp_dir()` root as `attach`/`fsutil`). Parent is created; the
/// file is removed after parse.
fn temp_report_path(name: &str) -> PathBuf {
    // A plugin's key carries `@` and `/`, neither of which is a file name — the
    // uuid is what makes the path unique anyway, so the name is only a label
    // and is reduced to something a filesystem accepts.
    let label: String = name
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '-' })
        .collect();
    audit_report_dir().join(format!("{label}-{}.sarif", uuid::Uuid::new_v4()))
}

/// **Which directories a sandboxed scanner must be able to WRITE.**
///
/// Exactly one rule, and it is the whole rule: a [`Transport::ReportFile`] tool
/// is handed an absolute SARIF path under cImp's own scratch
/// ([`audit_report_dir`]) and writes its findings there, so the container needs
/// full access to that directory's *parent of the report path* — the same
/// directory the path in argv points into, which is what makes the grant and
/// the argument impossible to drift apart. A [`Transport::Stdout`] tool writes
/// nothing outside the (already granted) project root and gets no extra grant.
///
/// Pure, so the rule is testable without stamping an ACL on a real directory.
/// The grant is only ever *applied* when the plan comes back `Sandboxed`;
/// `sandbox::plan` discards these hints when the switch is off.
fn sandbox_full_dirs(transport: Transport, report_path: Option<&Path>) -> Vec<PathBuf> {
    match transport {
        Transport::ReportFile => report_path
            .and_then(|p| p.parent())
            .map(|d| vec![d.to_path_buf()])
            .unwrap_or_default(),
        Transport::Stdout => Vec::new(),
    }
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

/// What [`finalize`] needs to know about the tool whose run it is classifying.
///
/// A small owned struct rather than a `&RunnableAudit` so the findings-vs-error
/// matrix can be driven directly by a test that has no manifest, no registry and
/// no plugin set — which is most of them, because that matrix is where "empty is
/// not absent" lives.
pub(super) struct Finalize<'a> {
    pub key: ToolKey,
    pub findings_exit_codes: &'a [i32],
    pub parser: AuditParser,
    /// The ingest gate output passes before it becomes findings — resolved once
    /// on `RunnableAudit`, from the manifest's declaration and the RESOLVED
    /// parser (G2).
    pub gate: super::runnable::IngestGate,
}

#[allow(clippy::too_many_arguments)]
pub(super) fn finalize(
    spec: &Finalize,
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
            let class = adapters::classify_exit(code, spec.findings_exit_codes);
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
                    // V38 (decision 3): a plugin tool's output must be a SARIF
                    // log that SAYS something before any of it becomes a
                    // finding. Placed here — after the truncation guard, before
                    // the parse — so "ran, zero findings" (`\"runs\": []`) stays
                    // a clean pass while a blank or unrecognizable artifact
                    // becomes a loud tool error instead of a reassuring zero.
                    if let Err(why) = spec.gate.check(sarif) {
                        return (ToolStatus::Failed, Vec::new(), Some(why));
                    }
                    // `cwd = root` so SARIF paths normalize project-relative.
                    let findings = parse_findings(&spec.key, spec.parser, sarif, root);
                    // A findings exit code with zero parsed findings means the
                    // report was lost (missing/unreadable temp file, malformed
                    // JSON) — the one thing this feature must not present as a
                    // clean pass.
                    if class == ExitClass::Findings && findings.is_empty() {
                        let code_str = code
                            .map(|c| c.to_string())
                            .unwrap_or_else(|| "unknown".to_string());
                        let tail = diag_tail(stderr, stdout);
                        // **Empty is not the same as unreadable** (rc.9 live).
                        // `semgrep` under the sandbox exited 1 with NO report,
                        // NO stdout and NO stderr — it never started its
                        // interpreter — and this message told the user their
                        // findings had been lost, which sent them looking for a
                        // parser bug. A tool that produced nothing at all did
                        // not lose a report; it never made one, and its exit
                        // code is therefore not evidence of findings either.
                        let mut msg = if sarif.trim().is_empty()
                            && stdout.trim().is_empty()
                            && stderr.trim().is_empty()
                        {
                            format!(
                                "exit code {code_str} would mean findings, but the tool produced \
                                 NO output at all — no report, no diagnostics, nothing on either \
                                 stream. That is how a tool dies before it starts (a missing \
                                 runtime or interpreter, or an OS sandbox that does not grant \
                                 one), so the exit code is not evidence of findings"
                            )
                        } else if sarif.trim().is_empty() {
                            format!(
                                "exit code {code_str} reports findings, but the tool wrote no \
                                 report at all"
                            )
                        } else {
                            format!(
                                "exit code {code_str} reports findings, but the SARIF report was \
                                 unreadable — findings were lost"
                            )
                        };
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
fn parse_findings(
    key: &ToolKey,
    parser: AuditParser,
    output: &str,
    root: &Path,
) -> Vec<AuditFinding> {
    parser
        .parse(output, root)
        .into_iter()
        // Attribution is the KEY THAT RAN. A SARIF log names its own producer in
        // `runs[].tool.driver.name`, and that string is a claim by output cImp
        // is auditing — reading it here would let a plugin file its findings
        // under a built-in scanner's name.
        .map(|diag| AuditFinding {
            tool: key.clone(),
            diag,
        })
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

/// Recompute each built-in QUALITY tool's `enabled` flag to its automatic
/// value — the manifest's own default AND applicable to `census` — and return
/// the `(tool id, want)` pairs that differ from what is stored.
///
/// Reading the manifest's `enabled_by_default` rather than a second table is
/// what keeps the heavyweights (`dotnet-analyzers` runs a real build,
/// `semgrep-quality` fetches a network ruleset) opt-in: their definitions say
/// so, and auto-selection has no separate opinion to drift from it.
///
/// **Built-ins only.** Auto-selection is a statement about the roster cImp
/// ships and knows the shape of; a user plugin's tool is enabled because the
/// user enabled it, and a census walk is not an argument for turning somebody
/// else's tool off. Security tools are never in scope either — a security audit
/// must not become census-dependent.
///
/// Pure: the settings write and the auto/manual gate live in
/// [`AuditState::apply_quality_auto_select`].
pub(crate) fn auto_select_quality(
    tools: &[EffectiveTool],
    census: &census::Census,
) -> Vec<(String, bool)> {
    let mut out = Vec::new();
    for t in tools {
        if t.provenance != crate::plugins::manifest::Provenance::Builtin
            || audit_category(t) != Some(Category::Quality)
        {
            continue;
        }
        let Ok(Some(runnable)) = RunnableAudit::from_effective(t) else {
            continue;
        };
        let want = t.manifest.enabled_by_default && runnable.applicable(census);
        if t.tool_enabled != want {
            out.push((t.tool_id.clone(), want));
        }
    }
    out
}

/// Which umbrella a resolved tool fans out under, or `None` when it is not an
/// umbrella tool at all (a `check`/`command` kind, which `run_check` and
/// `run_command` own).
///
/// One function, so every audit-side reader asks the question the same way and
/// nobody re-derives "security kind means the Security umbrella" — the mapping
/// that carries the security floor.
fn audit_category(tool: &EffectiveTool) -> Option<Category> {
    match tool.kind() {
        crate::plugins::manifest::ToolKind::Security => Some(Category::Security),
        crate::plugins::manifest::ToolKind::Audit => Some(Category::Quality),
        _ => None,
    }
}

/// Every tool the registry knows about for this project — cImp's own embedded
/// definitions and whatever the user dropped in the plugins folder, joined with
/// the stored configuration.
///
/// Empty (never an error) when no store has been published: the headless
/// subcommands and most tests construct an `AuditState` without one. That is a
/// real degradation since Phase E — the fourteen built-ins live in the registry
/// now — so every path that must see them publishes a store, and the ones that
/// do not are paths where no scan runs.
fn registry_tools(
    cfg: &crate::settings::ToolPluginsSettings,
    root: &Path,
) -> Vec<EffectiveTool> {
    let Some(store) = crate::plugins::global() else {
        return Vec::new();
    };
    crate::plugins::registry::effective_tools(&store.snapshot(), cfg, Some(root))
}

/// Pure scan planning: from every registered tool, the target `category` and
/// the project `census`, produce `(chips, to_run)`.
///
/// * `chips` — one [`ToolState`] per tool of this category, in registry order
///   (cImp's own first): `idle` when disabled, `skipped-not-applicable` when
///   enabled but gated off by the census, `running` (about to resolve)
///   otherwise, and `failed` for one that belongs to an umbrella and cannot be
///   prepared at all.
/// * `to_run` — the subset actually launched.
///
/// # Why the two provenances are not treated identically here
///
/// A **built-in** appears whether or not it is enabled, because that is the
/// contract the Code Audit panel and the umbrella report have had since V23: a
/// disabled scanner is greyed out and counted as `disabled`, so the roster is
/// the same fourteen names every time and a user can see what they turned off.
/// A **user plugin's** tool appears only when it is runnable, because an
/// unconfigured one is configuration the Tool Plugins pane shows — a chip
/// promising a scan cImp has no binary for would be a worse answer than
/// silence.
///
/// Pure, so the filter is unit-testable without an `AppHandle`, a settings file
/// or a plugin store.
fn plan_scan(
    tools: &[EffectiveTool],
    category: Category,
    census: &census::Census,
) -> (Vec<ToolState>, Vec<RunnableAudit>) {
    let mut chips = Vec::new();
    let mut to_run = Vec::new();
    for tool in tools {
        let builtin = tool.provenance == crate::plugins::manifest::Provenance::Builtin;
        if !builtin && !tool.runnable() {
            continue;
        }
        match RunnableAudit::from_effective(tool) {
            Ok(Some(runnable)) => {
                // The RESOLVED tool's own category, not a second derivation of
                // it: `RunnableAudit` is what the runner spawns, so its answer
                // is the one that has to decide which umbrella this belongs to.
                if runnable.category != category {
                    continue;
                }
                if !tool.enabled {
                    // Built-in only (a disabled plugin tool never reaches here).
                    chips.push(ToolState::idle(runnable.key, category));
                } else if runnable.applicable(census) {
                    chips.push(ToolState::fresh(runnable.key.clone(), category));
                    to_run.push(runnable);
                } else {
                    chips.push(ToolState::skipped_not_applicable(
                        runnable.key.clone(),
                        category,
                    ));
                }
            }
            // A `check`/`command`-kind tool: `run_check` and `run_command` own
            // that population, and it never appears under an umbrella.
            Ok(None) => continue,
            // A tool that belongs to an umbrella and cannot run: a failed chip
            // with the reason, never a silent omission. A tool the user enabled
            // and pointed at a binary must not vanish from a report in silence.
            // Which umbrella is not knowable once resolution has failed, so it
            // is filed under the one being scanned — visible in the run the user
            // is looking at rather than in one they may never trigger.
            Err(why) if audit_category(tool) == Some(category) => {
                chips.push(ToolState::failed_to_plan(
                    ToolKey::of(tool),
                    category,
                    format!("this plugin tool cannot run: {why}"),
                ))
            }
            Err(_) => continue,
        }
    }
    (chips, to_run)
}

/// The PRE-SCAN roster for one category — what a scan would run right now, as
/// `idle` chips, before any scan has produced one.
///
/// The same population and the same two provenance rules as [`plan_scan`], with
/// one deliberate difference: a built-in is **not** gated on applicability here.
/// The panel has always rendered the full built-in roster and applied its own
/// language gate to it, and moving that decision server-side would change what a
/// cold tab shows before the first census walk. A plugin tool's applicability
/// lives in a manifest the frontend cannot read, so that half is decided here —
/// but only against a census that has actually been taken (`census_known`),
/// because an unwalked project is unknown, not empty.
fn plan_roster(
    tools: &[EffectiveTool],
    category: Category,
    census: &census::Census,
    census_known: bool,
) -> Vec<ToolState> {
    let mut out = Vec::new();
    for tool in tools {
        let builtin = tool.provenance == crate::plugins::manifest::Provenance::Builtin;
        if !builtin && !tool.runnable() {
            continue;
        }
        match RunnableAudit::from_effective(tool) {
            Ok(Some(runnable)) if runnable.category == category => {
                let gated = !builtin && census_known && !runnable.applicable(census);
                out.push(if gated {
                    ToolState::skipped_not_applicable(runnable.key, category)
                } else {
                    ToolState::idle(runnable.key, category)
                });
            }
            Ok(_) => continue,
            // Shown for the reason `plan_scan` shows it during a scan: a
            // configured capability must not be invisible.
            Err(why) if audit_category(tool) == Some(category) => {
                out.push(ToolState::failed_to_plan(
                    ToolKey::of(tool),
                    category,
                    format!("this plugin tool cannot run: {why}"),
                ))
            }
            Err(_) => continue,
        }
    }
    out
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
fn record_audit_run(name: &str, root: &Path, findings: usize, ms: u64, ok: bool) {
    let rec = ActivityRecord {
        entry: ActivityEntry::new(
            ActivityKind::Audit,
            now_ms(),
            activity::root_key(root),
            "audit".to_string(),
            name.to_string(),
            root.display().to_string(),
            findings,
            ms,
            ok,
            // cImp runs the scanner itself — no calling tab.
            activity::Attribution::Headless,
            None,
        ),
        request: format!("audit scan: {name}"),
        response: format!("{findings} findings"),
    };
    activity::record_bg(rec);
}

// ── child spawn + capture ──────────────────────────────────────────────────

/// How a captured child ended.
pub(super) enum Outcome {
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

/// Everything one spawn asks of the OS boundary, in one owned value.
///
/// V38 turned three separate "and also…" arguments into a struct because the
/// plugin population brought a fourth (the declared posture) and a fifth (the
/// declared runtime), and a nine-argument spawn helper whose last four are
/// booleans and vectors is how a call site ends up passing them in the wrong
/// order. [`Default`] is the built-in tier's answer to all of it, which is why
/// `run_one` can still say what it does in one line.
#[derive(Debug)]
struct SpawnPosture {
    /// Directories the child must be able to WRITE (the report scratch).
    full_dirs: Vec<PathBuf>,
    /// Reviewed grant rows — V38: a manifest's screened `extra_grants`.
    rows: Vec<crate::sandbox::GrantRow>,
    /// Which runtime profile's grants apply. Default = V33's inference.
    runtime: crate::sandbox::RuntimeSelect,
    /// What to do when the boundary cannot be provided. Default =
    /// [`SandboxReq::Optional`]: degrade loudly, exactly as V33 shipped.
    sandbox_req: SandboxReq,
}

impl Default for SpawnPosture {
    /// **Hand-written, and the `sandbox_req` line is why.** `SandboxReq`'s own
    /// default is `Required` — the right answer for a MANIFEST, where an author
    /// who has not thought about confinement gets the safe one. It is the wrong
    /// answer here: deriving `Default` would silently retro-fit "refuse to run
    /// unsandboxed" onto all fourteen built-in adapters, which would stop every
    /// audit scan on a machine with the sandbox switched off. The built-in tier
    /// declares nothing, and "declares nothing" has always meant `optional`.
    fn default() -> Self {
        Self {
            full_dirs: Vec::new(),
            rows: Vec::new(),
            runtime: crate::sandbox::RuntimeSelect::Infer,
            sandbox_req: SandboxReq::Optional,
        }
    }
}

/// Spawn `resolved` with `argv` (cwd = `root`, `env` forced, console-suppressed
/// on Windows), capturing stdout/stderr on their own tasks so a killed child
/// still yields what it printed. Honors the per-tool `timeout` and the scan
/// `cancel` token — both kill the child's whole process tree (see
/// [`kill_tree`]).
/// V33: `sandbox` decides whether the scanner runs inside the OS boundary;
/// `tool_name` labels this seam's `sandbox` Events rows (`audit:semgrep`), so
/// the lane distinguishes a scanner from a `run_command` or a `run_check`.
/// V38: `posture` carries what a plugin manifest DECLARED about the boundary —
/// see [`SpawnPosture`] and the three arms below.
#[allow(clippy::too_many_arguments)]
async fn spawn_and_capture(
    resolved: &Path,
    argv: &[String],
    env: &[(String, String)],
    root: &Path,
    timeout: Duration,
    cancel: &CancellationToken,
    sandbox: &crate::sandbox::SandboxCfg,
    tool_name: &str,
    posture: &SpawnPosture,
) -> Capture {
    let seam = crate::sandbox::audit_seam(tool_name);
    let subject = crate::sandbox::program_subject(resolved);

    // `unsupported`: the manifest says this tool cannot work inside the
    // boundary, so the boundary is not ATTEMPTED — running it and watching it
    // die is the mysterious failure this declaration exists to replace. An
    // informed choice, made visible: the ask is shown as a permission where the
    // tool is enabled, and the run mints a row once per session.
    //
    // Expressed as a PLAN rather than as a second spawn path: everything below
    // (the process group, the caps, the drains, the kill-tree, the Linux denial
    // classifier) is what running a scanner means, and a tool that declared
    // itself unsandboxable must still get all of it.
    //
    // …and a disabled config is how that is expressed to the layer below:
    // `plan` prepares NOTHING for it, so no ACE is stamped and no drive is
    // mapped for a tool that declared it cannot use either. Passing the real
    // config and discarding the plan would make the same run, plus durable
    // changes to the user's machine on a tool's behalf.
    let declared_unsupported = posture.sandbox_req == SandboxReq::Unsupported;
    let unsupported_cfg =
        crate::plugins::posture::unsupported_cfg(&seam, root, &subject, posture.sandbox_req);
    let sandbox = unsupported_cfg.as_ref().unwrap_or(sandbox);

    // Only composed when the sandbox is on — `plan` discards it otherwise, and
    // the plain path below keeps its historical inherit-and-force environment.
    let base_env = if sandbox.enabled {
        crate::sandbox::child_env::minimal_env(&|key| std::env::var_os(key))
    } else {
        Vec::new()
    };
    let plan = match tokio::time::timeout(
        crate::sandbox::PREPARE_BACKSTOP,
        crate::sandbox::plan(
            sandbox,
            &seam,
            resolved,
            &crate::sandbox::GrantHints {
                // Nothing to infer: cImp resolved the scanner binary itself and
                // `prepare` grants its install dir. A scanner that shells out to
                // a helper relies on an already-readable dir or on the user's
                // `extra_grant_dirs`.
                programs: Vec::new(),
                // …but a report-file tool must be able to WRITE its SARIF.
                full_dirs: posture.full_dirs.clone(),
                // V38: a plugin manifest's screened `extra_grants`. Empty for a
                // built-in adapter, whose only widening is the report directory
                // above.
                rows: posture.rows.clone(),
                // V33's inference for a built-in adapter (nobody declared a
                // runtime for these); a plugin tool's manifest selection
                // otherwise.
                runtime: posture.runtime.clone(),
            },
            root,
            &base_env,
        ),
    )
    .await
    {
        Ok(plan) => plan,
        Err(_) => {
            // Wedged BEFORE the spawn. Refuse rather than silently dropping the
            // boundary — a scan that quietly ran unsandboxed is worse than a
            // failed tool chip that says why.
            crate::sandbox::record_event(
                &seam,
                root,
                "wedged",
                crate::sandbox::state_target("wedged", &crate::sandbox::program_subject(resolved)),
                format!(
                    "sandbox preparation for `{tool_name}` did not settle within {}s \
                     (profile / ACL grants / drive mapping). The scanner was NOT run.",
                    crate::sandbox::PREPARE_BACKSTOP.as_secs(),
                ),
                false,
            );
            return Capture {
                stdout: String::new(),
                stdout_truncated: false,
                stderr: String::new(),
                outcome: Outcome::SpawnError(format!(
                    "sandbox preparation did not settle within {}s — treating as wedged \
                     (see the sandbox lane); `{tool_name}` was not run",
                    crate::sandbox::PREPARE_BACKSTOP.as_secs()
                )),
            };
        }
    };
    #[cfg(windows)]
    if let crate::sandbox::Plan::Sandboxed(prepared) = &plan {
        return spawn_sandboxed(
            prepared, resolved, argv, env, &base_env, root, timeout, cancel, sandbox, &seam,
        )
        .await;
    }
    if let crate::sandbox::Plan::Plain(reason) = &plan {
        // V38: `required` means never run unprotected — including when the
        // master switch is off, which is the case an author cannot see and a
        // user can. A manifest that says "this tool must be confined" is not
        // overridden by a global preference; the tool is simply missing from
        // this scan, loudly, in both the lane and its own chip.
        if let Some(refusal) = crate::plugins::posture::required_refusal(
            &seam,
            root,
            &subject,
            posture.sandbox_req,
            reason,
        ) {
            return Capture {
                stdout: String::new(),
                stdout_truncated: false,
                stderr: String::new(),
                outcome: Outcome::SpawnError(refusal),
            };
        }
        // Decision 5: degradation is loud, never silent — except where a more
        // specific row was already minted above, and "off (user choice)" would
        // be an outright wrong reason for a tool cImp deliberately did not try
        // to confine.
        if !declared_unsupported {
            crate::sandbox::record_skip(&seam, reason, &subject, root);
        }
    }

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
    // V33 C3: Unix-only — own process group, so the cancel/timeout `kill_tree`
    // below reaps semgrep's forked workers the same way `taskkill /T` does on
    // Windows. This is the seam the whole-tree kill was written for.
    crate::procutil::own_process_group(&mut cmd);

    // V33 Phase D — on Linux this IS the sandboxed path: Landlock is applied to
    // the scanner command built above. Locked decision L4 is enforced by
    // `apply`: the C2 minimal base, then the adapter's forced variables, then
    // the sandbox's redirections last. A failure REFUSES the scanner (reported
    // as a `SpawnError`, i.e. a failed tool chip that says why) rather than
    // running it with the boundary quietly missing (decision D3).
    #[cfg(target_os = "linux")]
    if let crate::sandbox::Plan::Sandboxed(prepared) = &plan {
        if let Err(e) = prepared.apply(&mut cmd, &base_env, env.iter().map(|(k, v)| (*k, *v))) {
            return Capture {
                stdout: String::new(),
                stdout_truncated: false,
                stderr: String::new(),
                outcome: Outcome::SpawnError(e),
            };
        }
    }

    // Through the spawn gate like every other cImp spawn — see `spawn_gate`.
    let mut child = match crate::spawn_gate::spawn_tokio(&mut cmd) {
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
    // The confirmation row, once per scanner per session.
    #[cfg(target_os = "linux")]
    if matches!(&plan, crate::sandbox::Plan::Sandboxed(_)) {
        crate::sandbox::record_sandboxed(
            &seam,
            root,
            &crate::sandbox::program_subject(resolved),
            sandbox,
        );
    }

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

    // V33 Phase D — the Linux denial row. Only for a scanner that actually ran
    // to completion: a cancel and a timeout are not access-denial signatures,
    // and `Outcome` is where that distinction already lives.
    #[cfg(target_os = "linux")]
    {
        // Only a scanner that actually RAN, inside the boundary, can have hit
        // it: a cancel, a timeout and a spawn failure are not access-denial
        // signatures, and `Outcome` is where that distinction already lives.
        let confined_exit = match &outcome {
            Outcome::Exited(code) => {
                matches!(&plan, crate::sandbox::Plan::Sandboxed(_)).then_some(*code)
            }
            _ => None,
        };
        let class = confined_exit
            .and_then(|code| crate::sandbox::denial_signature(code, &stderr, sandbox.allow_network));
        if let Some(class) = class {
            crate::sandbox::record_denial(
                &seam,
                root,
                &crate::sandbox::program_subject(resolved),
                argv,
                confined_exit.flatten(),
                &stderr,
                class,
                sandbox,
            );
        }
    }
    Capture {
        stdout,
        stdout_truncated,
        stderr,
        outcome,
    }
}

/// V33 — run one audit scanner INSIDE the AppContainer.
///
/// Mirrors the plain path's contract exactly ([`Capture`], the same
/// [`Outcome`] classification, the same per-tool timeout and output cap) and
/// differs only in the OS boundary and — per locked decision L4 — in the
/// environment: the C2 minimal base, then the adapter's forced variables, then
/// the sandbox's redirections last.
///
/// # Cancellation
///
/// The scan's [`CancellationToken`] is bridged onto a
/// [`crate::sandbox::CancelFlag`] the blocking Win32 wait loop polls. The
/// future is NEVER abandoned on cancel — dropping it would drop the caller's
/// `Prepared`, which unmaps the subst drive while the child is still alive.
/// Instead the flag is raised and the same future is awaited to completion,
/// which returns as soon as the engine has terminated the child and drained
/// its pipes.
#[cfg(windows)]
#[allow(clippy::too_many_arguments)]
async fn spawn_sandboxed(
    prepared: &crate::sandbox::windows::Prepared,
    resolved: &Path,
    argv: &[String],
    env: &[(String, String)],
    base_env: &[(&str, std::ffi::OsString)],
    root: &Path,
    timeout: Duration,
    cancel: &CancellationToken,
    sandbox: &crate::sandbox::SandboxCfg,
    seam: &str,
) -> Capture {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc as StdArc;

    let mut child_env = crate::sandbox::child_env::ChildEnv::from_base(base_env);
    child_env.overlay(env.iter().map(|(k, v)| (k.as_str(), v.as_str())));
    child_env.overlay(
        prepared
            .env_overrides
            .iter()
            .map(|(k, v)| (k.as_str(), v.clone())),
    );
    let child_env = child_env.into_pairs();

    let flag: crate::sandbox::CancelFlag = StdArc::new(AtomicBool::new(false));
    let cwd = prepared.cwd();
    let fut = tokio::time::timeout(
        crate::sandbox::backstop_for(timeout),
        crate::sandbox::windows::spawn_and_capture(
            prepared,
            crate::sandbox::windows::SpawnRequest {
                program: resolved,
                args: argv,
                // The adapter builds argv in code; CRT quoting is correct here.
                raw_tail: None,
                env: &child_env,
                cwd: &cwd,
                cap: MAX_OUTPUT_BYTES,
                timeout,
                cancel: Some(StdArc::clone(&flag)),
            },
        ),
    );
    tokio::pin!(fut);
    let settled = tokio::select! {
        res = &mut fut => res,
        _ = cancel.cancelled() => {
            flag.store(true, Ordering::SeqCst);
            // Keep awaiting the SAME future — see this function's doc.
            (&mut fut).await
        }
    };

    let run = match settled {
        Err(_) => {
            crate::sandbox::record_event(
                seam,
                root,
                "wedged",
                crate::sandbox::state_target("wedged", &crate::sandbox::program_subject(resolved)),
                format!(
                    "the sandboxed scanner did not settle within {}s (tool timeout {}s + {}s \
                     settle slack). The spawn helper never returned; the child may have run, may \
                     still be running, or may never have started — cImp cannot tell, so this row \
                     asserts only the wedge.",
                    crate::sandbox::backstop_for(timeout).as_secs(),
                    timeout.as_secs(),
                    crate::sandbox::SANDBOX_SETTLE_SLACK.as_secs(),
                ),
                false,
            );
            return Capture {
                stdout: String::new(),
                stdout_truncated: false,
                stderr: String::new(),
                outcome: Outcome::SpawnError(format!(
                    "the sandboxed scanner did not settle within {}s — treating as wedged \
                     (see the sandbox lane)",
                    crate::sandbox::backstop_for(timeout).as_secs()
                )),
            };
        }
        Ok(Ok(run)) => run,
        Ok(Err(e)) => {
            // Classified ⇒ a `denied` row; unclassified ⇒ a `refused` one. Both
            // are minted, because a scanner that never started is a fact about
            // the boundary whichever error code carried it — see
            // `sandbox::record_spawn_failure`.
            crate::sandbox::record_spawn_failure(
                seam,
                root,
                &crate::sandbox::program_subject(resolved),
                argv,
                &e,
                sandbox,
            );
            return Capture {
                stdout: String::new(),
                stdout_truncated: false,
                stderr: String::new(),
                outcome: Outcome::SpawnError(e),
            };
        }
    };
    crate::sandbox::record_sandboxed(
        seam,
        root,
        &crate::sandbox::program_subject(resolved),
        sandbox,
    );

    let stdout = String::from_utf8_lossy(&run.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&run.stderr).into_owned();
    // A cancel outranks a timeout: both terminate the child, but only one of
    // them is the user asking. `run.cancelled` is the engine's own answer, not
    // a re-read of the token, so a token cancelled a microsecond after the
    // child exited on its own is still reported as an exit.
    let outcome = if run.cancelled {
        Outcome::Cancelled
    } else if run.timed_out {
        Outcome::TimedOut
    } else {
        if let Some(class) =
            crate::sandbox::denial_signature(run.exit_code, &stderr, sandbox.allow_network)
        {
            crate::sandbox::record_denial(
                seam,
                root,
                &crate::sandbox::program_subject(resolved),
                argv,
                run.exit_code,
                &stderr,
                class,
                sandbox,
            );
        } else if run.exit_code != Some(0) && stdout.trim().is_empty() && stderr.trim().is_empty() {
            // The rc.9 `audit:semgrep` shape: exit 1, both streams empty, so
            // `denial_signature` had nothing to read and the lane stayed silent
            // while the scan reported "findings were lost". See
            // `sandbox::record_silent_exit`.
            crate::sandbox::record_silent_exit(
                seam,
                root,
                &crate::sandbox::program_subject(resolved),
                argv,
                run.exit_code,
                sandbox,
            );
        }
        Outcome::Exited(run.exit_code)
    };
    Capture {
        stdout,
        // A leaked drain means the capture is INCOMPLETE, which for a
        // stdout-transport scanner is exactly what `stdout_truncated` exists to
        // say: `finalize_outcome` then refuses to read the SARIF as whole
        // rather than reporting a clean bill from a half-read report.
        stdout_truncated: run.stdout_capped || run.drains_leaked,
        stderr,
        outcome,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::checks::Severity;

    /// Every built-in tool, resolved through the embedded manifest against an
    /// UNTOUCHED settings container — so each one is exactly what a fresh
    /// install gets.
    ///
    /// This is the fixture that replaced `adapters::adapter(AuditToolId::…)`.
    /// The difference is the point of Phase E: the facts these tests assert on
    /// (argv, exit codes, parser, applicability) used to be a `static` table and
    /// are now read from `plugins/builtin/cimp-audit.json` through the same
    /// registry a dropped-in plugin goes through.
    fn builtin_tools() -> Vec<EffectiveTool> {
        crate::plugins::registry::effective_tools(
            &crate::plugins::builtin::plugin_set(),
            &crate::settings::ToolPluginsSettings::default(),
            None,
        )
    }

    /// One built-in tool by its wire id, resolved and ready to run.
    fn builtin(id: &str) -> RunnableAudit {
        let tools = builtin_tools();
        let tool = tools
            .iter()
            .find(|t| t.tool_id == id)
            .unwrap_or_else(|| panic!("no built-in tool `{id}`"));
        RunnableAudit::from_effective(tool)
            .unwrap_or_else(|e| panic!("`{id}` is not runnable: {e}"))
            .unwrap_or_else(|| panic!("`{id}` is not an umbrella tool"))
    }

    /// The built-in roster with `enabled` overridden per tool — the fixture the
    /// planning tests use in place of the old `Vec<AuditToolConfig>`.
    fn builtin_tools_with(enables: &[(&str, bool)]) -> Vec<EffectiveTool> {
        let mut cfg = crate::settings::ToolPluginsSettings::default();
        let plugin = cfg
            .plugins
            .entry(crate::plugins::builtin::AUDIT_PLUGIN_KEY.to_string())
            .or_default();
        for (id, on) in enables {
            plugin.tools.entry((*id).to_string()).or_default().enabled = *on;
        }
        crate::plugins::registry::effective_tools(
            &crate::plugins::builtin::plugin_set(),
            &cfg,
            None,
        )
    }

    /// [`finalize`] for one built-in tool, reading its exit codes, parser and
    /// ingest gate from its manifest.
    #[allow(clippy::too_many_arguments)]
    fn finalize_builtin(
        id: &str,
        outcome: Outcome,
        sarif: &str,
        sarif_truncated: bool,
        stdout: &str,
        stderr: &str,
        root: &Path,
        timeout: Duration,
    ) -> (ToolStatus, Vec<AuditFinding>, Option<String>) {
        let tool = builtin(id);
        finalize(
            &Finalize {
                key: tool.key.clone(),
                findings_exit_codes: &tool.findings_exit_codes,
                parser: tool.parser,
                gate: tool.gate,
            },
            outcome,
            sarif,
            sarif_truncated,
            stdout,
            stderr,
            root,
            timeout,
        )
    }

    /// The security trio's parser — every fixture below is SARIF, and naming it
    /// once keeps the `parse_findings` call sites about the fixture.
    fn sarif_parser() -> AuditParser {
        builtin("osv-scanner").parser
    }

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
        let key = ToolKey::Builtin("osv-scanner".to_string());
        let f = parse_findings(&key, sarif_parser(), OSV_SARIF, &root());
        assert_eq!(f.len(), 1);
        assert_eq!(f[0].tool, key);
        assert_eq!(f[0].diag.code.as_deref(), Some("GHSA-r8w9-5wcg-vfj7"));
        assert_eq!(f[0].diag.severity, Severity::Warning);
        assert_eq!(f[0].diag.file, "Cargo.lock");
        assert_eq!(f[0].diag.line, 1);
    }

    #[test]
    fn gitleaks_sarif_fixture_relativizes_path() {
        let f = parse_findings(
            &ToolKey::Builtin("gitleaks".to_string()),
            sarif_parser(),
            GITLEAKS_SARIF,
            &root(),
        );
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
        let f = parse_findings(
            &ToolKey::Builtin("semgrep".to_string()),
            sarif_parser(),
            SEMGREP_SARIF,
            &root(),
        );
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
        let (status, findings, error) = finalize_builtin(
            "osv-scanner",
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
        let (status, findings, error) = finalize_builtin(
            "gitleaks",
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
    /// a clean "0 findings" pass — and the message must say WHICH of the three
    /// it was, because they send the reader to three different places.
    #[test]
    fn findings_exit_with_empty_sarif_is_failed() {
        for sarif in ["", "not json at all"] {
            let (status, findings, error) = finalize_builtin(
                "gitleaks",
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
            // The diagnostic tail rides along in every branch — it is the only
            // thing in the message that came from the tool itself.
            assert!(msg.contains("permission denied"), "{msg}");
            if sarif.is_empty() {
                // The tool talked (stderr) but wrote no report.
                assert!(msg.contains("wrote no report at all"), "{msg}");
                assert!(!msg.contains("NO output at all"), "{msg}");
            } else {
                assert!(msg.contains("unreadable — findings were lost"), "{msg}");
            }
        }
    }

    /// **The rc.9 `audit:semgrep` misread.** A sandboxed `semgrep.exe` that was
    /// granted its `Scripts` directory but not the Python install root behind it
    /// exited **1 with no report, no stdout and no stderr** — it never started
    /// its interpreter. Exit 1 is semgrep's findings code, so the runner said
    /// "the SARIF report was empty or unreadable — findings were lost", which
    /// describes a parser problem the user then went looking for.
    ///
    /// Nothing was lost: nothing was ever produced. A tool that emitted NOTHING
    /// on any channel did not run, and its exit code is not evidence of
    /// findings — the message has to say that, and name the shape (a runtime or
    /// interpreter the sandbox does not grant) that actually causes it.
    #[test]
    fn a_findings_exit_with_no_output_at_all_is_not_a_lost_report() {
        let (status, findings, error) = finalize_builtin(
            "semgrep",
            Outcome::Exited(Some(1)), // semgrep's findings code
            "",                       // no SARIF
            false,                    // not truncated — there was nothing to truncate
            "",                       // no stdout
            "",                       // no stderr either
            &root(),
            Duration::from_secs(600),
        );
        assert_eq!(status, ToolStatus::Failed);
        assert!(findings.is_empty());
        let msg = error.expect("a silent findings exit must explain itself");
        assert!(msg.contains("NO output at all"), "{msg}");
        assert!(
            msg.contains("not evidence of findings"),
            "the exit code must be disowned, not repeated as fact: {msg}"
        );
        assert!(
            msg.contains("interpreter") && msg.contains("sandbox"),
            "the message must name the shape that produces it: {msg}"
        );
        // …and it must NOT claim a report went missing.
        assert!(!msg.contains("findings were lost"), "{msg}");
    }

    /// A capped (known-incomplete) stdout SARIF is discarded as a failure even
    /// on a "clean" exit — a truncated document must not read as a clean bill.
    #[test]
    fn truncated_sarif_is_failed_not_clean() {
        for code in [0, 1] {
            let (status, findings, error) = finalize_builtin(
                "osv-scanner",
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
        let (status, findings, error) = finalize_builtin(
            "semgrep",
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
        let f = |o: Outcome| {
            finalize_builtin(
                "semgrep",
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
        let (status, findings, error) = finalize_builtin(
            "cppcheck",
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
    /// The [`AuditParser::EslintJson`](super::runnable::AuditParser) decoder runs
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
        let (status, findings, error) = finalize_builtin(
            "eslint",
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
        for id in ["cppcheck", "eslint", "oxlint"] {
            let (status, findings, error) = finalize_builtin(
                id,
                Outcome::Exited(Some(0)),
                "",
                false,
                "",
                "",
                &root(),
                Duration::from_secs(600),
            );
            assert_eq!(status, ToolStatus::Done, "{id}");
            assert!(findings.is_empty(), "{id}");
            assert!(error.is_none(), "{id}");
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

    /// A Quality scan never launches a Security tool, shows a disabled tool as
    /// `idle`, and reports an enabled-but-inapplicable tool `skipped-not-
    /// applicable`; only enabled + applicable tools land in `to_run`.
    #[test]
    fn plan_scan_quality_filters_category_disabled_and_applicability() {
        // A Rust + JS project: no `.py`, no `.go`.
        let census = census::Census::from_parts(&["ts", "rs"], &["Cargo.toml", "package.json"]);
        let tools = builtin_tools_with(&[("cargo-machete", false)]);
        let (chips, to_run) = plan_scan(&tools, Category::Quality, &census);

        // No security tool leaks into a quality scan.
        assert!(chips.iter().all(|c| c.category == Category::Quality));
        assert!(!chips.iter().any(|c| c.id.is_builtin("osv-scanner")));

        let status = |id: &str| {
            chips
                .iter()
                .find(|c| c.id.is_builtin(id))
                .unwrap_or_else(|| panic!("no chip for `{id}`"))
                .status
        };
        assert_eq!(status("oxlint"), ToolStatus::Running, "applicable (.ts)");
        assert_eq!(
            status("ruff"),
            ToolStatus::SkippedNotApplicable,
            "no .py in this project"
        );
        assert_eq!(status("cargo-machete"), ToolStatus::Idle, "disabled");
        assert_eq!(status("typos"), ToolStatus::Running, "always applicable");
        // A DISABLED built-in is still a chip — the pre-V38 contract the panel
        // greys out and the report counts as `disabled`.
        assert_eq!(
            chips.len(),
            11,
            "every quality built-in gets a chip, enabled or not"
        );

        // `to_run` is exactly the enabled + applicable set, in manifest order.
        let run: Vec<String> = to_run.iter().map(|t| t.key.wire()).collect();
        assert_eq!(run, vec!["oxlint", "typos", "knip"]);
    }

    /// A Security scan excludes every Quality tool; the always-applicable trio
    /// runs even against an empty census.
    #[test]
    fn plan_scan_security_excludes_quality_tools() {
        let census = census::Census::default();
        let (chips, to_run) = plan_scan(&builtin_tools(), Category::Security, &census);
        assert!(chips.iter().all(|c| c.category == Category::Security));
        assert!(!chips.iter().any(|c| c.id.is_builtin("oxlint")));
        let run: Vec<String> = to_run.iter().map(|t| t.key.wire()).collect();
        assert_eq!(run, vec!["osv-scanner", "gitleaks", "semgrep"]);
    }

    /// Quality auto-selection: each built-in quality tool's `enabled` becomes
    /// its manifest default AND census-applicable; the tools whose manifests say
    /// `enabled_by_default: false` stay opt-in even when applicable; security
    /// tools and user plugins are never touched; a second pass is a no-op.
    ///
    /// Driven from two extreme starting states rather than one, because the
    /// function returns only what CHANGES: from all-off, every "must be
    /// selected" claim is a real change to assert, and from all-on every "must
    /// be deselected" one is. A single starting state would leave half the rules
    /// asserted as `None`, which is also what a broken implementation returns.
    #[test]
    fn auto_select_quality_follows_census_and_keeps_heavyweights_opt_in() {
        // A Rust + TS project: no `.py` / `.go` / `.java` / C / eslint config.
        let census = census::Census::from_parts(&["ts", "rs"], &["Cargo.toml", "package.json"]);
        const QUALITY: &[&str] = &[
            "oxlint",
            "golangci-lint",
            "ruff",
            "cppcheck",
            "typos",
            "eslint",
            "pmd",
            "knip",
            "cargo-machete",
            "dotnet-analyzers",
            "semgrep-quality",
        ];
        let all = |on: bool| -> Vec<(&'static str, bool)> {
            QUALITY.iter().map(|id| (*id, on)).collect()
        };
        let decide = |state: &[(&'static str, bool)]| {
            let changes = auto_select_quality(&builtin_tools_with(state), &census);
            move |id: &str| changes.iter().find(|(k, _)| k == id).map(|(_, v)| *v)
        };

        // From ALL OFF: exactly the default-on, applicable tools turn on.
        let off = all(false);
        let from_off = decide(&off);
        for id in ["oxlint", "typos", "knip", "cargo-machete"] {
            assert_eq!(from_off(id), Some(true), "{id} applies to this project");
        }
        for id in ["ruff", "golangci-lint", "cppcheck", "pmd", "eslint"] {
            assert_eq!(from_off(id), None, "{id} does not apply and stays off");
        }
        // The two the MANIFEST marks opt-in stay off even though
        // `semgrep-quality` is ungated (always applicable) — this is the rule
        // that stops a first quality audit running a real .NET build or
        // fetching a ruleset over the network.
        for id in ["dotnet-analyzers", "semgrep-quality"] {
            assert_eq!(from_off(id), None, "{id} is opt-in by its manifest");
        }

        // From ALL ON: exactly the inapplicable and the opt-in tools turn off.
        let on = all(true);
        let from_on = decide(&on);
        for id in ["ruff", "golangci-lint", "cppcheck", "pmd", "eslint"] {
            assert_eq!(from_on(id), Some(false), "{id} does not apply here");
        }
        for id in ["dotnet-analyzers", "semgrep-quality"] {
            assert_eq!(from_on(id), Some(false), "{id} is opt-in by its manifest");
        }
        for id in ["oxlint", "typos", "knip", "cargo-machete"] {
            assert_eq!(from_on(id), None, "{id} is already at its automatic value");
        }
        // Security tools are out of scope entirely, from either direction — a
        // security audit must never become census-dependent.
        for id in ["osv-scanner", "gitleaks", "semgrep"] {
            assert_eq!(from_off(id), None, "{id} is a security tool");
            assert_eq!(from_on(id), None, "{id} is a security tool");
        }

        // Idempotent: applying what it asked for leaves nothing to decide.
        let mut applied = on.clone();
        for (id, v) in auto_select_quality(&builtin_tools_with(&on), &census) {
            let id = leaked(&id);
            match applied.iter_mut().find(|(k, _)| *k == id) {
                Some(slot) => slot.1 = v,
                None => applied.push((id, v)),
            }
        }
        assert!(auto_select_quality(&builtin_tools_with(&applied), &census).is_empty());
    }

    /// `builtin_tools_with` takes `&'static str` keys; these ids come from a
    /// manifest read at run time. Leaking a handful of short strings in a test
    /// process is the honest trade against threading a lifetime through a
    /// fixture helper.
    fn leaked(s: &str) -> &'static str {
        Box::leak(s.to_string().into_boxed_str())
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
        let mut ts = ToolState::fresh(ToolKey::Builtin("gitleaks".to_string()), Category::Security);
        ts.status = ToolStatus::Done;
        let one = || AuditFinding {
            tool: ToolKey::Builtin("gitleaks".to_string()),
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
            tools: vec![ToolState::fresh(ToolKey::Builtin("oxlint".to_string()), Category::Quality)],
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
                id: ToolKey::Builtin("gitleaks".to_string()),
                category: Category::Security,
                status: ToolStatus::Done,
                findings: vec![AuditFinding {
                    tool: ToolKey::Builtin("gitleaks".to_string()),
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


    // ── V38 Phase C: the plugin fan-out ────────────────────────────────────

    /// A plugin set built through the REAL loader, so every fixture below is a
    /// manifest that actually validates. Four tools: a security scanner, a
    /// quality one gated on Java, a `command` kind (Phase D's population) and
    /// one whose id collides with a built-in's name.
    fn plugin_fixture() -> (crate::plugins::PluginSet, PathBuf) {
        let dir = std::env::temp_dir().join(format!("cimp-fanout-{}", uuid::Uuid::new_v4()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");
        std::fs::write(
            dir.join("acme.json"),
            r#"{
              "manifest_version": 1,
              "name": "acme",
              "version": "1.0.0",
              "categories": [
                { "id": "sec", "label": "Security", "tools": ["scan", "gitleaks"] },
                { "id": "q", "label": "Quality", "tools": ["lint", "fmt"] }
              ],
              "tools": [
                { "id": "scan", "label": "Acme Scan", "kind": "security", "argv": ["{root}"] },
                { "id": "gitleaks", "label": "gitleaks", "kind": "security", "argv": ["{root}"] },
                { "id": "lint", "label": "Acme Lint", "kind": "audit", "argv": ["{root}"],
                  "applicability": { "extensions": ["java"], "markers": [] } },
                { "id": "fmt", "label": "Acme Format", "kind": "command" }
              ]
            }"#,
        )
        .expect("write manifest");
        let set = crate::plugins::loader::scan_dir(&dir, crate::plugins::manifest::Provenance::User);
        assert!(set.errors.is_empty(), "{:?}", set.errors);
        (set, dir)
    }

    /// The fixture plugin's tools, resolved with a path so they are runnable —
    /// **without** the built-in roster, for the tests that are about the plugin
    /// population alone.
    fn effective(set: &crate::plugins::PluginSet) -> Vec<crate::plugins::registry::EffectiveTool> {
        let mut cfg = crate::settings::ToolPluginsSettings::default();
        for id in ["scan", "gitleaks", "lint", "fmt"] {
            cfg.global_paths.insert(
                format!("acme@1.0.0/{id}"),
                "C:\\bin\\acme.exe".to_string(),
            );
        }
        crate::plugins::registry::effective_tools(set, &cfg, None)
    }

    /// The whole population the runner sees: cImp's own embedded definitions
    /// FIRST, then the fixture plugin's — the order `loader::scan_all`
    /// establishes and that the security floor rests on.
    fn effective_with_builtins(
        set: &crate::plugins::PluginSet,
    ) -> Vec<crate::plugins::registry::EffectiveTool> {
        let mut all = builtin_tools();
        all.extend(effective(set));
        all
    }

    /// **The chip residual Phase C left open, closed.** The PRE-SCAN roster
    /// carries the built-ins AND this project's runnable plugin tools, so a
    /// plugin tool the user enabled and pointed at a binary is visible before
    /// anything has run — not only once a scan starts.
    #[test]
    fn the_pre_scan_roster_shows_plugin_tools_beside_the_builtins() {
        let (set, dir) = plugin_fixture();
        let tools = effective_with_builtins(&set);

        let roster = plan_roster(&tools, Category::Security, &census::Census::default(), false);
        let ids: Vec<String> = roster.iter().map(|t| t.id.wire()).collect();
        // The built-in security trio, then the plugin tools of that category.
        assert!(ids.contains(&"gitleaks".to_string()));
        assert!(ids.contains(&"acme@1.0.0/scan".to_string()));
        assert!(ids.contains(&"acme@1.0.0/gitleaks".to_string()));
        assert!(
            roster.iter().all(|t| t.status == ToolStatus::Idle),
            "a pre-scan roster promises nothing about outcomes"
        );
        assert!(
            roster.iter().all(|t| t.category == Category::Security),
            "one panel, one category"
        );
        // A `command`-kind tool is never an umbrella chip (Phase D's other half).
        assert!(!ids.iter().any(|id| id.ends_with("/fmt")));

        // A plugin tool's applicability lives in a manifest the frontend cannot
        // read, so the BACKEND decides it — but only against a census that was
        // actually taken. Unknown census ⇒ shown; known-and-not-matching ⇒
        // already marked, so `partitionChips` hides it with no new rule.
        let quality = |census: &census::Census, known| {
            plan_roster(&tools, Category::Quality, census, known)
                .into_iter()
                .find(|t| t.id.wire() == "acme@1.0.0/lint")
                .expect("the java-gated tool is in the quality roster")
                .status
        };
        assert_eq!(
            quality(&census::Census::default(), false),
            ToolStatus::Idle,
            "an unwalked project is unknown, not empty"
        );
        assert_eq!(
            quality(&census::Census::from_parts(&["rs"], &[]), true),
            ToolStatus::SkippedNotApplicable
        );
        assert_eq!(
            quality(&census::Census::from_parts(&["java"], &[]), true),
            ToolStatus::Idle
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The built-in half of the roster is the pre-V38 contract, unchanged: every
    /// configured entry of the category, DISABLED ones included (the panel greys
    /// them), and untouched by whatever the plugin population does.
    #[test]
    fn the_roster_still_lists_disabled_builtins() {
        let all_off: Vec<(&str, bool)> = ["osv-scanner", "gitleaks", "semgrep"]
            .iter()
            .map(|id| (*id, false))
            .collect();
        let roster = plan_roster(
            &builtin_tools_with(&all_off),
            Category::Security,
            &census::Census::default(),
            true,
        );
        let ids: Vec<String> = roster.iter().map(|t| t.id.wire()).collect();
        for id in ["osv-scanner", "gitleaks", "semgrep"] {
            assert!(ids.contains(&id.to_string()), "{id} must still be listed");
        }
    }

    /// The fan-out rule: this category's kind, gated by the SAME census test a
    /// built-in gets, with `check`/`command` kinds left to Phase D.
    #[test]
    fn plan_scan_filters_a_plugin_by_kind_category_and_applicability() {
        let (set, dir) = plugin_fixture();
        let tools = effective(&set);

        let (chips, to_run) = plan_scan(&tools, Category::Security, &census::Census::default());
        let ids: Vec<String> = to_run.iter().map(|t| t.key.wire()).collect();
        assert_eq!(ids, vec!["acme@1.0.0/scan", "acme@1.0.0/gitleaks"]);
        assert!(
            chips.iter().all(|c| c.category == Category::Security),
            "a security fan-out must not carry another category's chips"
        );

        // Quality, empty census: the Java-gated tool is planned as a chip and
        // NOT launched — the built-in `skipped-not-applicable` state, reached by
        // the built-in rule.
        let (chips, to_run) = plan_scan(&tools, Category::Quality, &census::Census::default());
        assert!(to_run.is_empty(), "the java gate held");
        assert_eq!(chips.len(), 1);
        assert_eq!(chips[0].status, ToolStatus::SkippedNotApplicable);
        assert_eq!(chips[0].id.wire(), "acme@1.0.0/lint");

        // …and it runs once the project actually contains Java.
        let java = census::Census::from_parts(&["java"], &[]);
        let (_, to_run) = plan_scan(&tools, Category::Quality, &java);
        assert_eq!(to_run.len(), 1);

        // The `command`-kind tool never appears in either umbrella.
        for category in [Category::Security, Category::Quality] {
            let (chips, _) = plan_scan(&tools, category, &java);
            assert!(
                !chips.iter().any(|c| c.id.wire().ends_with("/fmt")),
                "a command-kind tool is Phase D's population, not an umbrella's"
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// **The security floor (invariant 2), generalized.** The built-in trio is
    /// part of a Security fan-out whatever the plugin population does, and the
    /// two rosters are computed independently — the built-in one FIRST, from
    /// settings alone. Keyed off `Provenance`, never off a name (R3).
    #[test]
    fn plugins_add_to_the_security_fanout_and_can_never_displace_a_builtin() {
        let (set, dir) = plugin_fixture();
        let census = census::Census::default();
        let all = effective_with_builtins(&set);

        let (chips, runs) = plan_scan(&all, Category::Security, &census);

        // The trio is in the fan-out, running, and it is FIRST — cImp's own
        // definitions are laid down before anything scanned from a folder.
        let run_ids: Vec<String> = runs.iter().map(|t| t.key.wire()).collect();
        assert_eq!(
            &run_ids[..3],
            &["osv-scanner", "gitleaks", "semgrep"],
            "the built-in security trio leads every security fan-out"
        );
        // A plugin can only ADD: every entry after the trio is user provenance,
        // and no plugin tool can present itself as a built-in. Keyed off
        // `Provenance` and the key namespaces, never off a name (R3).
        for t in &runs[3..] {
            assert!(
                matches!(t.key, ToolKey::Plugin(_)),
                "a scanned plugin must never land in the built-in namespace"
            );
            assert_eq!(t.provenance, crate::plugins::manifest::Provenance::User);
        }
        assert_eq!(runs.len(), 5, "three built-ins + the fixture's two");

        // Each built-in appears exactly once in the chip roster.
        for id in ["osv-scanner", "gitleaks", "semgrep"] {
            assert_eq!(
                chips.iter().filter(|c| c.id.is_builtin(id)).count(),
                1,
                "{id} appears exactly once"
            );
        }

        // …and disabling every built-in still cannot remove them from the
        // ROSTER (they become `idle` chips), which is what keeps a user who
        // switched one off able to see that they did.
        let mut cfg = crate::settings::ToolPluginsSettings::default();
        let plugin = cfg
            .plugins
            .entry(crate::plugins::builtin::AUDIT_PLUGIN_KEY.to_string())
            .or_default();
        for id in ["osv-scanner", "gitleaks", "semgrep"] {
            plugin.tools.entry(id.to_string()).or_default().enabled = false;
        }
        let off = crate::plugins::registry::effective_tools(
            &crate::plugins::builtin::plugin_set(),
            &cfg,
            None,
        );
        let (chips, runs) = plan_scan(&off, Category::Security, &census);
        assert!(runs.is_empty());
        assert_eq!(chips.len(), 3);
        assert!(chips.iter().all(|c| c.status == ToolStatus::Idle));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// **No shadowing at the fan-out.** A plugin tool whose id and label spell a
    /// built-in's name gets its own key and runs BESIDE it; nothing about the
    /// built-in changes, and the two are distinguishable in every consumer
    /// because attribution is the key.
    #[test]
    fn a_plugin_named_like_a_builtin_runs_beside_it_not_instead_of_it() {
        let (set, dir) = plugin_fixture();
        let tools = effective(&set);
        let census = census::Census::default();

        let (_, plugin_runs) = plan_scan(&tools, Category::Security, &census);
        let shadow = plugin_runs
            .iter()
            .find(|t| t.label == "gitleaks")
            .expect("the shadowing fixture");
        assert_eq!(shadow.key.wire(), "acme@1.0.0/gitleaks");
        assert!(!shadow.key.is_builtin("gitleaks"), "not the built-in");

        // Both populations produce findings, and each finding carries its own
        // key — a report reader can always tell which tool said what.
        let mine = parse_findings(&shadow.key, AuditParser::Sarif, GITLEAKS_SARIF, &root());
        let theirs = parse_findings(
            &ToolKey::Builtin("gitleaks".to_string()),
            sarif_parser(),
            GITLEAKS_SARIF,
            &root(),
        );
        assert_eq!(mine.len(), 1);
        assert_eq!(mine[0].tool.wire(), "acme@1.0.0/gitleaks");
        assert_eq!(theirs[0].tool.wire(), "gitleaks");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A manifest that cannot produce a runnable tool becomes a FAILED chip
    /// carrying the reason — never a silent omission from the roster.
    #[test]
    fn a_plugin_tool_that_cannot_run_is_a_failed_chip_not_a_silent_drop() {
        let dir = std::env::temp_dir().join(format!("cimp-badparser-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        // `parser` is validated against the findings namespace for USER plugins,
        // so this manifest is built as a builtin-provenance one — the only way
        // to reach the refusal, and exactly the shape Phase E will introduce.
        std::fs::write(
            dir.join("bad.json"),
            r#"{
              "manifest_version": 1,
              "name": "cimp-bad",
              "version": "1.0.0",
              "categories": [{ "id": "sec", "label": "Security", "tools": ["scan"] }],
              "tools": [{ "id": "scan", "label": "Bad", "kind": "security",
                          "argv": ["{root}"], "parser": "cargo-json" }]
            }"#,
        )
        .expect("write manifest");
        let set =
            crate::plugins::loader::scan_dir(&dir, crate::plugins::manifest::Provenance::Builtin);
        assert!(set.errors.is_empty(), "{:?}", set.errors);
        let mut cfg = crate::settings::ToolPluginsSettings::default();
        cfg.global_paths.insert(
            "cimp-bad@1.0.0/scan".to_string(),
            "C:\\bin\\bad.exe".to_string(),
        );
        let tools = crate::plugins::registry::runnable_tools(&set, &cfg, None);

        let (chips, to_run) =
            plan_scan(&tools, Category::Security, &census::Census::default());
        assert!(to_run.is_empty());
        assert_eq!(chips.len(), 1);
        assert_eq!(chips[0].status, ToolStatus::Failed);
        let err = chips[0].error.as_deref().unwrap_or_default();
        assert!(err.contains("cargo-json"), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ── V38 Phase C: ingest semantics ──────────────────────────────────────

    /// A plugin fixture's finalize context — SARIF, exit 1 means findings, and
    /// the envelope gate ON (which is what every user plugin gets).
    fn plugin_spec(key: &ToolKey) -> Finalize<'_> {
        Finalize {
            key: key.clone(),
            findings_exit_codes: &[1],
            parser: AuditParser::Sarif,
            gate: crate::audit::runnable::IngestGate::Sarif,
        }
    }

    /// **Empty is not absent.** The whole substantiveness matrix on the shared
    /// finalize path: `runs: []` on a clean exit is the ONLY empty-looking
    /// output that reads as a clean scan.
    #[test]
    fn a_plugin_tools_blank_output_is_an_error_not_a_clean_scan() {
        let key = ToolKey::Plugin("acme@1.0.0/scan".to_string());
        let spec = plugin_spec(&key);
        let fin = |sarif: &str, code: i32| {
            finalize(
                &spec,
                Outcome::Exited(Some(code)),
                sarif,
                false,
                "",
                "",
                &root(),
                Duration::from_secs(60),
            )
        };

        // A SARIF log with no results: ran, found nothing, clean.
        let (status, findings, error) = fin(r#"{"version":"2.1.0","runs":[]}"#, 0);
        assert_eq!(status, ToolStatus::Done);
        assert!(findings.is_empty() && error.is_none());

        // Nothing at all, on a CLEAN exit: a tool that said nothing is not a
        // tool that found nothing.
        let (status, _, error) = fin("", 0);
        assert_eq!(status, ToolStatus::Failed);
        assert!(error.unwrap().contains("no output at all"));

        // Parseable but not SARIF: zero findings out of a document cImp never
        // understood must not read as a clean bill.
        let (status, _, error) = fin("{}", 0);
        assert_eq!(status, ToolStatus::Failed);
        assert!(error.unwrap().contains("not a SARIF log"));

        // Not JSON at all (a usage message on stdout).
        let (status, _, error) = fin("usage: acme [options]", 0);
        assert_eq!(status, ToolStatus::Failed);
        assert!(error.unwrap().contains("not JSON"));

        // A real log with a result: findings, on either exit class.
        for code in [0, 1] {
            let (status, findings, error) = fin(GITLEAKS_SARIF, code);
            assert_eq!(status, ToolStatus::Done, "exit {code}");
            assert_eq!(findings.len(), 1);
            assert!(error.is_none());
        }

        // A findings exit code with an empty log keeps the built-in rule too —
        // the envelope fires first and says the more precise thing.
        let (status, _, error) = fin("", 1);
        assert_eq!(status, ToolStatus::Failed);
        assert!(error.unwrap().contains("no output at all"));
    }

    /// **Attribution is the registry entry that ran.** A hostile SARIF naming a
    /// built-in scanner as its own driver still files its findings under the
    /// plugin key — the tool name inside output is a claim by the thing being
    /// audited.
    #[test]
    fn findings_are_attributed_to_the_tool_that_ran_not_to_the_name_in_the_output() {
        let key = ToolKey::Plugin("acme@1.0.0/scan".to_string());
        let (status, findings, _) = finalize(
            &plugin_spec(&key),
            Outcome::Exited(Some(0)),
            // The driver claims to be gitleaks.
            GITLEAKS_SARIF,
            false,
            "",
            "",
            &root(),
            Duration::from_secs(60),
        );
        assert_eq!(status, ToolStatus::Done);
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].tool, key);
        assert!(
            !findings[0].tool.is_builtin("gitleaks"),
            "the embedded driver name must never become attribution"
        );
    }

    /// The built-in population's semantics are UNCHANGED by the envelope gate
    /// (R4): gitleaks writes no report at all on a clean run, and that is still
    /// a clean, zero-finding pass.
    #[test]
    fn the_envelope_gate_does_not_apply_to_the_builtin_tier() {
        let (status, findings, error) = finalize_builtin(
            "gitleaks",
            Outcome::Exited(Some(0)),
            "",
            false,
            "",
            "",
            &root(),
            Duration::from_secs(60),
        );
        assert_eq!(status, ToolStatus::Done);
        assert!(findings.is_empty() && error.is_none());
    }

    // ── V38 Phase C: the declared sandbox posture ──────────────────────────

    /// `SpawnPosture::default()` is the BUILT-IN tier's posture, and its
    /// `sandbox_req` must be `optional`.
    ///
    /// A derived `Default` would inherit `SandboxReq`'s own default — `required`,
    /// which is the right answer for a manifest and a catastrophic one here: it
    /// would refuse to run all fourteen built-in scanners on any machine with
    /// the sandbox switched off. This caught exactly that during Phase C.
    #[test]
    fn the_builtin_spawn_posture_declares_nothing_and_therefore_degrades() {
        let p = SpawnPosture::default();
        assert_eq!(p.sandbox_req, SandboxReq::Optional);
        assert_eq!(p.runtime, crate::sandbox::RuntimeSelect::Infer);
        assert!(p.rows.is_empty() && p.full_dirs.is_empty());
    }

    /// **`required` refuses even when the sandbox is globally OFF.** The
    /// manifest says this tool must never run unprotected, and a global
    /// preference does not overrule it — the tool is missing from the scan,
    /// with a reason, instead of running unconfined.
    #[tokio::test]
    async fn a_required_sandbox_refuses_to_run_when_sandboxing_is_off() {
        let (prog, argv) = sleeper();
        let cancel = CancellationToken::new();
        let started = Instant::now();
        let cap = spawn_and_capture(
            &prog,
            &argv,
            &[],
            &std::env::temp_dir(),
            Duration::from_secs(30),
            &cancel,
            &crate::sandbox::SandboxCfg::disabled(),
            "test-required",
            &SpawnPosture {
                sandbox_req: SandboxReq::Required,
                ..SpawnPosture::default()
            },
        )
        .await;
        // Nothing was spawned at all — the 30s sleeper never ran.
        assert!(started.elapsed() < Duration::from_secs(5));
        match cap.outcome {
            Outcome::SpawnError(e) => {
                assert!(e.contains("sandbox: required"), "{e}");
                assert!(e.contains("switched off"), "{e}");
            }
            _ => panic!("a `required` tool must not run unsandboxed"),
        }
    }

    /// `unsupported` is the opposite decision and must still RUN — outside the
    /// boundary, on purpose, with a row. Proven by the child actually starting
    /// (it is killed by the timeout it was given).
    #[tokio::test]
    async fn an_unsupported_sandbox_declaration_still_runs_the_tool() {
        let (prog, argv) = sleeper();
        let cancel = CancellationToken::new();
        let cap = spawn_and_capture(
            &prog,
            &argv,
            &[],
            &std::env::temp_dir(),
            Duration::from_millis(300),
            &cancel,
            &crate::sandbox::SandboxCfg::disabled(),
            "test-unsupported",
            &SpawnPosture {
                sandbox_req: SandboxReq::Unsupported,
                ..SpawnPosture::default()
            },
        )
        .await;
        assert!(
            matches!(cap.outcome, Outcome::TimedOut),
            "an `unsupported` tool runs; it is not refused"
        );
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
            // Deliberately UNsandboxed: this asserts the timeout/kill contract, and
            // routing it through the AppContainer would ACL-stamp the developer's
            // real toolchain dirs as a side effect of running the suite (the
            // `run_command` precedent).
            &crate::sandbox::SandboxCfg::disabled(),
            "test-sleeper",
            &SpawnPosture::default(),
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
            &crate::sandbox::SandboxCfg::disabled(),
            "test-sleeper",
            &SpawnPosture::default(),
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

    // ── V33 — the sandboxed audit seam ────────────────────────────────────

    /// The backstop relation for this seam. An audit tool's budget is the
    /// user's `code_audit.timeout_secs` (or a per-tool override), so the
    /// relation is asserted across the range rather than on one constant: if
    /// the caller-side deadline ever failed to outlast the child's, an
    /// ordinary slow semgrep run would be reported as a *wedge*, which is the
    /// one row in that lane that is supposed to mean cImp itself is broken.
    #[test]
    fn the_audit_sandbox_backstop_always_exceeds_the_tool_timeout() {
        for secs in [1u64, 60, 300, 1800, 7200] {
            let child = Duration::from_secs(secs);
            let backstop = crate::sandbox::backstop_for(child);
            assert!(
                backstop > child,
                "backstop {backstop:?} must exceed the tool timeout {child:?}"
            );
            assert_eq!(backstop, child + crate::sandbox::SANDBOX_SETTLE_SLACK);
        }
    }

    /// **The report directory is granted exactly when a tool writes one.**
    ///
    /// A `Transport::ReportFile` scanner (gitleaks, cppcheck, dotnet-analyzers)
    /// is handed an absolute SARIF path in its argv and writes there; without a
    /// write grant on that directory the sandbox turns three working tools into
    /// denial rows. A `Transport::Stdout` scanner writes nothing outside the
    /// already-granted project root and must get NO extra grant — every entry
    /// here widens the boundary.
    ///
    /// Pure-logic only: no ACL is stamped, and the grant is applied solely on
    /// the `Sandboxed` arm (`sandbox::plan` discards the hints when the switch
    /// is off, which `sandbox::tests::disabled_cfg_yields_off_user` pins).
    #[test]
    fn only_a_report_file_tool_gets_its_report_directory_granted() {
        let report = audit_report_dir().join("gitleaks-1234.sarif");

        // The write grant is the report path's OWN parent — derived from the
        // same value that goes into argv, so the granted directory and the
        // argument cannot drift apart.
        let granted = sandbox_full_dirs(Transport::ReportFile, Some(&report));
        assert_eq!(granted, vec![audit_report_dir()]);
        assert_eq!(
            granted[0],
            report.parent().unwrap(),
            "the granted dir must be the parent of the path handed to the scanner"
        );
        // It is cImp's own scratch, NOT the user's project tree.
        assert!(granted[0].starts_with(std::env::temp_dir()));
        assert!(granted[0].is_dir(), "the grant target must exist beforehand");

        // A stdout-transport tool gets nothing.
        assert!(sandbox_full_dirs(Transport::Stdout, Some(&report)).is_empty());
        assert!(sandbox_full_dirs(Transport::Stdout, None).is_empty());
        // …and neither does a report-file tool with no path (defensive: the
        // runner always pairs the two, and an empty grant is the safe answer).
        assert!(sandbox_full_dirs(Transport::ReportFile, None).is_empty());

        // Cross-check against the REAL roster — every one of the fourteen, not a
        // hand-listed sample that would rot the moment a tool changed transport
        // or a fifteenth arrived.
        let mut report_file_tools = 0usize;
        for tool in builtin_tools() {
            let tool = RunnableAudit::from_effective(&tool)
                .expect("runnable")
                .expect("an umbrella tool");
            let subject = tool.spawn_subject();
            let path = matches!(tool.transport, Transport::ReportFile)
                .then(|| temp_report_path(&subject));
            let dirs = sandbox_full_dirs(tool.transport, path.as_deref());
            assert_eq!(
                dirs.is_empty(),
                tool.transport == Transport::Stdout,
                "{subject} asks for the wrong grant for its transport"
            );
            // …and the granted directory really is where the ARGUMENT points.
            // The sandboxed path passes argv through `SpawnRequest::args` with
            // no `raw_tail`, so each element is CRT-quoted and the scanner's own
            // runtime parses the identical string back: the absolute report path
            // reaches the tool unmangled, spaces and all.
            if let Some(path) = &path {
                report_file_tools += 1;
                let argv = tool.full_argv(Path::new("/proj"), Some(path), true);
                let rendered = path.to_string_lossy().into_owned();
                assert!(
                    argv.iter().any(|a| a.contains(&rendered)),
                    "{subject}'s argv does not carry the report path it will be graded on: \
                     {argv:?}"
                );
                assert_eq!(dirs, vec![path.parent().unwrap().to_path_buf()]);
            }
        }
        assert_eq!(
            report_file_tools, 3,
            "gitleaks, cppcheck and dotnet-analyzers write to a report file; a change here is a \
             change to what the sandbox has to grant"
        );
    }

    /// An audit tool's `sandbox`-lane rows name the SCANNER, not just "an
    /// audit" — the lane is scanned by its source column, and `audit:semgrep`
    /// hitting the boundary is a different fact from `audit:gitleaks` doing so.
    /// Each label is also distinct from the other two seams, which is what
    /// keeps `run_command`'s and `run_check`'s rows apart from these.
    ///
    /// The label is a built-in's COMMAND name, so the Security `semgrep` and the
    /// Quality `semgrep-quality` share one — deliberately: it is the same binary
    /// under the same grants, and the boundary cannot tell them apart either.
    /// That is also why `spawn_subject` exists rather than the runner using the
    /// tool key: a user who greps `audit:semgrep` after this migration must
    /// still find their rows.
    #[test]
    fn audit_rows_name_the_scanner_and_not_just_the_seam() {
        let mut seen = std::collections::BTreeSet::new();
        for id in [
            "osv-scanner",
            "gitleaks",
            "semgrep",
            "eslint",
            "dotnet-analyzers",
        ] {
            let tool = builtin(id);
            let command = tool.spawn_subject();
            let seam = crate::sandbox::audit_seam(&command);
            assert!(
                seam.starts_with("audit:") && seam.contains(&command),
                "an audit seam label must name its scanner: {seam}"
            );
            assert_ne!(seam, crate::sandbox::SEAM_RUN_COMMAND);
            assert_ne!(seam, crate::sandbox::SEAM_RUN_CHECK);
            seen.insert(seam);
        }
        assert_eq!(seen.len(), 5, "distinct binaries must get distinct labels");
        // …and the documented exception: one binary, one label.
        assert_eq!(
            crate::sandbox::audit_seam(&builtin("semgrep").spawn_subject()),
            crate::sandbox::audit_seam(&builtin("semgrep-quality").spawn_subject()),
        );
        // The label really is the COMMAND, not the tool id — `dotnet-analyzers`
        // runs `dotnet`, and the lane says so.
        assert_eq!(builtin("dotnet-analyzers").spawn_subject(), "dotnet");
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
            &crate::sandbox::SandboxCfg::disabled(),
            "test-missing",
            &SpawnPosture::default(),
        )
        .await;
        assert!(matches!(cap.outcome, Outcome::SpawnError(_)));
    }

    // ── V30 Phase C: completion-push gate + payload ────────────────────────

    /// A finished snapshot with the given per-tool statuses.
    fn done_snapshot(statuses: &[(&str, ToolStatus)], total_findings: usize) -> AuditSnapshot {
        AuditSnapshot {
            root: "/proj/root".to_string(),
            scanning: false,
            last_scan_at: Some(1_700_000_000_000),
            tools: statuses
                .iter()
                .map(|(id, status)| ToolState {
                    status: *status,
                    ..ToolState::fresh(ToolKey::Builtin((*id).to_string()), Category::Security)
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
                ("gitleaks", ToolStatus::Done),
                ("osv-scanner", ToolStatus::Done),
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
                ("ruff", ToolStatus::Done),
                ("eslint", ToolStatus::Failed),
                ("pmd", ToolStatus::NotInstalled),
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
