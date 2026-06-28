//! V8-02 offload supervisor — the app-owned lifecycle for the **pool** of
//! offload backends.
//!
//! ccImp owns the *process* of each **Local** backend (a `llama-server`
//! spawned from its `server_command`), driven by `enabled`/`autostart` and
//! the per-backend Start/Stop/Restart IPC. **Remote** backends have no
//! process — the supervisor only health-probes them for the Settings status
//! line. Spawns lazily (never blocks app launch) and fails soft: a bad
//! command or a server that never reaches ready surfaces as an
//! [`OffloadState::Error`] status, not a hang. Children are `kill_on_drop`
//! and [`OffloadSupervisor::stop`] runs from the `CloseRequested` path, so
//! no orphan `llama-server` survives a graceful exit.
//!
//! V8-01 ran a single local server; V8-02 generalizes that to a map keyed
//! by backend name so the motivating "big local + small LAN box" setup
//! works. The legacy no-arg [`start`](OffloadSupervisor::start)/`stop`/
//! `restart` operate on the *primary* (first enabled Local) backend so the
//! existing single-server Settings controls keep working unchanged.
//!
//! Local servers run as plain managed children (output piped to the tracing
//! log). Rendering each as a read-only "Offload Server" tab is still the
//! tracked follow-up.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use serde::Serialize;
use tauri::{AppHandle, Emitter};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Child;
use tokio::sync::{Mutex as TokioMutex, RwLock};
use tracing::{debug, info, warn};

/// Cap on the per-backend captured-output ring buffer (model-load progress +
/// server logs), enough to cover a load + a good window of runtime, bounded so
/// a long-lived server can't grow it without limit.
const MAX_LOG_LINES: usize = 800;

/// One captured `llama-server` output line, emitted live on
/// `offload-server-output` for the read-only Settings log panel.
#[derive(Clone, Serialize)]
struct ServerLogLine {
    backend: String,
    line: String,
}

use crate::error::{AppError, AppResult};
use crate::settings::{
    BackendTier, OffloadBackend, OffloadBackendKind, SettingsHandle, ToolScope,
};

use super::server::{LlamaServer, ServerCommand};
use super::Backend;

/// Coarse status for the **primary** local backend, surfaced to the
/// frontend as the aggregate `offload-state` event (mirrors the STT/TTS
/// state-event pattern). Per-backend detail comes from [`BackendStatus`].
#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
#[serde(tag = "state", rename_all = "lowercase")]
pub enum OffloadState {
    /// Feature off (`offload.enabled == false`).
    Disabled,
    /// Enabled but no process running (lazy / stopped).
    Stopped,
    /// Process spawned, waiting for `/health`.
    Starting,
    /// Healthy; carries the discovered window + slot accounting.
    Ready {
        n_ctx: Option<u32>,
        slots: u32,
        in_flight: u32,
    },
    /// Spawn/health failure (carries a human-readable reason).
    Error { message: String },
}

/// Per-backend status row for the Settings backends editor. Covers Local
/// (process + health) and Remote (health-probe only) backends.
#[derive(Clone, Debug, Serialize)]
pub struct BackendStatus {
    pub name: String,
    /// `"local"` | `"lan"` | `"cloud"`.
    pub kind: String,
    /// `"fast"` | `"quality"`.
    pub tier: String,
    pub enabled: bool,
    /// A cloud backend awaiting the consent toggle (unusable until granted).
    pub cloud_blocked: bool,
    /// Coarse run state: `"disabled"`, `"stopped"`, `"starting"`, `"ready"`,
    /// `"unreachable"`, or `"error"`.
    pub state: String,
    pub n_ctx: Option<u32>,
    pub slots: u32,
    pub in_flight: u32,
    /// Short tool-scope summary (`"all tools"`, `"web/docs only"`, …).
    pub tool_scope: String,
    /// A human-readable failure reason when `state == "error"` (e.g. a
    /// non-llama.cpp server squatting on the port). `None` otherwise.
    pub error: Option<String>,
}

