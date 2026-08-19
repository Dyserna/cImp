//! V23 Code Audit — aggregated security scanning.
//!
//! Phase A ships only tool *detection*: [`audit_detect_tool`] resolves a
//! configured audit tool via [`crate::pty::resolve_command`] (honoring the
//! per-tool `path` override in [`crate::settings::AuditToolConfig`]) and probes
//! `<tool> --version`. The result is a report: this command writes no settings.
//! (The Settings pane may store what it found — that is the pane's write, made
//! on the user's click, exactly as a Browse… would be.)
//!
//! Phase B extends this module with the full concurrent runner, per-tool SARIF
//! adapters, and the `audit-status` progress events.

pub mod adapters;
pub mod census;
/// V38 Phase E — the pre-migration byte-match golden for the fourteen built-in
/// tools. Test-only: it exists to prove that turning the adapter table into
/// embedded manifests changed nothing observable.
#[cfg(test)]
mod golden;
pub mod mcp;
pub mod runnable;
pub mod runner;

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use tauri::State;

use crate::error::{AppError, AppResult};
use crate::ipc::AppState;

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

/// V23 Phase A: result of a Detect probe. The frontend renders it inline
/// (`✓ <version> — <path>` / the error); producing it changes nothing here, and
/// whether the found path is then stored is the Settings pane's decision.
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

/// Resolve one registered tool honoring its configured `path` and probe
/// `<tool> --version`. Read-only and side-effect-free w.r.t. settings — this
/// command never mutates the stored path. Always returns `Ok`: a not-found tool
/// is a normal result (`found = false`), not an error.
///
/// # What it searches for, and why that is not the run-time rule
///
/// With an empty `path` the probe searches `ebin` → `PATH` for the name
/// [`crate::plugins::registry::probe_command_name`] derives — for EVERY tool,
/// including a user plugin's. That is wider than
/// `EffectiveTool::resolves_by_name`, which stays exactly as it was: run time
/// still requires a stored path for a user plugin (decision 7/10), and nothing
/// about a Detect click makes a path-less tool runnable.
///
/// The two differ because they answer different questions. Run time asks "may
/// cImp spawn a binary the user never pointed it at?" — no. Detect asks "is
/// this tool on this machine?", pressed by the user, about one tool, with the
/// answer shown to them. Gating the second on the first made Detect report "not
/// found on PATH or ebin" for every starter-pack tool without ever looking,
/// which was untrue as well as useless.
///
/// The write half lives in the Settings pane on purpose: the probe reports, and
/// the found path becomes configuration through the same UI act that a Browse…
/// would have been. Keeping this command settings-read-only is what lets it be
/// called freely without a click ever silently changing what a scan launches.
///
/// `tool_key` is the registry key (`cimp-audit@1/gitleaks`, or a user plugin's
/// `name@version/tool-id`), and `path` is the LIVE value from the Settings
/// input — passed explicitly so a just-typed value cannot race the
/// fire-and-forget `settings_update` push. `None` falls back to what the
/// registry resolved for this project.
///
/// # Why this takes a key rather than an id
///
/// Before V38 it took an `AuditToolId`, because the only tools with a Detect
/// button were the fourteen built-in scanners. They are registry entries now,
/// beside whatever the user dropped in the plugins folder, and the button lives
/// in the Tool Plugins pane where both populations are configured. A key is what
/// that pane has.
#[tauri::command]
pub async fn audit_detect_tool(
    state: State<'_, AppState>,
    tool_key: String,
    path: Option<String>,
) -> AppResult<AuditDetectResult> {
    let settings = state.settings.current();
    let root = state.launch.cwd.clone();
    let tool = crate::plugins::registry::effective_tools(
        &crate::plugins::snapshot_or_scan(),
        &settings.tool_plugins,
        Some(&root),
    )
    .into_iter()
    .find(|t| t.tool_key == tool_key);
    let Some(tool) = tool else {
        return Ok(AuditDetectResult {
            found: false,
            path: None,
            version: None,
            error: Some(format!(
                "no registered tool `{tool_key}` — the plugin that declared it may have been \
                 removed; press Rescan"
            )),
        });
    };
    let override_path = path.or_else(|| tool.path.clone()).unwrap_or_default();
    Ok(detect_tool(
        &override_path,
        tool.probe_name().as_deref(),
        tool.manifest.project_local_bin.as_deref(),
        Some(&root),
    )
    .await)
}

