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
use crate::checks::{parsers, Diag, ParserKind};
use crate::settings::{AuditToolConfig, AuditToolId, SettingsHandle};

use super::adapters::{self, Adapter, ExitClass, Transport};

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
/// strings are exactly `idle | running | done | failed | not-installed`.
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
    /// The binary could not be resolved (ebin/PATH/override) — the scan
    /// proceeds with the remaining tools.
    NotInstalled,
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
    fn fresh(id: AuditToolId) -> Self {
        Self {
            id,
            status: ToolStatus::Running,
            findings: Vec::new(),
            duration_ms: 0,
            error: None,
            resolved: None,
            scanned_artifacts: Vec::new(),
        }
    }

    /// A configured-but-disabled tool: shown as an `idle` chip, never scanned.
    fn idle(id: AuditToolId) -> Self {
        Self {
            status: ToolStatus::Idle,
            ..Self::fresh(id)
        }
    }
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
    /// Per-tool state, in configured order (enabled tools only).
    pub tools: Vec<ToolState>,
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
            total_findings,
            truncated,
        }
    }
}

/// The managed audit runner. Constructed once in the Tauri setup hook and
/// `app.manage`d as `Arc<AuditState>`.
pub struct AuditState {
    app: AppHandle,
    settings: SettingsHandle,
    inner: StdMutex<Inner>,
}

impl AuditState {
    /// `root` is the launch project root (`launch_cwd`) — the directory every
    /// scan runs against.
    pub fn new(app: AppHandle, settings: SettingsHandle, root: PathBuf) -> Arc<Self> {
        Arc::new(Self {
            app,
            settings,
            inner: StdMutex::new(Inner {
                root,
                scanning: false,
                last_scan_at: None,
                tools: Vec::new(),
                cancel: None,
            }),
        })
    }

    /// The full (uncapped) snapshot for tab mount.
    pub fn snapshot(&self) -> AuditSnapshot {
        self.inner.lock().unwrap().snapshot(None)
    }

