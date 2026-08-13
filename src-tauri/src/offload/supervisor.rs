//! V8-02 offload supervisor — the app-owned lifecycle for the **pool** of
//! offload backends.
//!
//! cImp owns the *process* of each **Local** backend (a `llama-server`
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
//! log) and surface in the Offload server dashboard (Tool Activity tab).

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use serde::Serialize;
use tauri::{AppHandle, Emitter};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Child;
use tokio::sync::{oneshot, Mutex as TokioMutex, RwLock};
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
use crate::settings::{BackendTier, OffloadBackend, OffloadBackendKind, SettingsHandle, ToolScope};

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
    /// When this process was spawned (epoch millis). Only the lifecycle feed
    /// reads it: it turns the `ready` row's `ms` into time-to-healthy (model
    /// load included) and the `stop` row's into uptime, which are the two
    /// numbers you actually want when a backend is behaving badly.
    started_ms: u64,
}

/// One transition in a local server's life, as it appears in the Events tab's
/// `tool` column.
///
/// A closed enum for the same reason [`crate::activity::ActivityKind`] is: the
/// frontend switches on these strings to pick a status word, and a typo'd
/// free-form verb would render as the no-category fallback instead of failing
/// to compile.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ServerEvent {
    /// The child process was spawned. Not yet healthy — `Ready` is a separate
    /// row precisely because the gap between them is where a model load (or a
    /// silent failure to load) lives.
    Start,
    /// `/health` came back and the window/slot accounting was read.
    Ready,
    /// The child was killed. An intentional stop, so `ok` stays true — see
    /// [`StopCause`] for which kind of intent.
    Stop,
    /// The backend never got to `Ready`: the command was missing or unparseable,
    /// the spawn failed, or the readiness probe errored or timed out.
    Fail,
}

impl ServerEvent {
    pub const fn as_str(self) -> &'static str {
        match self {
            ServerEvent::Start => "start",
            ServerEvent::Ready => "ready",
            ServerEvent::Stop => "stop",
            ServerEvent::Fail => "fail",
        }
    }
}

/// Who asked for a start. Recorded because "the server started" is not the
/// interesting half — a start the *user* clicked and a start some code path
/// triggered on its own are different events, and the on-demand one
/// ([`StartCause::Lazy`]) is the one that surprises people: an `offload_task`
/// can load a multi-GB model with nobody having pressed anything.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StartCause {
    /// `autostart_all` at app launch.
    Autostart,
    /// A user pressed Start (or the legacy single-server control) in Settings.
    Ipc,
    /// Warmed by the first `offload_task` that wanted this backend
    /// (`OffloadService`'s "start on first offload").
    Lazy,
    /// The start half of a Restart.
    Restart,
}

impl StartCause {
    const fn as_str(self) -> &'static str {
        match self {
            StartCause::Autostart => "autostart",
            StartCause::Ipc => "user",
            StartCause::Lazy => "on first offload",
            StartCause::Restart => "restart",
        }
    }
}

/// Why a server was stopped. Without this a `stop` row is unreadable: the app
/// quitting, a user pressing Stop, and the teardown half of a Restart all kill
/// the same child, and only one of them means anything went wrong.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StopCause {
    /// A user pressed Stop (or the legacy single-server control).
    Ipc,
    /// The stop half of a Restart — a `start` row follows immediately.
    Restart,
    /// `stop_all` from the graceful-exit path, or offload being disabled.
    Shutdown,
}

impl StopCause {
    const fn as_str(self) -> &'static str {
        match self {
            StopCause::Ipc => "user",
            StopCause::Restart => "restart",
            StopCause::Shutdown => "app shutdown",
        }
    }
}