/// Resolve a tool's binary to an on-disk path, for the DETECT probe.
///
/// Deliberately the same ladder `audit::runner::resolve_runnable` walks — a ✓
/// from Detect that pointed at a different binary than a scan launches would be
/// worse than no button at all. (The one place the two now differ is step 3 for
/// a USER plugin: Detect searches a derived name that run time will not, and
/// the pane answers that by STORING what it found, so the tool a scan resolves
/// afterwards is the exact path the probe reported.)
///
/// 1. **Configured `path`** (non-empty) — used verbatim (the V23 contract): a
///    deliberate "use exactly this binary" that project-local resolution must
///    not second-guess.
/// 2. **Project-local `node_modules/.bin`** — for a tool whose manifest names a
///    shim (eslint, knip), the project's own install beats a global one. Only
///    when there is no configured path and a `root` is known.
/// 3. **`ebin` → `PATH`** on the bare command name the caller derived.
///
/// `command` is whatever [`crate::plugins::registry::probe_command_name`]
/// could derive — for a built-in that is its manifest's `command`, and for a
/// user plugin it is a name read off the definition (a check's program, a
/// command tool's id) **for this probe only**. Decision 10 is unchanged where it
/// bites: RUN time still refuses to guess a binary for a user plugin, so an
/// unconfigured one is still inert in every pipeline. What this ladder produces
/// is an answer shown to the user, which becomes configuration only when they
/// store it. `None` (nothing derivable) resolves nowhere — the honest answer
/// rather than a guess.
fn resolve_audit_binary(
    override_path: &str,
    command: Option<&str>,
    project_local_bin: Option<&str>,
    root: Option<&Path>,
) -> AppResult<PathBuf> {
    let configured = override_path.trim();
    if !configured.is_empty() {
        return crate::pty::resolve_command(configured);
    }
    if let (Some(bin), Some(root)) = (project_local_bin, root) {
        if let Some(local) = resolve_project_local_bin(root, bin) {
            return Ok(local);
        }
    }
    let Some(command) = command else {
        return Err(AppError::Audit(
            "no path is configured for this tool".to_string(),
        ));
    };
    crate::pty::resolve_command(command)
}

