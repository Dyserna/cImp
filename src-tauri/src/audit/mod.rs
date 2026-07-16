//! V23 Code Audit — aggregated security scanning.
//!
//! Phase A ships only tool *detection*: [`audit_detect_tool`] resolves a
//! configured audit tool via [`crate::pty::resolve_command`] (honoring the
//! per-tool `path` override in [`crate::settings::AuditToolConfig`]) and probes
//! `<tool> --version`. The result is display-only — the Settings "Detect" button
//! shows it inline and never writes it back into the stored `path` field, so the
//! config stays "resolve normally" unless the user browses to an exe.
//!
//! Phase B extends this module with the full concurrent runner, per-tool SARIF
//! adapters, and the `audit-status` progress events.

pub mod adapters;
pub mod census;
pub mod mcp;
pub mod parsers;
pub mod runner;

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use tauri::State;

use crate::error::{AppError, AppResult};
use crate::ipc::AppState;
use crate::settings::AuditToolId;

pub use runner::{AuditSnapshot, AuditState};

/// V26 — process-global handle to the managed [`AuditState`], set once in
/// `main.rs` right where the state is constructed. The audit runner is a Tauri
/// `manage`d singleton reachable from IPC via `State<Arc<AuditState>>`, but the
/// **offload worker's native audit tools** (Stage 4) run in-process *outside*
/// any Tauri command context and so can't reach the managed state that way —
/// exactly the seam `crate::graph::offload_query` provides for the graph. This
/// [`OnceLock`] is that seam for audits: `set_global` at startup, `global()`
/// from the worker.
static GLOBAL: OnceLock<Arc<AuditState>> = OnceLock::new();

/// Publish the managed [`AuditState`] as the process global. Idempotent — a
/// second call is ignored (there is only ever one runner per launch), so a
/// stray double-set can never swap the live state out from under a consumer.
pub fn set_global(state: Arc<AuditState>) {
    let _ = GLOBAL.set(state);
}

/// The process-global [`AuditState`], or `None` before `main.rs` has set it
/// (e.g. a headless subcommand that never builds the runner). The offload
/// worker's native audit tools (Stage 4) reach the runner through this.
pub fn global() -> Option<Arc<AuditState>> {
    GLOBAL.get().cloned()
}

/// How long the `--version` probe may run before it's abandoned as
/// "not found / unresponsive". Short — this backs an interactive button.
const DETECT_TIMEOUT: Duration = Duration::from_secs(8);

/// V23 Phase A: result of a Detect probe. Display-only — the frontend renders it
/// inline (`✓ <version> — <path>` / the error) and NEVER writes it back into the
/// tool's `path` field.
#[derive(Debug, Clone, serde::Serialize)]
pub struct AuditDetectResult {
    /// Whether the tool resolved AND its `--version` probe ran successfully.
    pub found: bool,
    /// The resolved path — an ebin/PATH hit, or the verbatim override. Present
    /// even on a probe failure once the binary itself resolved.
    pub path: Option<String>,
    /// The tool's reported version line, when found and parseable.
    pub version: Option<String>,
    /// Why detection failed (not on PATH/ebin, spawn error, timeout), when not
    /// found.
    pub error: Option<String>,
}

/// V23 Phase A: resolve one audit tool honoring the per-tool `path` override and
/// probe `<tool> --version`. Read-only and side-effect-free w.r.t. settings —
/// the Detect button shows the result inline but never mutates the stored path.
/// Always returns `Ok`: a not-found tool is a normal result (`found = false`),
/// not an error.
///
/// `path` is the LIVE override from the Settings input (empty = resolve the
/// bare command name). The frontend passes it explicitly so a just-typed value
/// can't race the fire-and-forget `settings_update` push; `None` falls back to
/// the persisted setting.
#[tauri::command]
pub async fn audit_detect_tool(
    state: State<'_, AppState>,
    id: AuditToolId,
    path: Option<String>,
) -> AppResult<AuditDetectResult> {
    let override_path = match path {
        Some(p) => p,
        // The persisted per-tool override (empty = resolve ebin → PATH).
        None => state
            .settings
            .current()
            .code_audit
            .tools
            .iter()
            .find(|t| t.id == id)
            .map(|t| t.path.clone())
            .unwrap_or_default(),
    };
    // The launch project root scopes project-local `node_modules/.bin`
    // resolution (eslint/knip) so a Detect ✓ matches what a scan will launch.
    let root = state.launch.cwd.clone();
    Ok(detect_tool(id, &override_path, Some(&root)).await)
}

/// The command to resolve for a tool: the per-tool `path` override verbatim
/// (trimmed), or the bare command name when the override is empty. The single
/// definition of the override contract — the Detect probe AND the scan runner
/// both go through [`resolve_audit_binary`], so a ✓ from Detect can't disagree
/// with what a scan actually launches.
fn effective_command(id: AuditToolId, override_path: &str) -> String {
    let p = override_path.trim();
    if p.is_empty() {
        id.command_name().to_string()
    } else {
        p.to_string()
    }
}