    /// Emit the current state as a (findings-capped) `audit-status` event.
    /// Built under the lock, emitted after dropping it (the graph-service
    /// discipline — a same-thread listener must not re-lock `inner`).
    fn emit_event(&self) {
        let snap = self.inner.lock().unwrap().snapshot(Some(EVENT_FINDINGS_PER_TOOL_CAP));
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

    /// Begin a scan. Rejects (clear error) if one is already in flight or no
    /// tool is enabled. Returns immediately; work runs on a background task and
    /// streams progress via `audit-status`.
    pub fn start_scan(self: &Arc<Self>) -> Result<(), String> {
        let cfg = self.settings.current().code_audit;
        // The master switch is enforced here, not just by tab visibility —
        // the IPC commands are registered unconditionally (the offload/graph
        // services gate the same way).
        if !cfg.enabled {
            return Err("Code Audit is disabled — enable it in cImp settings".to_string());
        }
        let enabled: Vec<AuditToolConfig> = cfg.tools.iter().filter(|t| t.enabled).cloned().collect();
        if enabled.is_empty() {
            return Err("no audit tools are enabled".to_string());
        }

        let (root, cancel) = {
            let mut inner = self.inner.lock().unwrap();
            if inner.scanning {
                return Err("a scan is already in progress".to_string());
            }
            inner.scanning = true;
            inner.last_scan_at = Some(now_ms());
            // Every configured tool gets a chip: enabled ones start `running`
            // (about to resolve), disabled ones sit at `idle` so the tab shows
            // the full tool list without the frontend re-reading settings.
            inner.tools = cfg
                .tools
                .iter()
                .map(|t| {
                    if t.enabled {
                        ToolState::fresh(t.id)
                    } else {
                        ToolState::idle(t.id)
                    }
                })
                .collect();
            let cancel = CancellationToken::new();
            inner.cancel = Some(cancel.clone());
            (inner.root.clone(), cancel)
        };
        self.emit_event();

        let this = self.clone();
        let timeout = Duration::from_secs(cfg.timeout_secs.max(1));
        tauri::async_runtime::spawn(async move {
            this.run(enabled, root, timeout, cancel).await;
        });
        Ok(())
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
    /// then clear the scanning flag.
    async fn run(
        self: Arc<Self>,
        tools: Vec<AuditToolConfig>,
        root: PathBuf,
        timeout: Duration,
        cancel: CancellationToken,
    ) {
        let git_repo = root.join(".git").exists();
        let mut handles = Vec::new();

        for tool in tools {
            // The same override contract Detect probes — the shared helper is
            // the single definition, so the two can't drift.
            let name = super::effective_command(tool.id, &tool.path);
            match crate::pty::resolve_command(&name) {
                Err(_) => {
                    self.patch_tool(tool.id, |ts| {
                        ts.status = ToolStatus::NotInstalled;
                        ts.error = Some("not found on PATH or ebin".to_string());
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
                        this.run_one(tool, resolved, root, git_repo, timeout, cancel).await;
                    }));
                }
            }
        }

        for h in handles {
            let _ = h.await;
        }

        {
            let mut inner = self.inner.lock().unwrap();
            inner.scanning = false;
            inner.cancel = None;
        }
        self.emit_event();
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
        let argv = adapter.full_argv(&root, report_path.as_deref(), git_repo, &tool.extra_args);

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
            tool.id, adapter, cap.outcome, &sarif, sarif_truncated, &cap.stdout, &cap.stderr,
            &root, timeout,
        );

        // Scan-coverage: the lockfiles/manifests osv-scanner reports scanning,
        // pulled from the same SARIF in a second best-effort pass (osv-scanner
        // only — its `runs[].artifacts` are the audit-only coverage signal).
        let scanned_artifacts = if tool.id == AuditToolId::OsvScanner && status == ToolStatus::Done {
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
    dir.join(format!("{}-{}.sarif", id.command_name(), uuid::Uuid::new_v4()))
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
/// `sarif` text has already been resolved from the adapter's transport by the
/// caller (empty for a killed child), and `sarif_truncated` says whether that
/// text is known-incomplete (stdout blew the capture cap / drain timed out).
/// This is the one place the audit runner's findings-vs-error exit semantics
/// are applied.
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
        Outcome::Cancelled => (ToolStatus::Failed, Vec::new(), Some("scan cancelled".to_string())),
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
                        let code_str =
                            code.map(|c| c.to_string()).unwrap_or_else(|| "unknown".to_string());
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

/// Parse a tool's SARIF into tagged findings (project-relative paths).
fn parse_findings(id: AuditToolId, sarif: &str, root: &Path) -> Vec<AuditFinding> {
    parsers::parse(ParserKind::Sarif, sarif, "", root, None)
        .into_iter()
        .map(|diag| AuditFinding { tool: id, diag })
        .collect()
}

/// A concise `failed` message for a tool-error exit, appending a short tail of
/// the tool's own diagnostics (stderr preferred, else stdout) so an offline /
/// misconfigured run surfaces the tool's reason, not a bare code.
fn exit_error_message(code: Option<i32>, stderr: &str, stdout: &str) -> String {
    let code_str = code.map(|c| c.to_string()).unwrap_or_else(|| "unknown".to_string());
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
    let detail = if stderr.trim().is_empty() { stdout.trim() } else { stderr.trim() };
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

    let out_task = tokio::spawn(crate::procutil::read_capped(child.stdout.take(), MAX_OUTPUT_BYTES));
    let err_task = tokio::spawn(crate::procutil::read_capped(child.stderr.take(), MAX_OUTPUT_BYTES));

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
    Capture { stdout, stdout_truncated, stderr, outcome }
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
        assert_eq!(a, vec!["Cargo.lock".to_string(), "package-lock.json".to_string()]);
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
                AuditToolId::Semgrep, a, o, "", false, "", "", &root(), Duration::from_secs(5),
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

    // ── snapshot wire cap ───────────────────────────────────────────────────

    #[test]
    fn event_snapshot_caps_findings_and_flags_truncated() {
        let mut ts = ToolState::fresh(AuditToolId::Gitleaks);
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
        ts.findings = (0..EVENT_FINDINGS_PER_TOOL_CAP + 10).map(|_| one()).collect();
        let inner = Inner {
            root: root(),
            scanning: false,
            last_scan_at: Some(123),
            tools: vec![ts],
            cancel: None,
        };
        // Full snapshot (IPC): everything, never truncated.
        let full = inner.snapshot(None);
        assert_eq!(full.total_findings, EVENT_FINDINGS_PER_TOOL_CAP + 10);
        assert!(!full.truncated);
        assert_eq!(full.tools[0].findings.len(), EVENT_FINDINGS_PER_TOOL_CAP + 10);
        // Event snapshot: capped, truncated flag set, total still true.
        let evt = inner.snapshot(Some(EVENT_FINDINGS_PER_TOOL_CAP));
        assert!(evt.truncated);
        assert_eq!(evt.tools[0].findings.len(), EVENT_FINDINGS_PER_TOOL_CAP);
        assert_eq!(evt.total_findings, EVENT_FINDINGS_PER_TOOL_CAP + 10);
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
        assert_keys(&serde_json::to_value(&snap).expect("snapshot serializes"), AUDIT_RUNTIME_TS);
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
        ];
        fn _statuses_exhaustive(s: ToolStatus) {
            match s {
                ToolStatus::Idle
                | ToolStatus::Running
                | ToolStatus::Done
                | ToolStatus::Failed
                | ToolStatus::NotInstalled => {}
            }
        }
        for s in statuses {
            let wire = serde_json::to_value(s).unwrap().as_str().unwrap().to_string();
            assert!(
                AUDIT_RUNTIME_TS.contains(&format!("'{wire}'")),
                "ToolStatus wire `{wire}` is missing from the TS `AuditToolStatus` union",
            );
        }

        let severities = [Severity::Error, Severity::Warning, Severity::Note];
        fn _severities_exhaustive(s: Severity) {
            match s {
                Severity::Error | Severity::Warning | Severity::Note => {}
            }
        }
        for sev in severities {
            let wire = serde_json::to_value(sev).unwrap().as_str().unwrap().to_string();
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
        assert!(started.elapsed() < Duration::from_secs(10), "child was not killed on timeout");
        assert!(matches!(cap.outcome, Outcome::TimedOut), "expected TimedOut");
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
        assert!(started.elapsed() < Duration::from_secs(10), "child was not killed on cancel");
        assert!(matches!(cap.outcome, Outcome::Cancelled), "expected Cancelled");
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
}