/// A live local server: the child handle plus the HTTP view.
struct Running {
    child: Child,
    server: Arc<LlamaServer>,
}

/// App-owned supervisor. Held in `AppState` behind an `Arc`.
pub struct OffloadSupervisor {
    /// Local backends ccImp owns the process of, keyed by backend name.
    running: TokioMutex<HashMap<String, Running>>,
    /// Aggregate state of the primary backend (for the `offload-state`
    /// event + legacy single-status IPC).
    state: RwLock<OffloadState>,
    settings: SettingsHandle,
    app: AppHandle,
    /// Per-backend captured stdout/stderr ring buffer (keyed by backend
    /// name), powering the read-only Settings log panel. Cleared on each
    /// (re)start so the panel shows the fresh model-load output. `Arc` so the
    /// per-stream drain tasks can append concurrently.
    logs: Arc<StdMutex<HashMap<String, VecDeque<String>>>>,
}

/// The enabled Local backends in the configured pool, in order.
fn local_backends(settings: &crate::settings::OffloadSettings) -> Vec<OffloadBackend> {
    settings
        .effective_backends()
        .into_iter()
        .filter(|b| b.enabled && matches!(b.kind, OffloadBackendKind::Local { .. }))
        .collect()
}

/// Pull a Local backend's spawnable config out of its kind.
fn local_command(b: &OffloadBackend) -> Option<(String, bool)> {
    match &b.kind {
        OffloadBackendKind::Local {
            server_command,
            autostart,
        } => Some((server_command.clone(), *autostart)),
        _ => None,
    }
}

impl OffloadSupervisor {
    pub fn new(app: AppHandle, settings: SettingsHandle) -> Arc<Self> {
        let initial = if settings.current().offload.enabled {
            OffloadState::Stopped
        } else {
            OffloadState::Disabled
        };
        Arc::new(Self {
            running: TokioMutex::new(HashMap::new()),
            state: RwLock::new(initial),
            settings,
            app,
            logs: Arc::new(StdMutex::new(HashMap::new())),
        })
    }

    /// Buffered captured output for one backend (or the primary backend when
    /// `name` is `None`), oldest line first. Drives the read-only Settings
    /// log panel's initial fill; live lines arrive via `offload-server-output`.
    pub fn server_logs(&self, name: Option<String>) -> Vec<String> {
        let key = name.or_else(|| self.primary_name());
        match key {
            Some(k) => self
                .logs
                .lock()
                .unwrap()
                .get(&k)
                .map(|d| d.iter().cloned().collect())
                .unwrap_or_default(),
            None => Vec::new(),
        }
    }

    /// The live [`LlamaServer`] handle for a running local backend, or
    /// `None` if it isn't currently running. Used by [`OffloadService`] to
    /// read **live** slot/`in_flight`/`n_ctx` state and acquire a real slot
    /// (so cross-tab `in_flight` is honest — the warm-pool fix).
    ///
    /// [`OffloadService`]: super::OffloadService
    pub async fn running_server(&self, name: &str) -> Option<Arc<LlamaServer>> {
        self.running.lock().await.get(name).map(|r| r.server.clone())
    }

    /// The primary backend name (first enabled Local backend), or `None`
    /// when the pool has no local backend.
    fn primary_name(&self) -> Option<String> {
        local_backends(&self.settings.current().offload)
            .first()
            .map(|b| b.name.clone())
    }

    /// Aggregate status of the primary backend (refreshes slot accounting
    /// from the live server if running). Kept for the legacy
    /// `offload_status` IPC + the single-status Settings readout.
    pub async fn status(&self) -> OffloadState {
        if let Some(name) = self.primary_name() {
            if let Some(running) = self.running.lock().await.get(&name) {
                if running.server.is_ready() {
                    return OffloadState::Ready {
                        n_ctx: running.server.n_ctx(),
                        slots: running.server.slots(),
                        in_flight: running.server.in_flight(),
                    };
                }
            }
        }
        self.state.read().await.clone()
    }

