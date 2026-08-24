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
use crate::offload::mcp_host::HostError;
use crate::offload::service::PushNotice;
use crate::plugins::manifest::{ProviderRef, SandboxReq};
use crate::plugins::registry::EffectiveTool;
use crate::settings::SettingsHandle;

use super::adapters::{self, Category, ExitClass, Transport};
use super::census;
use super::runnable::{AuditParser, RunnableAudit, ToolKey};

/// Tauri event emitted on every per-tool transition, carrying a (findings-
/// capped) [`AuditSnapshot`]. Phase C subscribes to this.
///
/// V42 F6 (#131): DEFINED FROM `service::events`, so the string is spelled
/// exactly once in the crate. The alias stays because this name is what the
/// module's own callers and tests read.
pub const AUDIT_STATUS_EVENT: &str = crate::service::events::AUDIT_STATUS;

/// Per-tool captured-output cap. SARIF for a large scan is sizable but bounded;
/// 16 MiB is generous headroom without letting a runaway tool exhaust memory.
// `pub(crate)` so `plugins::spec` can pin it against docs/TOOL-PLUGINS.md.
pub(crate) const MAX_OUTPUT_BYTES: usize = 16 * 1024 * 1024;

/// The event payload caps each tool's findings at this many; past it, the
/// event sets [`AuditSnapshot::truncated`] and the frontend pulls the full set
/// via `audit_snapshot`. The full snapshot IPC is never capped.
// `pub(crate)` so `plugins::spec` can pin it against docs/TOOL-PLUGINS.md.
pub(crate) const EVENT_FINDINGS_PER_TOOL_CAP: usize = 500;

/// V38 Phase F — the `scope` a tier-2 provider call is made under.
///
/// `scope` is the label the injection features and the SSRF flag rows resolve a
/// caller by, and every other producer passes a tab's scope or a worker task's.
/// An audit fan-out is neither, so it says what it is: one stable word, so a row
/// about a provider call is greppable and cannot be mistaken for a tab's.
const PROVIDER_SCOPE: &str = "audit";

/// What one tier-2 provider call came back as.
///
/// A local enum rather than reusing [`Outcome`]: that type is about a CHILD
/// PROCESS (it carries an exit code, a spawn error, a kill), and a provider call
/// has none of those facts. The two meet in [`finalize`], which is where the
/// shared half begins — see [`AuditState::run_one_provider`].
enum ProviderOutcome {
    /// The server answered. The text is what its `content` rendered to, which
    /// the ingest gate then judges: answering is not the same as saying
    /// something.
    Answered(String),
    /// The scan was cancelled while the call was in flight.
    Cancelled,
    /// The tool's wall-clock budget elapsed first — either the runner's own
    /// timer or the identical deadline the host call was given.
    TimedOut,
    /// The host refused the call because the USER has the server (or every
    /// category containing it) switched off. Not a failure: the configuration
    /// did exactly what it says, so this renders as a DISABLED tool, the same
    /// as a built-in scanner whose checkbox is unticked. The string is the
    /// host's refusal sentence, kept because it names WHICH toggle.
    RefusedDisabled(String),
    /// The server errored, the host refused it for any other reason (an
    /// ungranted consumer, the SSRF screen, a vanished tool), or there is no
    /// host in this process. Never a clean pass.
    Failed(String),
}