/// Resolve a tool's binary to an on-disk path. Resolution order (V25 Phase B):
///
/// 1. **Per-tool `path` override** (non-empty) — used verbatim (the V23
///    contract): a deliberate "use exactly this binary" that project-local
///    resolution must not second-guess.
/// 2. **Project-local `node_modules/.bin`** — for a node tool that declares
///    [`Adapter::project_local_bin`] (eslint, knip), the project's own install
///    beats a global one. Only consulted when there is no override and a `root`
///    is known.
/// 3. **ebin → PATH** on the bare [`AuditToolId::command_name`].
///
/// Both the Detect probe and the scan runner go through here, so the two agree.
/// `root` is the scan/launch project root (`None` when unavailable — e.g. a
/// unit test — collapses to override-then-ebin/PATH).
fn resolve_audit_binary(
    id: AuditToolId,
    override_path: &str,
    root: Option<&Path>,
) -> AppResult<PathBuf> {
    if override_path.trim().is_empty() {
        if let (Some(bin), Some(root)) = (adapters::adapter(id).project_local_bin, root) {
            if let Some(local) = resolve_project_local_bin(root, bin) {
                return Ok(local);
            }
        }
    }
    crate::pty::resolve_command(&effective_command(id, override_path))
}

/// The first existing `node_modules/.bin/<name>` shim under `root`, or `None`.
/// On Windows the runnable shim is the `.cmd`/`.CMD`/`.bat` launcher (the bare
/// `<name>` is a POSIX shell script npm also drops that Windows can't spawn), so
/// those are tried first — mirroring `pty::resolve`'s Windows extension order.
fn resolve_project_local_bin(root: &Path, name: &str) -> Option<PathBuf> {
    let bin_dir = root.join("node_modules").join(".bin");
    for candidate in project_local_candidates(name) {
        let p = bin_dir.join(&candidate);
        if p.is_file() {
            return Some(p);
        }
    }
    None
}

/// Filenames to try for a project-local node bin shim.
fn project_local_candidates(name: &str) -> Vec<String> {
    #[cfg(windows)]
    {
        vec![
            format!("{name}.cmd"),
            format!("{name}.CMD"),
            format!("{name}.bat"),
            name.to_string(),
        ]
    }
    #[cfg(not(windows))]
    {
        vec![name.to_string()]
    }
}

/// The detection core, split out so tests can exercise it without a Tauri
/// `State`. `override_path` is the tool's configured `path` (empty = resolve via
/// project-local `node_modules/.bin` then ebin → PATH; non-empty = used
/// verbatim). `root` scopes the project-local lookup (`None` = skip it).
async fn detect_tool(
    id: AuditToolId,
    override_path: &str,
    root: Option<&Path>,
) -> AuditDetectResult {
    let resolved = match resolve_audit_binary(id, override_path, root) {
        Ok(p) => p,
        Err(_) => {
            return AuditDetectResult {
                found: false,
                path: None,
                version: None,
                error: Some("not found on PATH or ebin".to_string()),
            };
        }
    };
    let path_str = resolved.display().to_string();

    let mut cmd = tokio::process::Command::new(&resolved);
    cmd.arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    // Don't flash a console window for the probe on Windows.
    #[cfg(windows)]
    cmd.creation_flags(crate::procutil::CREATE_NO_WINDOW);

    let child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            return AuditDetectResult {
                found: false,
                path: Some(path_str),
                version: None,
                error: Some(format!("failed to run --version: {e}")),
            };
        }
    };
    // Backstop reaper if cImp dies hard mid-probe before kill_on_drop fires —
    // the same job-object guard every other spawn site applies.
    crate::process_guard::guard_child(&child);

    match tokio::time::timeout(DETECT_TIMEOUT, child.wait_with_output()).await {
        // A binary that runs but rejects `--version` (wrong exe, or a build
        // wanting a `version` subcommand) is NOT "found" — otherwise its usage/
        // error line would be presented as a version string.
        Ok(Ok(output)) if !output.status.success() => AuditDetectResult {
            found: false,
            path: Some(path_str),
            version: None,
            error: Some(format!(
                "--version probe exited with code {}{}",
                output
                    .status
                    .code()
                    .map(|c| c.to_string())
                    .unwrap_or_else(|| "unknown".into()),
                parse_version(&output.stderr, &output.stdout)
                    .map(|l| format!(": {l}"))
                    .unwrap_or_default()
            )),
        },
        Ok(Ok(output)) => AuditDetectResult {
            found: true,
            path: Some(path_str),
            version: parse_version(&output.stdout, &output.stderr),
            error: None,
        },
        Ok(Err(e)) => AuditDetectResult {
            found: false,
            path: Some(path_str),
            version: None,
            error: Some(format!("failed to run --version: {e}")),
        },
        Err(_) => AuditDetectResult {
            found: false,
            path: Some(path_str),
            version: None,
            error: Some(format!("timed out after {}s", DETECT_TIMEOUT.as_secs())),
        },
    }
}