    /// Per-backend status for every enabled backend in the pool (Local +
    /// Remote). Remote backends are health-probed inline (short timeout).
    pub async fn statuses(&self) -> Vec<BackendStatus> {
        let snap = self.settings.current().offload;
        if !snap.enabled {
            return Vec::new();
        }
        let backends = snap.effective_backends();
        // Do NOT hold the `running` lock across this loop: a remote backend's
        // status is a live network health probe (up to the client timeout,
        // ~10s), and holding the mutex across it would stall every concurrent
        // start/stop/running_server call. `backend_status` takes the lock
        // itself, briefly and await-free, only for the local branch that needs
        // it.
        let mut out = Vec::with_capacity(backends.len());
        for b in &backends {
            out.push(self.backend_status(b, &snap).await);
        }
        out
    }

    async fn backend_status(
        &self,
        b: &OffloadBackend,
        snap: &crate::settings::OffloadSettings,
    ) -> BackendStatus {
        let tier = match b.tier {
            BackendTier::Fast => "fast",
            BackendTier::Quality => "quality",
        };
        let scope = scope_summary(&b.tool_scope, snap);
        match &b.kind {
            OffloadBackendKind::Local { .. } => {
                // Short, await-free critical section: the `Server` accessors
                // are all synchronous, so we never hold the lock across an
                // `.await` (the remote probe path below holds no lock at all).
                let running = self.running.lock().await;
                let (state, n_ctx, slots, in_flight, error) = match running.get(&b.name) {
                    Some(r) if r.server.is_ready() => (
                        "ready",
                        r.server.n_ctx(),
                        r.server.slots(),
                        r.server.in_flight(),
                        None,
                    ),
                    // Spawned but not ready: surface a recorded failure (e.g.
                    // a non-llama.cpp server on the port) as an error rather
                    // than a perpetual "starting".
                    Some(r) => match r.server.last_error() {
                        Some(msg) => ("error", None, 0, 0, Some(msg)),
                        None => ("starting", None, 0, 0, None),
                    },
                    None if !b.enabled => ("disabled", None, 0, 0, None),
                    None => ("stopped", None, 0, 0, None),
                };
                BackendStatus {
                    name: b.name.clone(),
                    kind: "local".into(),
                    tier: tier.into(),
                    enabled: b.enabled,
                    cloud_blocked: false,
                    state: state.into(),
                    n_ctx,
                    slots,
                    in_flight,
                    tool_scope: scope,
                    error,
                }
            }
            OffloadBackendKind::Remote {
                base_url,
                auth_token,
                is_cloud,
                ..
            } => {
                let (kind, cloud_blocked) = if *is_cloud {
                    ("cloud", b.cloud_blocked())
                } else {
                    ("lan", false)
                };
                // Probe health via the RemoteBackend impl (best-effort)
                // unless blocked by consent.
                let (state, n_ctx, slots) = if cloud_blocked {
                    ("blocked", b.declared_context, 1)
                } else {
                    probe_remote(
                        &b.name,
                        base_url,
                        auth_token,
                        *is_cloud,
                        b.tier,
                        b.tool_scope.clone(),
                        b.declared_context,
                    )
                    .await
                };
                BackendStatus {
                    name: b.name.clone(),
                    kind: kind.into(),
                    tier: tier.into(),
                    enabled: b.enabled,
                    cloud_blocked,
                    state: state.into(),
                    n_ctx,
                    slots,
                    in_flight: 0,
                    tool_scope: scope,
                    error: None,
                }
            }
        }
    }

    async fn set_state(&self, new: OffloadState) {
        *self.state.write().await = new.clone();
        if let Err(e) = self.app.emit("offload-state", &new) {
            warn!(error = %e, "offload: emit offload-state failed");
        }
    }

    /// Start the **primary** local backend (legacy single-server control).
    pub async fn start(self: &Arc<Self>) -> AppResult<()> {
        let name = self
            .primary_name()
            .ok_or_else(|| AppError::Offload("no local backend configured".into()))?;
        self.start_backend(&name).await
    }