/// One tool's lifecycle within a scan. Serialized kebab-case, so the wire
/// strings are exactly `idle | running | done | failed | cancelled |
/// not-installed | path-invalid | skipped-not-applicable`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ToolStatus {
    /// Configured but not part of / not yet started in the current scan — and,
    /// since V38, the terminal state of a tier-2 provider tool whose MCP server
    /// (or its every category) the user has switched off. Both are the same
    /// fact from the reader's side: a tool that is switched off did not run and
    /// did not fail. Rendered and counted as `disabled` by
    /// [`format_result`](super::mcp::format_result).
    Idle,
    /// Resolved and its child is running.
    Running,
    /// Ran to completion (exit 0 = clean, or a findings exit code) — `findings`
    /// is authoritative even when empty.
    Done,
    /// A tool error: non-findings exit code, spawn failure, or timeout. A
    /// timeout stays here on purpose — the budget was the user's, but the tool
    /// failed to finish inside it, and its partial output is not a result.
    Failed,
    /// V38: the USER stopped the scan while this tool was still running.
    ///
    /// Its own status rather than [`Failed`](Self::Failed), because nothing
    /// went wrong: the chip, the umbrella report's tally and the `audit` lane
    /// row all used to say "failed" about an outcome the user asked for, which
    /// is indistinguishable from the scanner having crashed. A cancelled tool
    /// produced no findings, and the ones that had already finished keep
    /// theirs.
    Cancelled,
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
    /// Error detail when `status == failed` / `not-installed`, or (V38) the
    /// host's refusal sentence on an `idle` tier-2 provider whose server is
    /// switched off; `null` otherwise.
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
///   Still a `bool` from the scan's own token rather than a
///   [`ToolStatus::Cancelled`] chip in the snapshot: a stop that lands between
///   tools produces no such chip, and this gate must hold for that case too.
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
    /// V38 Phase F: the warm MCP host, for **tier-2 provider** tools. `None` in
    /// tests and in any standalone construction — a provider tool then fails
    /// with that as its reason, which is honest, rather than silently reporting
    /// a clean scan.
    ///
    /// The host, not the `OffloadService`: the runner needs exactly one thing
    /// from that layer (dispatch a `tools/call` under V37's enforcement) and
    /// holding the service would be an Arc cycle back into the thing that holds
    /// the push registry this struct already takes only the send half of.
    mcp: Option<Arc<crate::offload::mcp_host::McpHost>>,
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
        mcp: Option<Arc<crate::offload::mcp_host::McpHost>>,
    ) -> Arc<Self> {
        Arc::new(Self {
            app,
            settings,
            pushes,
            mcp,
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

    /// Whether the per-consumer expose toggle for `consumer` is on — a
    /// registered harness's token resolves to its own
    /// `harness[<id>].expose_code_audit` row, `"offload"` to
    /// `code_audit.expose_offload`, and anything else to **false**.
    ///
    /// The loopback `/audit/run` route re-checks this on every run so that
    /// unchecking an expose toggle takes effect for already-running tabs —
    /// advertisement is gated separately at spawn/injection time
    /// (`tabs::config`), and a child spawned while its consumer was opted in
    /// outlives that gate. The master `enabled` switch is enforced by
    /// [`begin_scan`](Self::begin_scan), not here.
    pub fn consumer_exposed(&self, consumer: &str) -> bool {
        let settings = self.settings.current();
        // V40 Phase A (locked decision 2): an UNRECOGNISED consumer is not
        // exposed. It used to fall through to `expose_claude`, so a caller
        // asserting any token at all was gated by a checkbox belonging to a
        // harness it is not — the fail-OPEN direction on a question about
        // reaching a scanner.
        //
        // V40 Phase B finished the job (locked decisions 5 and 25): the
        // `expose_claude` / `expose_opencode` half of the field TRIO is
        // `Settings::harness[<id>].expose_code_audit`, resolved through the
        // registry, so a third harness's checkbox is read the day it registers
        // instead of falling into the `_` arm. `expose_offload` keeps a field
        // of its own — the offload worker is cImp's own in-process consumer,
        // not a harness.
        if let Some(harness) = crate::harness::HarnessId::from_consumer(consumer) {
            return settings.harness_settings(harness).expose_code_audit;
        }
        consumer == "offload" && settings.code_audit.expose_offload
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
    /// On success returns `(to_run, ctx)` — the enabled+applicable subset to
    /// launch, plus the [`RunCtx`] every frame of the spawn chain runs under
    /// (the scan root, the resolved global wall-clock budget, this scan's cancel
    /// token, and the V33 OS-sandbox config) — leaving the runner in the
    /// `scanning` state with its chips already emitted. The caller's only
    /// remaining job is to drive `run(..)` (spawned or awaited) which clears
    /// `scanning` when it finishes.
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
    fn begin_scan(
        self: &Arc<Self>,
        category: Category,
    ) -> Result<(Vec<RunnableAudit>, RunCtx), String> {
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
            RunCtx {
                root,
                timeout: Duration::from_secs(global_timeout),
                cancel,
                sandbox,
            },
        ))
    }

    /// Begin a scan of `category` (V25 Phase C). Only tools of that category are
    /// considered; of those, only the `enabled && applicable(&census)` set is
    /// launched. Rejects (clear error) if a scan of *either* category is already
    /// in flight (one scan at a time globally) or no tool of this category is
    /// enabled. Returns immediately; work runs on a background task and streams
    /// progress via `audit-status`.
    pub fn start_scan(self: &Arc<Self>, category: Category) -> Result<(), String> {
        let (to_run, ctx) = self.begin_scan(category)?;
        let this = self.clone();
        tauri::async_runtime::spawn(async move {
            // V30 Phase C: `Initiator::Gui` — nobody is awaiting this scan, so
            // its completion is exactly the kind of fact the session-push bus
            // exists for.
            this.run(to_run, category, Initiator::Gui, ctx).await;
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
        let (to_run, ctx) = self.begin_scan(category)?;
        // V30 Phase C: `Initiator::Agent` — the snapshot returned below IS the
        // caller's tool result, so this path never pushes (it would duplicate
        // the report into the very session that asked for it).
        Ok(self
            .clone()
            .run(to_run, category, Initiator::Agent, ctx)
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
    async fn run(
        self: Arc<Self>,
        tools: Vec<RunnableAudit>,
        category: Category,
        initiator: Initiator,
        // V42 R26: the scan's own context, exactly as `begin_scan` resolved it.
        // `ctx.timeout` is still the GLOBAL budget here — the per-tool narrowing
        // is the `effective_tool_timeout` line below, and it happens once, in the
        // frame that builds each tool's own context.
        ctx: RunCtx,
    ) -> AuditSnapshot {
        // V30: wall clock for the completion push's duration floor. Started
        // here (not in `begin_scan`) so it measures the scan itself, not the
        // census walk and settings sync that precede it.
        let started = Instant::now();
        let git_repo = ctx.root.join(".git").exists();
        let mut handles = Vec::new();

        for tool in tools {
            // Per-tool timeout override (`None` = the global
            // `code_audit.timeout_secs`). A build-style tool
            // (`dotnet-analyzers`) wants a longer budget than a linter.
            let timeout = effective_tool_timeout(tool.timeout_secs, ctx.timeout);
            // V38 Phase F — tier 2: nothing to resolve, because nothing is
            // spawned. The branch is HERE rather than inside `run_one` because
            // resolution is the whole of what the two tiers do differently
            // before the call; everything after it (the ingest gate, the
            // attribution, the chip, the `audit` lane row) is shared.
            if let Some(provider) = tool.provider.clone() {
                let key = tool.key.clone();
                self.patch_tool(&key, |ts| {
                    ts.status = ToolStatus::Running;
                    // A provider tool has no on-disk program; the pane renders
                    // the server it calls instead (`provider` on the chip).
                    ts.resolved = None;
                });
                let this = self.clone();
                let cancel = ctx.cancel.clone();
                let root = ctx.root.clone();
                handles.push(tauri::async_runtime::spawn(async move {
                    this.run_one_provider(tool, provider, root, timeout, cancel)
                        .await;
                }));
                continue;
            }
            // ONE resolution rule, branching only where the two provenances
            // genuinely differ: cImp resolves a bare command name for a tool it
            // shipped, and never for one it did not (decision 7). See
            // [`resolve_runnable`].
            match resolve_runnable(&tool, &ctx.root) {
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
                    // This tool's own context: the scan's, with the global
                    // budget narrowed to the per-tool one resolved above.
                    let ctx = RunCtx {
                        timeout,
                        ..ctx.clone()
                    };
                    handles.push(tauri::async_runtime::spawn(async move {
                        this.run_one(tool, resolved, git_repo, ctx).await;
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
        // cancelled one. Still the TOKEN and not the snapshot, even though V38
        // gave a cancelled tool its own `ToolStatus::Cancelled`: that status
        // only appears on a tool that was mid-run when the stop landed, so a
        // scan cancelled between tools (or after the last one finished) leaves
        // a snapshot indistinguishable from a completed run. The token is the
        // only witness that covers every case.
        self.announce_scan_complete(
            &snap,
            category,
            initiator,
            ctx.cancel.is_cancelled(),
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

    /// V38 Phase F — run one **tier-2 provider** tool: one `tools/call` against
    /// the MCP server its manifest names, and its SARIF text through the same
    /// ingest gate a spawned tool's stdout goes through.
    ///
    /// # What is shared with tier 1, and why that is the point
    ///
    /// Everything after the bytes arrive: [`finalize`] applies the tool's
    /// [`IngestGate`](super::runnable::IngestGate) (always `Sarif` here — a
    /// provider tool's parser is forced at load), the findings are attributed to
    /// the REGISTRY KEY rather than to whatever `runs[].tool.driver.name` claims,
    /// the report caps apply, and the chip and the `audit` lane row are the same
    /// ones a spawned tool produces. A blank or non-SARIF answer is therefore a
    /// tool ERROR, not a clean scan, exactly as for tier 1.
    ///
    /// # What is different
    ///
    /// * **Nothing is spawned**, so there is no sandbox posture, no spawn ledger
    ///   row, no runtime canary and no wedged-child backstop. Validation refuses
    ///   the fields that would describe those things, so there is nothing to
    ///   ignore here.
    /// * **Nothing is passed.** The call carries an empty argument object: the
    ///   server scans what it is configured to scan. cImp's project root is not
    ///   sent, because a provider has no guarantee of sharing this machine's
    ///   filesystem and a path it cannot read is worse than no path at all.
    /// * **Enforcement is V37's.** The call goes through
    ///   [`call_recorded_with_deadline`](crate::offload::mcp_host::McpHost::call_recorded_with_deadline),
    ///   so a disabled server (or a disabled category) refuses it with the same
    ///   words any other proxied call gets, the SSRF screen runs, and the `mcp`
    ///   lane row is minted with the server, its category and this consumer.
    /// * **The tool's timeout really is the budget.** The deadline handed to the
    ///   host equals the outer timer, so a provider that legitimately scans for
    ///   minutes gets them. It is `..._with_deadline` for exactly that reason:
    ///   the plain entry point keeps the host's 45 s, which is right for a
    ///   model's turn and wrong for a repository scan.
    /// * **A toggled-off server is not a failure.** It comes back as
    ///   [`ProviderOutcome::RefusedDisabled`] and renders as a DISABLED tool.
    ///   Health is deliberately NOT consulted as a gate — dispatch is the truth,
    ///   and an "unhealthy" server that answers must not be refused by cImp.
    async fn run_one_provider(
        self: Arc<Self>,
        tool: RunnableAudit,
        provider: ProviderRef,
        root: PathBuf,
        timeout: Duration,
        cancel: CancellationToken,
    ) {
        use crate::offload::mcp_host::Consumer;
        use crate::offload::outbound;

        let started = Instant::now();
        // The routing name the host expects. Composed here, from the manifest's
        // two halves, rather than stored as one string: `<server>__<tool>` is
        // the host's convention, and this is the only place a manifest's words
        // are turned into it.
        let namespaced = format!("{}__{}", provider.server, provider.tool);

        let outcome = match self.mcp.clone() {
            None => ProviderOutcome::Failed(
                "this cImp process has no MCP host, so a provider-backed tool cannot be called \
                 (the offload runtime starts one whenever any MCP server is configured)"
                    .to_string(),
            ),
            Some(host) => {
                let snap = self.settings.current();
                // `AppWide` is the honest scope: an audit fan-out belongs to no
                // tab and is not the offload worker, and `AppWide`'s documented
                // meaning is "what the application is configured to do".
                let policy = outbound::Policy::from_settings(
                    &snap,
                    crate::settings::injection::Scope::AppWide,
                );
                // One ledger per tool run: the SSRF doubling counter is a
                // property of a caller's session, and one scan is this caller's
                // whole session.
                let ledger = outbound::TaskAudit::default();
                // `..._with_deadline`, and the deadline is the SAME `timeout`
                // the outer timer below uses. Without it the host applied its
                // own 45s `REQUEST_TIMEOUT` to every `tools/call`, so this
                // tool's configured budget was dead configuration for
                // providers and no scan slower than 45s could ever succeed.
                let call = host.call_recorded_with_deadline(
                    Consumer::Audit,
                    Some(root.as_path()),
                    &namespaced,
                    serde_json::json!({}),
                    PROVIDER_SCOPE,
                    // No tab: cImp runs the scan itself, the same reading
                    // `record_audit_run` takes a few lines below.
                    activity::Attribution::Headless,
                    &policy,
                    &ledger,
                    timeout,
                );
                tokio::select! {
                    // Cancel abandons the call cleanly: the future is dropped,
                    // which is all a cancel can mean when there is no child to
                    // kill. A request already in flight completes on the
                    // server's side and its answer is discarded — the same
                    // shape V37 documents for a toggle landing mid-call.
                    _ = cancel.cancelled() => ProviderOutcome::Cancelled,
                    // The outer timer stays the authoritative verdict; the inner
                    // deadline is its equal, so whichever fires first the answer
                    // is the same WORD. Which one wins is a race nobody should
                    // depend on, and neither may report "unreachable".
                    r = tokio::time::timeout(timeout, call) => match r {
                        Err(_) => ProviderOutcome::TimedOut,
                        Ok(answer) => provider_outcome(answer),
                    },
                }
            }
        };

        let duration_ms = started.elapsed().as_millis() as u64;
        let spec = Finalize {
            key: tool.key.clone(),
            findings_exit_codes: &tool.findings_exit_codes,
            parser: tool.parser,
            gate: tool.gate,
        };
        let (status, findings, error) =
            finalize_provider(&spec, &outcome, &provider, &root, timeout);

        let findings_count = findings.len();
        self.patch_tool(&tool.key, |ts| {
            ts.status = status;
            ts.findings = findings;
            ts.duration_ms = duration_ms;
            ts.error = error;
        });
        record_audit_run(
            &tool.key.wire(),
            &root,
            findings_count,
            duration_ms,
            status,
            matches!(outcome, ProviderOutcome::TimedOut),
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
    async fn run_one(
        self: Arc<Self>,
        tool: RunnableAudit,
        resolved: PathBuf,
        git_repo: bool,
        // V42 R26: this tool's context — the scan's, with `timeout` already
        // narrowed to this tool's budget by `run`.
        ctx: RunCtx,
    ) {
        let started = Instant::now();
        let root = ctx.root.as_path();
        let timeout = ctx.timeout;
        // What the sandbox lane and the activity row call this run. For a
        // built-in that is its command name (`audit:semgrep`), which is what
        // those rows have said since V33 and what a user grepping them expects;
        // for a plugin it is the namespaced key, which is the only name it has.
        let subject = tool.spawn_subject();
        let report_path = match tool.transport {
            Transport::ReportFile => Some(temp_report_path(&subject)),
            Transport::Stdout => None,
        };
        let argv = tool.full_argv(root, report_path.as_deref(), git_repo);

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
        let boundary_expected = ctx.sandbox.enabled && tool.sandbox != SandboxReq::Unsupported;
        let rows = crate::plugins::posture::screen_extra_grants(
            &seam,
            root,
            &tool.extra_grants,
            boundary_expected,
        );
        crate::plugins::posture::runtime_canary(&seam, root, &subject, &select, &resolved);

        let cap = spawn_and_capture(
            &resolved,
            &argv,
            &tool.env,
            &subject,
            &SpawnPosture {
                full_dirs,
                rows,
                runtime: select,
                sandbox_req: tool.sandbox,
            },
            &ctx,
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
        // Read before `cap.outcome` is moved into `finalize`: the activity row
        // needs the one fact the resulting `ToolStatus` deliberately does not
        // carry (a timeout and a crash are both `Failed`).
        let timed_out = matches!(cap.outcome, Outcome::TimedOut);
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
            root,
            timeout,
        );

        // Scan-coverage: the lockfiles/manifests osv-scanner reports scanning,
        // pulled from the same SARIF in a second best-effort pass. `osv-scanner`
        // only — its `runs[].artifacts` are the audit-only coverage signal, and
        // no other tool emits them.
        let scanned_artifacts = if tool.key.is_builtin("osv-scanner") && status == ToolStatus::Done
        {
            parsers::sarif_scanned_artifacts(&sarif, root)
        } else {
            Vec::new()
        };

        // Clean up the temp report regardless of outcome.
        if let Some(p) = &report_path {
            let _ = tokio::fs::remove_file(p).await;
        }

        let findings_count = findings.len();
        self.patch_tool(&tool.key, |ts| {
            ts.status = status;
            ts.findings = findings;
            ts.duration_ms = duration_ms;
            ts.error = error;
            ts.scanned_artifacts = scanned_artifacts;
        });

        record_audit_run(
            &subject,
            root,
            findings_count,
            duration_ms,
            status,
            timed_out,
        );
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
        // V38: `Cancelled`, not `Failed` — see `ToolStatus::Cancelled`. The
        // detail is unchanged, so every reader that showed it still does.
        Outcome::Cancelled => (
            ToolStatus::Cancelled,
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

/// V38 — what one host answer MEANS to the audit fan-out.
///
/// Pure, and split out of [`AuditState::run_one_provider`] because two of the
/// three error shapes are not tool failures and the difference is invisible in
/// the sentence: a deadline the user configured, and a server the user switched
/// off. Both used to render as `failed — … did not deliver findings: <prose>`,
/// which told the user something was broken when nothing was — the disabled one
/// most sharply, since it was their own toggle being reported as a fault.
///
/// The branch is on [`HostError`]'s CLASSIFICATION, never on its text: the text
/// is prose that gets reworded, and its remote half is prose a server cImp does
/// not control wrote. Anything unclassified stays a failure, so a new host error
/// site cannot accidentally become a clean or excused result.
fn provider_outcome(answer: Result<String, HostError>) -> ProviderOutcome {
    match answer {
        Ok(text) => ProviderOutcome::Answered(text),
        Err(e) if e.is_timeout() => ProviderOutcome::TimedOut,
        Err(e) if e.is_disabled_by_toggle() => ProviderOutcome::RefusedDisabled(e.to_string()),
        // `HostError`'s `Display` is bounded and carries the server's own
        // wording; the reader here is the Code Audit panel, which is a human.
        Err(e) => ProviderOutcome::Failed(e.to_string()),
    }
}

/// V38 Phase F — classify one tier-2 call's result, as a pure function.
///
/// Split out of [`AuditState::run_one_provider`] so the contract that matters
/// here — a remote answer goes through the SAME ingest gate and the SAME
/// attribution a spawned tool's stdout does, and no failure mode is a clean
/// pass — is assertable without a socket, a settings file or an `AppHandle`.
/// The `fake_server` pattern this file's neighbours use can only produce
/// "not connected"; everything interesting about a provider run is what happens
/// to the bytes after they arrive, and this is that.
fn finalize_provider(
    spec: &Finalize,
    outcome: &ProviderOutcome,
    provider: &ProviderRef,
    root: &Path,
    timeout: Duration,
) -> (ToolStatus, Vec<AuditFinding>, Option<String>) {
    match outcome {
        // Exit code 0 = `ExitClass::Clean`, which is what "the call returned"
        // means when there is no process to have had an exit code. The gate is
        // what decides whether the answer SAID anything.
        ProviderOutcome::Answered(text) => finalize(
            spec,
            Outcome::Exited(Some(0)),
            text,
            false,
            text,
            "",
            root,
            timeout,
        ),
        ProviderOutcome::Cancelled => {
            finalize(spec, Outcome::Cancelled, "", false, "", "", root, timeout)
        }
        ProviderOutcome::TimedOut => {
            finalize(spec, Outcome::TimedOut, "", false, "", "", root, timeout)
        }
        // The user's own switch, so the roster word is the one an unticked
        // built-in gets — `Idle` is what `audit::mcp` counts and renders as
        // "disabled". Counting it under "failed" told the user something was
        // broken when nothing was. The refusal sentence rides along as the
        // detail so the row still says which toggle, and no findings are
        // invented: a server that was never called found nothing.
        ProviderOutcome::RefusedDisabled(why) => {
            (ToolStatus::Idle, Vec::new(), Some(why.clone()))
        }
        // Never a clean pass. The reason is the server's or the host's, and it
        // names the server so the fix is findable: this failed chip is the only
        // place the fact is shown.
        ProviderOutcome::Failed(why) => (
            ToolStatus::Failed,
            Vec::new(),
            Some(format!(
                "the MCP provider `{}` (tool `{}`) did not deliver findings: {why}",
                provider.server, provider.tool
            )),
        ),
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
///
/// It governs BOTH tiers since V38: a spawned tool's child is killed at it, and
/// a tier-2 provider's `tools/call` is given it as the host deadline. (Until
/// that fix the provider half was capped at the host's 45 s regardless, which
/// made this number a lie for every provider tool.)
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

/// The word an `audit` lane row leads with, or `None` for a run that simply
/// succeeded (whose row keeps its pre-V38 shape).
///
/// `status` answers it for every case but one: a timeout and a crash are both
/// [`ToolStatus::Failed`] on purpose (the tool did not finish either way), and
/// those are two of the rows a reader most needs to tell apart afterwards — so
/// `timed_out` comes from the `Outcome` the caller still holds rather than
/// being guessed from the detail string.
///
/// Pure and total, so a status added later must be given a reading here rather
/// than silently inheriting one. `Running` and `Idle` are not terminal — a row
/// is only ever minted after a tool finished — but they are answered anyway:
/// a `None` for them would quietly claim success.
fn audit_row_outcome(status: ToolStatus, timed_out: bool) -> Option<&'static str> {
    match status {
        ToolStatus::Done => None,
        ToolStatus::Cancelled => Some("cancelled"),
        ToolStatus::Failed if timed_out => Some("timed out"),
        ToolStatus::Failed => Some("failed"),
        ToolStatus::NotInstalled => Some("not installed"),
        ToolStatus::PathInvalid => Some("misconfigured"),
        ToolStatus::SkippedNotApplicable => Some("not applicable"),
        ToolStatus::Idle => Some("disabled"),
        ToolStatus::Running => Some("running"),
    }
}

/// Record one tool run in the persistent tool-activity store (kind `audit`).
/// `chars` carries the finding count for audit entries.
///
/// V38: the terminal [`ToolStatus`] itself, not a pre-computed `ok` bool. The
/// `ok` column is derived from it here so a caller cannot disagree with the
/// chip it just wrote, and the row's `response` NAMES the outcome — a
/// cancelled run, a timed-out one and a crashed one were all `ok=false · 0
/// findings`, which is the same row for three different facts and no way to
/// tell them apart afterwards.
fn record_audit_run(
    name: &str,
    root: &Path,
    findings: usize,
    ms: u64,
    status: ToolStatus,
    timed_out: bool,
) {
    let ok = status == ToolStatus::Done;
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
            None,
            None,
        ),
        request: format!("audit scan: {name}"),
        // A successful row keeps its pre-V38 wording exactly; only the rows
        // that were previously indistinguishable gain the leading word.
        response: match audit_row_outcome(status, timed_out) {
            Some(word) => format!("{word} · {findings} findings"),
            None => format!("{findings} findings"),
        },
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

/// **What the scan gives every frame of the spawn chain**, in one owned value.
///
/// V42 R26: [`begin_scan`](AuditState::begin_scan) resolves these four together,
/// from ONE settings snapshot, and every frame below it needed all four —
/// [`run`](AuditState::run) → [`run_one`](AuditState::run_one) →
/// [`spawn_and_capture`] → [`spawn_sandboxed`] each took them as four separate
/// arguments and each carried an `#[allow(clippy::too_many_arguments)]` to say
/// so. The complement of [`SpawnPosture`]: that struct is what ONE TOOL asks of
/// the OS boundary, this is what THE SCAN hands to every tool.
///
/// Plumbing only — no field has a default, none may be omitted, and the values
/// are the same ones the frames already threaded. Two of them narrow as they go
/// down, both deliberately and both in exactly one place:
///
/// * `timeout` is the resolved GLOBAL budget at `run`, and `run` narrows it per
///   tool through [`effective_tool_timeout`] before building the frame below;
/// * `sandbox` is the scan's resolved config, and [`spawn_and_capture`] swaps in
///   the disabled twin for a tool whose manifest declared itself unsandboxable
///   (see `plugins::posture::unsupported_cfg`). Folding that swap into the
///   context is the point: the frame below must never be handed the declared
///   config while this one runs against the override.
#[derive(Clone)]
struct RunCtx {
    /// The scan root — the child's cwd, the sandbox's granted project dir, and
    /// the root every Events row is filed under.
    root: PathBuf,
    /// The wall-clock budget for this frame's work (see the narrowing note).
    timeout: Duration,
    /// This scan's cancel token. Both tiers honour it; the sandboxed path
    /// bridges it onto a [`crate::sandbox::CancelFlag`].
    cancel: CancellationToken,
    /// The V33 OS-sandbox config, resolved once per scan (see the narrowing
    /// note).
    sandbox: crate::sandbox::SandboxCfg,
}

impl RunCtx {
    /// This context with `sandbox` replaced — the ONE narrowing
    /// [`spawn_and_capture`] applies, kept as a named operation so the swap
    /// cannot be done by shadowing a local and then forgetting to pass it on.
    fn with_sandbox(&self, sandbox: crate::sandbox::SandboxCfg) -> Self {
        Self {
            sandbox,
            ..self.clone()
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
async fn spawn_and_capture(
    resolved: &Path,
    argv: &[String],
    env: &[(String, String)],
    tool_name: &str,
    posture: &SpawnPosture,
    // V42 R26: the scan root, this tool's timeout, the scan's cancel token and
    // the resolved sandbox config, threaded as one value — see [`RunCtx`].
    ctx: &RunCtx,
) -> Capture {
    let root = ctx.root.as_path();
    let timeout = ctx.timeout;
    let cancel = &ctx.cancel;
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
    // V42 R26: folded into the CONTEXT rather than shadowing a bare `sandbox`
    // argument, so `spawn_sandboxed` below cannot be handed the declared config
    // while this frame runs against the disabled twin. Everything else in the
    // context is unchanged by the swap.
    let overridden;
    let ctx = match crate::plugins::posture::unsupported_cfg(
        &seam,
        root,
        &subject,
        posture.sandbox_req,
    ) {
        Some(cfg) => {
            overridden = ctx.with_sandbox(cfg);
            &overridden
        }
        None => ctx,
    };
    let sandbox = &ctx.sandbox;

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
        return spawn_sandboxed(prepared, resolved, argv, env, &base_env, &seam, ctx).await;
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
        if let Err(e) = prepared.apply(&mut cmd, &base_env, env.iter().map(|(k, v)| (k.as_str(), v.as_str()))) {
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
async fn spawn_sandboxed(
    prepared: &crate::sandbox::windows::Prepared,
    resolved: &Path,
    argv: &[String],
    env: &[(String, String)],
    base_env: &[(&str, std::ffi::OsString)],
    seam: &str,
    // V42 R26: the caller's context, already carrying the EFFECTIVE sandbox
    // config for this tool (see [`RunCtx::with_sandbox`]).
    ctx: &RunCtx,
) -> Capture {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc as StdArc;

    let root = ctx.root.as_path();
    let timeout = ctx.timeout;
    let cancel = &ctx.cancel;
    let sandbox = &ctx.sandbox;

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

/// The module's unit tests, in a sibling file (#132): `runner.rs` was 2,581
/// production lines under 2,131 test lines, and the tests are unchanged by the
/// move — same crate, same module, same privacy.
#[cfg(test)]
#[path = "runner/tests.rs"]
mod tests;