/// Build the `offload_server` activity row for one lifecycle transition.
///
/// Separate from the recording so the row a transition produces is assertable
/// without spawning a process — the supervisor's paths are otherwise only
/// reachable with a real `llama-server` on the other end, which is how this
/// feed would have ended up tested by re-deriving it beside itself.
///
/// Column conventions, which the frontend mirrors:
/// * `source` — the backend name, matching the `offload` task rows, so
///   filtering the feed by a backend shows its tasks and its process history
///   together.
/// * `tool` — the [`ServerEvent`].
/// * `target` — the human-readable *why*/*what*: the cause for a start or stop,
///   the discovered window for a ready, the failure reason for a fail.
/// * `root` — always empty. A server process is genuinely not about a project,
///   which is one of the two things the empty sentinel is documented to mean
///   (`ActivityEntry::root`); the project-scoped views correctly do not show it.
/// * `tab` — always [`Attribution::Headless`]. cImp's own process management has
///   no tab behind it even when a user pressed the button, and inventing one
///   would be a worse lie than the honest "no tab".
fn lifecycle_record(
    backend: &str,
    event: ServerEvent,
    target: String,
    detail: String,
    ms: u64,
) -> crate::activity::ActivityRecord {
    crate::activity::ActivityRecord {
        entry: crate::activity::ActivityEntry::new(
            crate::activity::ActivityKind::OffloadServer,
            crate::activity::now_ms(),
            String::new(),
            backend.to_string(),
            event.as_str().to_string(),
            target,
            0,
            ms,
            event != ServerEvent::Fail,
            crate::activity::Attribution::Headless,
            None,
        ),
        request: String::new(),
        response: detail,
    }
}

/// App-owned supervisor. Held in `AppState` behind an `Arc`.
pub struct OffloadSupervisor {
    /// Local backends cImp owns the process of, keyed by backend name.
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
            ..
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
        self.running
            .lock()
            .await
            .get(name)
            .map(|r| r.server.clone())
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
    pub async fn start(self: &Arc<Self>, cause: StartCause) -> AppResult<()> {
        let name = self
            .primary_name()
            .ok_or_else(|| AppError::Offload("no local backend configured".into()))?;
        self.start_backend(&name, None, cause).await
    }

    /// Start one named Local backend if not already running. Idempotent —
    /// except with a `command_override`, where an already-running backend is
    /// an error (silently ignoring the edited command would let the popup
    /// report success while the old process keeps running).
    ///
    /// `command_override` (the Offload server dashboard's "show command on start"
    /// popup) replaces the backend's configured `server_command` for this
    /// launch only — it is never persisted, and goes through the exact same
    /// parse/validation as the configured command.
    /// `cause` names who asked; it is recorded on the `start` (or `fail`) row
    /// and is the difference between a readable process history and a list of
    /// starts with no explanation.
    pub async fn start_backend(
        self: &Arc<Self>,
        name: &str,
        command_override: Option<String>,
        cause: StartCause,
    ) -> AppResult<()> {
        // One funnel for the failure feed: every way a start can fail before a
        // healthy server exists returns `Err` from `start_inner`, so a new
        // early return cannot quietly skip the `fail` row the way it would if
        // each site recorded for itself.
        let started = crate::activity::now_ms();
        let result = self.start_inner(name, command_override, cause).await;
        if let Err(e) = &result {
            crate::activity::record_bg(lifecycle_record(
                name,
                ServerEvent::Fail,
                format!("start ({}) failed", cause.as_str()),
                e.to_string(),
                crate::activity::now_ms().saturating_sub(started),
            ));
        }
        result
    }

    async fn start_inner(
        self: &Arc<Self>,
        name: &str,
        command_override: Option<String>,
        cause: StartCause,
    ) -> AppResult<()> {
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
            if command_override.is_some() {
                return Err(AppError::Offload(format!(
                    "`{name}` is already running — stop it first to start with an edited command"
                )));
            }
            return Ok(()); // already running/starting
        }
        let command = command_override.unwrap_or(command);
        if command.trim().is_empty() {
            // Drop the `running` lock before the set_state await so a slow
            // event emit can't block other backend start/stop calls.
            drop(guard);
            self.set_state(OffloadState::Error {
                message: format!("`{name}`: server_command is not configured"),
            })
            .await;
            return Err(AppError::Offload("server_command is empty".into()));
        }
        let cmd = ServerCommand::parse(&command)?;
        // Fresh capture buffer per (re)start so the panel shows this load.
        self.logs.lock().unwrap().remove(name);
        let (child, exited) = spawn_child(&cmd, &self.app, name, self.logs.clone())?;
        let server = Arc::new(LlamaServer::with_config(
            &backend.name,
            &command,
            backend.tier,
            backend.tool_scope.clone(),
        )?);
        let started_ms = crate::activity::now_ms();
        guard.insert(
            name.to_string(),
            Running {
                child,
                server: server.clone(),
                started_ms,
            },
        );
        drop(guard);