    /// Start one named Local backend if not already running. Idempotent.
    pub async fn start_backend(self: &Arc<Self>, name: &str) -> AppResult<()> {
        let snap = self.settings.current().offload;
        if !snap.enabled {
            return Err(AppError::OffloadNotReady("offload is disabled".into()));
        }
        let backend = local_backends(&snap)
            .into_iter()
            .find(|b| b.name == name)
            .ok_or_else(|| AppError::Offload(format!("no local backend named `{name}`")))?;
        let (command, _autostart) = local_command(&backend)
            .ok_or_else(|| AppError::Offload(format!("`{name}` is not a local backend")))?;

        let mut guard = self.running.lock().await;
        if guard.contains_key(name) {
            return Ok(()); // already running/starting
        }
        if command.trim().is_empty() {
            self.set_state(OffloadState::Error {
                message: format!("`{name}`: server_command is not configured"),
            })
            .await;
            return Err(AppError::Offload("server_command is empty".into()));
        }
        let cmd = ServerCommand::parse(&command)?;
        // Fresh capture buffer per (re)start so the panel shows this load.
        self.logs.lock().unwrap().remove(name);
        let child = spawn_child(&cmd, &self.app, name, self.logs.clone())?;
        let server = Arc::new(LlamaServer::with_config(
            &backend.name,
            &command,
            backend.tier,
            backend.tool_scope.clone(),
        )?);
        guard.insert(
            name.to_string(),
            Running {
                child,
                server: server.clone(),
            },
        );
        drop(guard);

        let is_primary = self.primary_name().as_deref() == Some(name);
        if is_primary {
            self.set_state(OffloadState::Starting).await;
        }
        info!(backend = name, base_url = %server.base_url(), "offload: server starting");

        // Readiness probe — does not block the caller.
        let this = self.clone();
        let name_owned = name.to_string();
        tauri::async_runtime::spawn(async move {
            let result = server.poll_until_ready(Duration::from_secs(600)).await;
            if !is_primary {
                if let Err(e) = &result {
                    warn!(backend = %name_owned, error = %e, "offload: backend failed to become ready");
                }
                return;
            }
            match result {
                Ok(()) => {
                    info!(n_ctx = ?server.n_ctx(), "offload: server ready");
                    this.set_state(OffloadState::Ready {
                        n_ctx: server.n_ctx(),
                        slots: server.slots(),
                        in_flight: server.in_flight(),
                    })
                    .await;
                }
                Err(e) => {
                    warn!(error = %e, "offload: server failed to become ready");
                    this.set_state(OffloadState::Error {
                        message: e.to_string(),
                    })
                    .await;
                }
            }
        });
        Ok(())
    }

    /// Start every enabled Local backend that has `autostart` set. Called
    /// at app launch.
    pub async fn autostart_all(self: &Arc<Self>) {
        let snap = self.settings.current().offload;
        if !snap.enabled {
            return;
        }
        for b in local_backends(&snap) {
            if let Some((_, true)) = local_command(&b) {
                if let Err(e) = self.start_backend(&b.name).await {
                    warn!(backend = %b.name, error = %e, "offload: autostart failed");
                }
            }
        }
    }

    /// Stop the **primary** local backend (legacy control).
    pub async fn stop(&self) {
        if let Some(name) = self.primary_name() {
            self.stop_backend(&name).await;
        }
    }

    /// Stop one named Local backend (kill the child) if running. Idempotent.
    pub async fn stop_backend(&self, name: &str) {
        let mut guard = self.running.lock().await;
        if let Some(mut running) = guard.remove(name) {
            running.server.mark_stopped();
            if let Err(e) = running.child.kill().await {
                warn!(backend = name, error = %e, "offload: failed to kill server child");
            }
            debug!(backend = name, "offload: server stopped");
        }
        drop(guard);
        if self.primary_name().as_deref() == Some(name) {
            let next = if self.settings.current().offload.enabled {
                OffloadState::Stopped
            } else {
                OffloadState::Disabled
            };
            self.set_state(next).await;
        }
    }