/// Best-effort version string: the first non-empty trimmed line of stdout,
/// falling back to stderr (some tools print `--version` to stderr).
fn parse_version(stdout: &[u8], stderr: &[u8]) -> Option<String> {
    fn first_line(bytes: &[u8]) -> Option<String> {
        String::from_utf8_lossy(bytes)
            .lines()
            .map(str::trim)
            .find(|l| !l.is_empty())
            .map(str::to_string)
    }
    first_line(stdout).or_else(|| first_line(stderr))
}

/// V23 Phase B / V25 Phase C: start a scan of the project root with the enabled +
/// applicable + resolvable tools of `category` (`"security"` / `"quality"`),
/// concurrently. Returns immediately; progress streams via the
/// [`AUDIT_STATUS_EVENT`] event and can be re-fetched with [`audit_snapshot`].
/// Rejected (a typed error the UI surfaces) when a scan is already in flight
/// (one at a time globally, either category) or no tool of `category` is enabled.
#[tauri::command]
pub async fn audit_start_scan(
    state: State<'_, Arc<AuditState>>,
    category: adapters::Category,
) -> AppResult<()> {
    state.inner().start_scan(category).map_err(AppError::Audit)
}

/// V23 Phase B: cancel the in-flight scan (kills the running tool children;
/// already-completed tools keep their findings). Errors when none is running.
#[tauri::command]
pub async fn audit_cancel_scan(state: State<'_, Arc<AuditState>>) -> AppResult<()> {
    state.cancel_scan().map_err(AppError::Audit)
}

/// V23 Phase B: the full (uncapped) runner snapshot — what the Code audit
/// view (Tool Activity tab) reads on mount and to fetch the complete findings
/// set after a truncated event.
#[tauri::command]
pub fn audit_snapshot(state: State<'_, Arc<AuditState>>) -> AuditSnapshot {
    state.snapshot()
}

/// Take (or reuse, ≤60s cache) the project's language census outside a scan,
/// apply quality auto-selection when it's on, and return the full snapshot —
/// so tab mount and the Settings section know applicability (chip gating,
/// "not applicable" hints, auto-selected checkboxes) before the first scan.
/// The walk is bounded but can take a couple of seconds cold, hence the
/// blocking-task hop. No-op passthrough while the feature is disabled or a
/// scan is in flight.
#[tauri::command]
pub async fn audit_refresh_census(
    state: State<'_, Arc<AuditState>>,
) -> AppResult<AuditSnapshot> {
    let audit = state.inner().clone();
    tauri::async_runtime::spawn_blocking(move || audit.refresh_census())
        .await
        .map_err(|e| AppError::Audit(format!("census task failed: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_version_prefers_stdout_then_stderr() {
        assert_eq!(
            parse_version(b"osv-scanner v1.9.2\n", b""),
            Some("osv-scanner v1.9.2".to_string())
        );
        // Falls back to stderr when stdout is blank.
        assert_eq!(
            parse_version(b"", b"gitleaks 8.18.0\n"),
            Some("gitleaks 8.18.0".to_string())
        );
        // Skips leading blank lines.
        assert_eq!(
            parse_version(b"\n  \n", b"fallback 1.0"),
            Some("fallback 1.0".to_string())
        );
        assert_eq!(parse_version(b"", b""), None);
    }

    #[tokio::test]
    async fn detect_missing_tool_reports_not_found() {
        // A bare name that is neither in ebin nor on PATH.
        let r = detect_tool(
            AuditToolId::OsvScanner,
            "cimp-definitely-not-a-real-tool-xyz",
            None,
        )
        .await;
        assert!(!r.found);
        assert!(r.path.is_none());
        assert_eq!(r.error.as_deref(), Some("not found on PATH or ebin"));
        assert!(r.version.is_none());
    }

    /// A binary that spawns but exits non-zero on `--version` (wrong exe, or a
    /// build wanting a `version` subcommand) must not be reported as installed
    /// with its usage/error line shown as the version.
    #[tokio::test]
    async fn detect_probe_failure_is_not_found() {
        // A real binary guaranteed to reject `--version` with a non-zero exit:
        // `where`(Windows) treats it as an unmatched pattern; `false` ignores it.
        #[cfg(windows)]
        let exe = which::which("where").expect("where on PATH");
        #[cfg(not(windows))]
        let exe = which::which("false").expect("false on PATH");
        let r = detect_tool(AuditToolId::Gitleaks, &exe.display().to_string(), None).await;
        assert!(!r.found, "{r:?}");
        assert!(r.path.is_some());
        assert!(r.version.is_none());
        assert!(
            r.error
                .as_deref()
                .unwrap_or("")
                .contains("exited with code"),
            "{r:?}"
        );
    }

    #[tokio::test]
    async fn detect_with_override_runs_version_probe() {
        // Use a real binary present in the build/test env (`cargo`) as the
        // verbatim override so the version probe actually runs end to end. The
        // adapter id is irrelevant to detection — only the resolved exe is run.
        let cargo = which::which("cargo").expect("cargo on PATH in the test env");
        let r = detect_tool(AuditToolId::Semgrep, &cargo.display().to_string(), None).await;
        assert!(r.found, "cargo override should resolve + probe: {r:?}");
        assert!(r.path.is_some());
        assert!(
            r.version.is_some(),
            "cargo --version should yield a line: {r:?}"
        );
    }
}