        let is_primary = self.primary_name().as_deref() == Some(name);
        if is_primary {
            self.set_state(OffloadState::Starting).await;
        }
        info!(backend = name, base_url = %server.base_url(), "offload: server starting");
        // The command is the row's payload, not its target: with a
        // `command_override` this is the ONLY durable record of what actually
        // ran (the override is deliberately never persisted to settings).
        crate::activity::record_bg(lifecycle_record(
            name,
            ServerEvent::Start,
            format!("{} · {}", cause.as_str(), server.base_url()),
            command.clone(),
            0,
        ));

        // Unexpected-exit watcher. Without it a server that dies on its own
        // leaves a `start`/`ready` pair with no terminator, and a feed of runs
        // that never end reads as "still running" — the run history would be
        // silently wrong in exactly the case it exists for.
        //
        // Two conditions keep it from crying wolf, and both are necessary:
        // * still the SAME run — `stop_backend` removes the entry before it
        //   kills, so an intentional stop finds nothing here; and matching
        //   `started_ms` means a restart that got in first is not mistaken for
        //   this process dying.
        // * it had reached ready — a death during model load already produces
        //   the readiness probe's "never became ready" row below, and one death
        //   must not report as two.
        {
            let this = self.clone();
            let name_owned = name.to_string();
            tauri::async_runtime::spawn(async move {
                if exited.await.is_err() {
                    return; // no exit edge available (see `spawn_child`)
                }
                let still_this_run = {
                    let running = this.running.lock().await;
                    running
                        .get(&name_owned)
                        .is_some_and(|r| r.started_ms == started_ms && r.server.is_ready())
                };
                if !still_this_run {
                    return;
                }
                warn!(backend = %name_owned, "offload: server exited unexpectedly");
                crate::activity::record_bg(lifecycle_record(
                    &name_owned,
                    ServerEvent::Fail,
                    "exited unexpectedly".to_string(),
                    "The server process ended without cImp stopping it. Its captured output is \
                     in Settings → Offload task tools → the backend's log panel, until the \
                     next start clears it."
                        .to_string(),
                    crate::activity::now_ms().saturating_sub(started_ms),
                ));
            });
        }