    /// Stop *all* local backends (app exit / disable).
    pub async fn stop_all(&self) {
        // Two passes: a backend started concurrently (e.g. a racing autostart
        // on disable) between snapshotting the map and stopping each entry
        // would survive a single pass. A second sweep catches that straggler
        // without risking an unbounded loop if starts kept arriving.
        for _ in 0..2 {
            let names: Vec<String> = self.running.lock().await.keys().cloned().collect();
            if names.is_empty() {
                break;
            }
            for name in names {
                self.stop_backend(&name).await;
            }
        }
    }

    /// Restart the primary local backend with its current command (Reset).
    pub async fn restart(self: &Arc<Self>) -> AppResult<()> {
        let name = self
            .primary_name()
            .ok_or_else(|| AppError::Offload("no local backend configured".into()))?;
        self.restart_backend(&name).await
    }

    /// Restart one named Local backend (Reset): stop, then start.
    pub async fn restart_backend(self: &Arc<Self>, name: &str) -> AppResult<()> {
        self.stop_backend(name).await;
        self.start_backend(name).await
    }

    /// Run one offload task against a ready **local** backend (used by the
    /// Settings "Test offload" button). Picks the first ready local server,
    /// acquires a concurrency slot (bounded by `offload_timeout_secs`), runs
    /// the scoped native-tools agent loop, and returns the synthesized
    /// result. Errors if no local backend is ready.
    pub async fn run_task(
        &self,
        instructions: String,
        thinking: super::agent::ThinkingMode,
    ) -> AppResult<String> {
        let snap = self.settings.current().offload;
        let server = {
            let guard = self.running.lock().await;
            guard
                .values()
                .find(|r| r.server.is_ready())
                .map(|r| r.server.clone())
                .ok_or_else(|| {
                    AppError::OffloadNotReady(
                        "no local backend is running/ready — Start one first".into(),
                    )
                })?
        };

        let timeout = Duration::from_secs(snap.offload_timeout_secs.max(30));
        let _permit = server.acquire_slot(timeout).await?;

        let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
        let ctx = super::tools::ToolCtx::new(
            snap.allowed_roots.clone(),
            snap.command_allowlist.clone(),
            snap.command_policies.clone(),
            &cwd,
        );
        let router = super::agent::NativeRouter::new(
            super::tools::enabled_defs(&snap.tools),
            ctx,
            server.tool_scope().clone(),
        );
        let cfg = super::agent::AgentConfig {
            base_url: server.base_url(),
            model: None,
            max_steps: snap.max_steps.max(1),
            budget_tokens: server.per_slot_budget(snap.budget_high_water_pct),
            per_tool_result_token_cap: snap.per_tool_result_token_cap.max(256),
            auth_token: None,
        };
        let task = super::agent::OffloadTask {
            instructions,
            context: None,
            thinking,
        };
        let deadline = std::time::Instant::now() + timeout;
        // Self-test path: no external cancel source.
        let cancel = tokio_util::sync::CancellationToken::new();
        super::agent::run(server.client(), &cfg, &router, task, deadline, &cancel).await
    }
}

/// Coarse tool-scope summary for the status row (mirrors the MCP-side
/// renderer; kept here to avoid a cross-module dependency on the child).
fn scope_summary(scope: &ToolScope, snap: &crate::settings::OffloadSettings) -> String {
    match scope {
        ToolScope::All => "all tools".to_string(),
        _ => {
            let local_blocked = !scope.allows("read_file") && !scope.allows("code_search");
            if local_blocked {
                "web/docs only".to_string()
            } else {
                let mut n = 0;
                for t in ["read_file", "code_search", "run_command"] {
                    if scope.allows(t) {
                        n += 1;
                    }
                }
                for s in &snap.mcp_servers {
                    if s.enabled && scope.allows(&s.name) {
                        n += 1;
                    }
                }
                format!("{n} tools")
            }
        }
    }
}