/// The first existing `node_modules/.bin/<name>` shim under `root`, or `None`.
/// On Windows the runnable shim is the `.cmd`/`.CMD`/`.bat` launcher (the bare
/// `<name>` is a POSIX shell script npm also drops that Windows can't spawn), so
/// those are tried first — mirroring `pty::resolve`'s Windows extension order.
pub(super) fn resolve_project_local_bin(root: &Path, name: &str) -> Option<PathBuf> {
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
/// project-local `node_modules/.bin` then ebin → PATH); `root` scopes the
/// project-local lookup (`None` = skip it).
async fn detect_tool(
    override_path: &str,
    command: Option<&str>,
    project_local_bin: Option<&str>,
    root: Option<&Path>,
) -> AuditDetectResult {
    let resolved = match resolve_audit_binary(override_path, command, project_local_bin, root) {
        Ok(p) => p,
        Err(_) => {
            // Three genuinely different failures, and a message for each —
            // saying "not found on PATH or ebin" when no search ever ran is the
            // bug this arm was rewritten to fix.
            let error = if !override_path.trim().is_empty() {
                // A configured path that resolves nowhere is a misconfiguration,
                // and naming the offending value is the whole of the fix.
                format!("configured path not found: {}", override_path.trim())
            } else if let Some(name) = command {
                // A name search DID run and came back empty: say which name, so
                // the user can check that spelling against their own PATH.
                format!("`{name}` not found on PATH or ebin")
            } else {
                // Nothing to search for (a findings tool whose manifest names no
                // binary). Telling the user it is "not on PATH" would be a
                // finding cImp never made; the honest answer is that this tool
                // has to be pointed at something.
                "no path set — type a path or use Browse…".to_string()
            };
            return AuditDetectResult {
                found: false,
                path: None,
                version: None,
                error: Some(error),
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

    // Through the spawn gate like every other cImp spawn — see `spawn_gate`.
    let child = match crate::spawn_gate::spawn_tokio(&mut cmd) {
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

/// V38 Phase D: the tools a scan of `category` **would** run right now — the
/// configured built-in roster plus this project's runnable plugin tools — as
/// `idle` chips.
///
/// This is the Code Audit panel's PRE-SCAN chip list. Before V38 that list was
/// derived in TypeScript from `code_audit.tools`, i.e. from the built-in roster
/// alone, so a plugin tool the user had enabled and pointed at a binary stayed
/// invisible until a scan started running it. Read-only and cheap: the CACHED
/// census (never a walk — [`audit_refresh_census`] owns that), the live settings
/// snapshot, and the in-memory registry join.
#[tauri::command]
pub fn audit_effective_roster(
    state: State<'_, Arc<AuditState>>,
    category: adapters::Category,
) -> Vec<runner::ToolState> {
    state.effective_roster(category)
}

/// Take (or reuse, ≤60s cache) the project's language census outside a scan,
/// apply quality auto-selection when it's on, and return the full snapshot —
/// so tab mount and the Settings section know applicability (chip gating,
/// "not applicable" hints, auto-selected checkboxes) before the first scan.
/// The walk is bounded but can take a couple of seconds cold, hence the
/// blocking-task hop. No-op passthrough while the feature is disabled or a
/// scan is in flight.
#[tauri::command]
pub async fn audit_refresh_census(state: State<'_, Arc<AuditState>>) -> AppResult<AuditSnapshot> {
    let audit = state.inner().clone();
    tauri::async_runtime::spawn_blocking(move || audit.refresh_census())
        .await
        .map_err(|e| AppError::Audit(format!("census task failed: {e}")))
}

// ── The audit-tool scope actions retired in V38 Phase E ─────────────────────
//
// `audit_tools_global_config` / `_save_global` / `_load_global` existed because
// `code_audit.tools` was PROJECT-scoped with one machine-scoped field inside it
// (`path`): the user needed an explicit way to say "make this project's tool
// selection the default for every project", and an indicator saying which of the
// two a given tool was currently following.
//
// Schema v33 removed the reason. Per-tool enables, timeouts and paths now live
// in `tool_plugins`, which is machine scope by construction (the overlay strip
// enforces it structurally rather than by write-through), so there is no project
// copy to promote and no local/global ambiguity to indicate. What a project may
// still differ on — a tool's declared variable values and its extra CLI
// parameters — rides the overlay exactly as before and needs no button, because
// editing it in a project IS the project-scoped act.

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
        // A CONFIGURED override that resolves nowhere is a misconfiguration —
        // the error names the offending value (distinct from the empty-path
        // "not found on PATH or ebin" case, which the scan runner also
        // branches on).
        let r = detect_tool("cimp-definitely-not-a-real-tool-xyz", None, None, None).await;
        assert!(!r.found);
        assert!(r.path.is_none());
        assert_eq!(
            r.error.as_deref(),
            Some("configured path not found: cimp-definitely-not-a-real-tool-xyz")
        );
        assert!(r.version.is_none());
    }

    /// The two EMPTY-override failures are different facts and say so. Before
    /// this split, a tool with nothing to search for was reported as "not found
    /// on PATH or ebin" — a search result cImp had never obtained.
    #[tokio::test]
    async fn detect_without_a_path_distinguishes_no_name_from_a_failed_search() {
        // A name WAS searched for: the message names it, so the user can check
        // that spelling against their own PATH.
        let r = detect_tool("", Some("cimp-not-a-real-binary-xyz"), None, None).await;
        assert!(!r.found);
        assert!(r.path.is_none());
        assert_eq!(
            r.error.as_deref(),
            Some("`cimp-not-a-real-binary-xyz` not found on PATH or ebin")
        );

        // Nothing to search for — the honest answer is that this tool has to be
        // pointed at something, not that it is missing from PATH.
        let r = detect_tool("", None, None, None).await;
        assert!(!r.found);
        assert_eq!(
            r.error.as_deref(),
            Some("no path set — type a path or use Browse…")
        );
    }

    /// The probe resolves a bare NAME through `ebin` → `PATH`, which is what
    /// makes Detect useful for a user plugin's tool: `cargo build` derives
    /// `cargo`, and the ladder finds the same binary a Browse… would have.
    #[tokio::test]
    async fn detect_resolves_a_bare_name_through_the_path_ladder() {
        let r = detect_tool("", Some("cargo"), None, None).await;
        assert!(r.found, "cargo is on PATH in the test env: {r:?}");
        assert!(r.path.is_some() && r.version.is_some(), "{r:?}");
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
        let r = detect_tool(&exe.display().to_string(), None, None, None).await;
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
        // verbatim override so the version probe actually runs end to end. Which
        // tool it is is irrelevant to detection — only the resolved exe is run.
        let cargo = which::which("cargo").expect("cargo on PATH in the test env");
        let r = detect_tool(&cargo.display().to_string(), None, None, None).await;
        assert!(r.found, "cargo override should resolve + probe: {r:?}");
        assert!(r.path.is_some());
        assert!(
            r.version.is_some(),
            "cargo --version should yield a line: {r:?}"
        );
    }
}