        // Readiness probe — does not block the caller.
        let this = self.clone();
        let name_owned = name.to_string();
        tauri::async_runtime::spawn(async move {
            let result = server.poll_until_ready(Duration::from_secs(600)).await;
            let ms = crate::activity::now_ms().saturating_sub(started_ms);
            // Recorded for EVERY backend, primary or not. `set_state` below is
            // still primary-only — it drives the single aggregate `offload-state`
            // event — but that asymmetry is exactly why a non-primary backend
            // failing to load used to leave nothing but a `warn!` in the log.
            match &result {
                Ok(()) => crate::activity::record_bg(lifecycle_record(
                    &name_owned,
                    ServerEvent::Ready,
                    match server.n_ctx() {
                        Some(n) => format!("n_ctx {n} · {} slots", server.slots()),
                        None => format!("{} slots · window not reported", server.slots()),
                    },
                    String::new(),
                    ms,
                )),
                Err(e) => crate::activity::record_bg(lifecycle_record(
                    &name_owned,
                    ServerEvent::Fail,
                    "never became ready".to_string(),
                    e.to_string(),
                    ms,
                )),
            }
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
                if let Err(e) = self
                    .start_backend(&b.name, None, StartCause::Autostart)
                    .await
                {
                    warn!(backend = %b.name, error = %e, "offload: autostart failed");
                }
            }
        }
    }

    /// Stop the **primary** local backend (legacy control).
    pub async fn stop(&self, cause: StopCause) {
        if let Some(name) = self.primary_name() {
            self.stop_backend(&name, cause).await;
        }
    }

    /// Stop one named Local backend (kill the child) if running. Idempotent.
    ///
    /// `cause` names the intent. It is the whole readability of the `stop` row:
    /// the app quitting, a user pressing Stop and the teardown half of a Restart
    /// all arrive here as the same `kill()`.
    pub async fn stop_backend(&self, name: &str, cause: StopCause) {
        // Remove under the lock, then release it BEFORE the kill().await — a
        // slow child kill must not block start/stop of other backends.
        let removed = {
            let mut guard = self.running.lock().await;
            guard.remove(name)
        };
        if let Some(mut running) = removed {
            running.server.mark_stopped();
            let killed = running.child.kill().await;
            if let Err(e) = &killed {
                warn!(backend = name, error = %e, "offload: failed to kill server child");
            }
            debug!(backend = name, "offload: server stopped");
            // Idempotent by design: only the call that actually removed a live
            // entry writes a row, so the second `stop_all` sweep and a repeated
            // Stop click add nothing.
            crate::activity::record_bg(lifecycle_record(
                name,
                ServerEvent::Stop,
                cause.as_str().to_string(),
                match killed {
                    Ok(()) => String::new(),
                    // The child is dropped either way (`kill_on_drop`), so the
                    // server IS gone — but "we could not kill it cleanly" is a
                    // fact worth keeping next to a stop that misbehaved.
                    Err(e) => format!("kill failed: {e}"),
                },
                crate::activity::now_ms().saturating_sub(running.started_ms),
            ));
        }
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
                self.stop_backend(&name, StopCause::Shutdown).await;
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
    ///
    /// Deliberately two rows in the feed, not one `restart`: the stop can
    /// succeed and the start fail, and a single row would have to pick one
    /// outcome for both halves.
    pub async fn restart_backend(self: &Arc<Self>, name: &str) -> AppResult<()> {
        self.stop_backend(name, StopCause::Restart).await;
        self.start_backend(name, None, StartCause::Restart).await
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
        // V32 Phase G: the whole snapshot is kept (not just its `offload` half)
        // because the injection resolver reads across it — the per-tab L3 rows
        // live on `tabs`, and one settings read must answer every question this
        // run asks.
        let settings = self.settings.current();
        let snap = settings.offload.clone();
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
        // #48, finding F-10: the self-test's backend is by construction a LOCAL
        // one, but the policy is resolved through the shared constructor rather
        // than hardcoded — so `graph.enabled = false` denies a hallucinated
        // `graph_*` here exactly as it does on the other two worker paths.
        let router = super::agent::NativeRouter::new(
            super::tools::enabled_defs(&snap.tools),
            ctx,
            super::backend_gate::BackendGate::for_worker(
                server.tool_scope().clone(),
                false,
                &settings,
            ),
        );
        // #48/M-1: one scope for this self-test run. It makes a single
        // `agent::run` call, so there is no reset to fix here — it is threaded for
        // the same reason its budget is read from settings rather than hardcoded:
        // the self-test must not become the one worker path with a shape of its
        // own.
        let mut task_scope = super::agent::TaskScope::for_task();
        let cfg = super::agent::AgentConfig {
            base_url: server.base_url(),
            model: None,
            max_steps: snap.max_steps.max(1),
            budget_tokens: server.per_slot_budget(snap.budget_high_water_pct),
            n_ctx: server.n_ctx(),
            slots: server.slots(),
            per_tool_result_token_cap: snap.per_tool_result_token_cap.max(256),
            auth_token: None,
            per_call_timeout: timeout,
            // V32 Phase C: the self-test uses a NativeRouter (no MCP host), so
            // no EXTERNAL tool is reachable and the budget is inert — filled
            // from settings anyway so the paths cannot drift. (#48/M-1: the scope
            // id that used to sit here is now `task_scope` above.)
            // V32 Phase G: resolved at the `offload-worker` pseudo-scope like
            // every other worker-side control, even though this router reaches
            // no EXTERNAL tool — the self-test must not become the one path
            // whose posture is hardcoded.
            external_budget: crate::settings::injection::budget_limits(
                &settings,
                crate::settings::injection::Scope::OffloadWorker,
            ),
            latch_active: crate::settings::injection::effective(
                crate::settings::injection::Feature::TaintLatch,
                crate::settings::injection::Scope::OffloadWorker,
                &settings,
            ),
            canary_active: crate::settings::injection::effective(
                crate::settings::injection::Feature::Canary,
                crate::settings::injection::Scope::OffloadWorker,
                &settings,
            ),
        };
        let task = super::agent::OffloadTask {
            instructions,
            context: None,
            thinking,
            schema: None,
            // V32: the self-test runs an app-composed canned prompt with no
            // external caller, so it declares no profile and latches
            // dynamically like any undeclared task.
            profile: None,
        };
        let deadline = std::time::Instant::now() + timeout;
        // Self-test path: no external cancel source.
        let cancel = tokio_util::sync::CancellationToken::new();
        super::agent::run(
            server.client(),
            &cfg,
            &router,
            task,
            deadline,
            None,
            &cancel,
            &mut task_scope,
        )
        .await
    }

    /// V11 Phase F — a single plain completion against a ready **local** backend:
    /// no agent loop, no tools, and **never** a remote/cloud backend (project
    /// source stays on the machine). Used by internal callers like the context
    /// digest generator. Errors if no local backend is ready. Local-only holds by
    /// construction — `running` only ever contains local servers; remote backends
    /// live on a separate path and never appear here.
    pub async fn run_internal(
        &self,
        prompt: String,
        max_tokens: u32,
        timeout: Duration,
    ) -> AppResult<String> {
        let server = {
            let guard = self.running.lock().await;
            guard
                .values()
                .find(|r| r.server.is_ready())
                .map(|r| r.server.clone())
                .ok_or_else(|| {
                    AppError::OffloadNotReady("no local backend is running/ready".into())
                })?
        };
        let _permit = server.acquire_slot(timeout).await?;

        // A plain, non-streaming completion — no tools, thinking suppressed.
        // Non-streaming forgoes the disconnect-abort that streaming gives on a
        // client timeout, so a timed-out call may leave the backend generating up
        // to `max_tokens` more; acceptable here because callers pass a small cap
        // (digests use 128) and the work is opt-in, best-effort background jobs.
        let req = super::openai::ChatRequest {
            messages: vec![super::openai::ChatMessage::user(prompt)],
            tools: Vec::new(),
            tool_choice: None,
            model: None,
            temperature: Some(0.2),
            chat_template_kwargs: Some(serde_json::json!({ "enable_thinking": false })),
            stream: Some(false),
            stream_options: None,
            max_tokens: Some(max_tokens),
            response_format: None,
        };
        let url = format!("{}/v1/chat/completions", server.base_url());
        let resp = server
            .client()
            .post(&url)
            .json(&req)
            .timeout(timeout)
            .send()
            .await
            .map_err(|e| AppError::Offload(format!("internal completion request failed: {e}")))?;
        if !resp.status().is_success() {
            return Err(AppError::Offload(format!(
                "internal completion returned HTTP {}",
                resp.status()
            )));
        }
        let body: super::openai::ChatResponse = resp
            .json()
            .await
            .map_err(|e| AppError::Offload(format!("internal completion decode failed: {e}")))?;
        let content = body
            .choices
            .first()
            .and_then(|c| c.message.content.clone())
            .unwrap_or_default();
        Ok(super::openai::strip_think(&content))
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
                    if s.offload_access && scope.allows(&s.name) {
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
) -> AppResult<(Child, oneshot::Receiver<()>)> {
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
    // VRAM-holding server even if cImp dies hard (crash / panic=abort /
    // dev hot-reload TerminateProcess), where kill_on_drop never fires.
    crate::process_guard::guard_child(&child);

    // Drain stdout/stderr into the log + capture buffer so they don't fill
    // the OS pipe buffer and stall the server, and so the panel can show them.
    //
    // stdout's drain doubles as the **process-exit signal**: the child holds the
    // write end, so the read end reaching EOF means the process is gone. That is
    // the only exit edge available without moving the `Child` out of `Running`
    // (which is where `stop_backend`'s `kill()` needs it), and it fires for a
    // crash exactly as it does for a kill — telling the two apart is the
    // caller's job, not this signal's. `stdout` only, so an exit produces one
    // notification rather than one per stream.
    let (exited_tx, exited_rx) = oneshot::channel();
    let mut exited_tx = Some(exited_tx);
    if let Some(out) = child.stdout.take() {
        let app = app.clone();
        let backend = backend.to_string();
        let logs = logs.clone();
        let tx = exited_tx.take();
        tauri::async_runtime::spawn(async move {
            log_stream(out, "stdout", app, backend, logs).await;
            // A dropped receiver (nobody is watching) is not an error.
            if let Some(tx) = tx {
                let _ = tx.send(());
            }
        });
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
    // stdio was requested as `piped()` just above, so `stdout` is `Some` on
    // every real spawn. If that ever stops holding, a sender dropped here
    // closes the channel and the watcher exits quietly rather than waiting
    // forever on an edge that will never come.
    drop(exited_tx);
    Ok((child, exited_rx))
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::activity::ActivityKind;

    /// The four transitions must not collapse into "ok / not ok". A `stop` is
    /// an intended shutdown and a `fail` is not, and the Events tab picks its
    /// status word from `tool` — so a verb renamed here without the frontend
    /// following is a row that silently falls back to the no-category word.
    #[test]
    fn a_stop_is_not_a_failure_and_a_fail_is() {
        let stop = lifecycle_record("big-local", ServerEvent::Stop, "user".into(), String::new(), 90_000);
        assert!(stop.entry.ok, "an intentional stop must not read as a failure");
        assert_eq!(stop.entry.tool, "stop");
        assert_eq!(stop.entry.ms, 90_000, "a stop's duration is the run's uptime");

        for e in [ServerEvent::Start, ServerEvent::Ready] {
            assert!(lifecycle_record("b", e, String::new(), String::new(), 0).entry.ok);
        }
        let fail = lifecycle_record("b", ServerEvent::Fail, "never became ready".into(), "timed out".into(), 600_000);
        assert!(!fail.entry.ok);
        assert_eq!(fail.response, "timed out", "the reason must survive into the detail popup");
    }

    /// Every lifecycle row lands in the lane that was chosen for it, is
    /// attributed to the backend, and claims neither a project nor a tab.
    #[test]
    fn lifecycle_rows_are_headless_rootless_and_backend_sourced() {
        let r = lifecycle_record("big-local", ServerEvent::Ready, "n_ctx 32768 · 4 slots".into(), String::new(), 12_000);
        assert_eq!(r.entry.kind, ActivityKind::OffloadServer.as_str());
        assert_eq!(r.entry.source, "big-local");
        assert_eq!(r.entry.target, "n_ctx 32768 · 4 slots");
        assert_eq!(
            r.entry.root, "",
            "a server process is not about a project — see ActivityEntry::root"
        );
        assert!(
            matches!(r.entry.tab, crate::activity::Attribution::Headless),
            "process management has no tab behind it, even when a user pressed Start"
        );
        assert!(r.entry.session.is_none());
    }

    /// The causes exist to make a `stop` readable; two of them collapsing into
    /// the same word would put us back where we started.
    #[test]
    fn every_cause_has_its_own_word() {
        let starts = [
            StartCause::Autostart,
            StartCause::Ipc,
            StartCause::Lazy,
            StartCause::Restart,
        ]
        .map(StartCause::as_str);
        let stops = [StopCause::Ipc, StopCause::Restart, StopCause::Shutdown].map(StopCause::as_str);
        for set in [&starts[..], &stops[..]] {
            let mut seen = std::collections::HashSet::new();
            for w in set {
                assert!(!w.is_empty());
                assert!(seen.insert(*w), "duplicate cause word `{w}`");
            }
        }
    }
}