/// Best-effort health probe of a remote endpoint for the status display,
/// via the [`RemoteBackend`](super::RemoteBackend) `Backend` impl:
/// `(state, n_ctx, slots)` where state is `"ready"` or `"unreachable"`.
/// `slots` is the `/props` `total_slots` when the endpoint reports it
/// (a llama-server does), else the assumed single slot.
#[allow(clippy::too_many_arguments)]
async fn probe_remote(
    name: &str,
    base_url: &str,
    auth_token: &str,
    is_cloud: bool,
    tier: BackendTier,
    tool_scope: ToolScope,
    declared: Option<u32>,
) -> (&'static str, Option<u32>, u32) {
    let backend = match super::RemoteBackend::new(
        name, base_url, auth_token, is_cloud, tier, tool_scope, declared, 1,
    ) {
        Ok(b) => b,
        Err(_) => return ("error", declared, 1),
    };
    if backend.health_check().await {
        let _ = backend.refresh_props().await; // best-effort
        ("ready", backend.n_ctx(), backend.slots())
    } else {
        ("unreachable", declared, 1)
    }
}

/// Spawn the `llama-server` child, resolving the program via PATH and
/// draining its output to both the tracing log and the per-backend capture
/// ring buffer (which feeds the read-only Settings log panel via the
/// `offload-server-output` event). `kill_on_drop` guarantees no orphan if
/// the supervisor is dropped.
fn spawn_child(
    cmd: &ServerCommand,
    app: &AppHandle,
    backend: &str,
    logs: Arc<StdMutex<HashMap<String, VecDeque<String>>>>,
) -> AppResult<Child> {
    let binary = crate::pty::resolve_command(&cmd.program)?;
    let mut command = tokio::process::Command::new(&binary);
    command
        .args(&cmd.args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    // Windows allocates a fresh console window whenever a GUI process spawns a
    // console executable like `llama-server.exe`. We capture its output over
    // piped stdout/stderr anyway, so suppress the empty conhost window with
    // CREATE_NO_WINDOW (0x0800_0000).
    #[cfg(windows)]
    command.creation_flags(0x0800_0000);
    let mut child = command
        .spawn()
        .map_err(|e| AppError::Spawn(format!("llama-server: {e}")))?;
    // Backstop: assign to the kill-on-job-close job so the OS reaps this
    // VRAM-holding server even if ccImp dies hard (crash / panic=abort /
    // dev hot-reload TerminateProcess), where kill_on_drop never fires.
    crate::process_guard::guard_child(&child);

    // Drain stdout/stderr into the log + capture buffer so they don't fill
    // the OS pipe buffer and stall the server, and so the panel can show them.
    if let Some(out) = child.stdout.take() {
        tauri::async_runtime::spawn(log_stream(
            out,
            "stdout",
            app.clone(),
            backend.to_string(),
            logs.clone(),
        ));
    }
    if let Some(err) = child.stderr.take() {
        tauri::async_runtime::spawn(log_stream(
            err,
            "stderr",
            app.clone(),
            backend.to_string(),
            logs,
        ));
    }
    Ok(child)
}

async fn log_stream<R>(
    reader: R,
    label: &'static str,
    app: AppHandle,
    backend: String,
    logs: Arc<StdMutex<HashMap<String, VecDeque<String>>>>,
) where
    R: tokio::io::AsyncRead + Unpin,
{
    let mut lines = BufReader::new(reader).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        debug!(target: "offload_server", stream = label, "{line}");
        // Append to the capped ring buffer for this backend.
        {
            let mut guard = logs.lock().unwrap();
            let buf = guard.entry(backend.clone()).or_default();
            buf.push_back(line.clone());
            while buf.len() > MAX_LOG_LINES {
                buf.pop_front();
            }
        }
        // Push live to the read-only panel (best-effort).
        let _ = app.emit(
            "offload-server-output",
            ServerLogLine {
                backend: backend.clone(),
                line,
            },
        );
    }
}
