//! V8-01 offload supervisor — the app-owned lifecycle for the single
//! local `llama-server`.
//!
//! Owns the child process + the [`LlamaServer`] HTTP view, driven by
//! `enabled`/`autostart` and the Start/Stop/Restart IPC. Spawns lazily
//! (never blocks app launch) and fails soft: a bad command or a server
//! that never reaches ready surfaces as an [`OffloadState::Error`] status,
//! not a hang. Killed on app exit (the child is `kill_on_drop`, and
//! [`OffloadSupervisor::stop`] runs from the `CloseRequested` path).
//!
//! This milestone spawns the server as a plain managed child (output
//! piped to the tracing log). Rendering it as a read-only, non-closable
//! "Offload Server" terminal tab — so the user can watch model-load
//! progress in the UI — is the tracked Phase-A follow-up; the supervisor
//! API here is shaped to drive that tab when it lands.

use std::sync::Arc;
use std::time::Duration;

use serde::Serialize;
use tauri::{AppHandle, Emitter};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Child;
use tokio::sync::{Mutex as TokioMutex, RwLock};
use tracing::{debug, info, warn};

use crate::error::{AppError, AppResult};
use crate::settings::SettingsHandle;

use super::server::{LlamaServer, ServerCommand};
use super::Backend;

/// Coarse server status surfaced to the frontend (mirrors the STT/TTS
/// state-event pattern).
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

/// A live server: the child handle plus the HTTP view.
struct Running {
    child: Child,
    server: Arc<LlamaServer>,
}

/// App-owned supervisor. Held in `AppState` behind an `Arc`.
pub struct OffloadSupervisor {
    inner: TokioMutex<Option<Running>>,
    state: RwLock<OffloadState>,
    settings: SettingsHandle,
    app: AppHandle,
}

impl OffloadSupervisor {
    pub fn new(app: AppHandle, settings: SettingsHandle) -> Arc<Self> {
        let initial = if settings.current().offload.enabled {
            OffloadState::Stopped
        } else {
            OffloadState::Disabled
        };
        Arc::new(Self {
            inner: TokioMutex::new(None),
            state: RwLock::new(initial),
            settings,
            app,
        })
    }

    /// Current status snapshot (refreshes the slot accounting from the
    /// live server if running).
    pub async fn status(&self) -> OffloadState {
        if let Some(running) = self.inner.lock().await.as_ref() {
            if running.server.is_ready() {
                return OffloadState::Ready {
                    n_ctx: running.server.n_ctx(),
                    slots: running.server.slots(),
                    in_flight: running.server.in_flight(),
                };
            }
        }
        self.state.read().await.clone()
    }

    async fn set_state(&self, new: OffloadState) {
        *self.state.write().await = new.clone();
        if let Err(e) = self.app.emit("offload-state", &new) {
            warn!(error = %e, "offload: emit offload-state failed");
        }
    }

    /// Start the server if not already running. Idempotent: a healthy or
    /// starting server is left alone. Parses `server_command`, spawns the
    /// child, and kicks off the readiness + reaper tasks. Never blocks on
    /// the model load.
    pub async fn start(self: &Arc<Self>) -> AppResult<()> {
        let snap = self.settings.current().offload;
        if !snap.enabled {
            return Err(AppError::OffloadNotReady("offload is disabled".into()));
        }
        let mut guard = self.inner.lock().await;
        if guard.is_some() {
            return Ok(()); // already running/starting
        }
        if snap.server_command.trim().is_empty() {
            self.set_state(OffloadState::Error {
                message: "server_command is not configured".into(),
            })
            .await;
            return Err(AppError::Offload("server_command is empty".into()));
        }
        let cmd = ServerCommand::parse(&snap.server_command)?;
        let child = spawn_child(&cmd)?;
        let server = Arc::new(LlamaServer::new(&snap.server_command)?);
        *guard = Some(Running {
            child,
            server: server.clone(),
        });
        drop(guard);

        self.set_state(OffloadState::Starting).await;
        info!(base_url = %server.base_url(), "offload: server starting");

        // Readiness probe — does not block the caller.
        let this = self.clone();
        tauri::async_runtime::spawn(async move {
            match server.poll_until_ready(Duration::from_secs(600)).await {
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

    /// Stop the server (kill the child) if running. Idempotent.
    pub async fn stop(&self) {
        let mut guard = self.inner.lock().await;
        if let Some(mut running) = guard.take() {
            running.server.mark_stopped();
            if let Err(e) = running.child.kill().await {
                warn!(error = %e, "offload: failed to kill server child");
            }
            debug!("offload: server stopped");
        }
        let next = if self.settings.current().offload.enabled {
            OffloadState::Stopped
        } else {
            OffloadState::Disabled
        };
        self.set_state(next).await;
    }

    /// Restart with the current `server_command` (Reset): stop, then start.
    pub async fn restart(self: &Arc<Self>) -> AppResult<()> {
        self.stop().await;
        self.start().await
    }

    /// Run one offload task against the app-owned server (used by the
    /// Settings "Test offload" button). Acquires a concurrency slot
    /// (bounded by `offload_timeout_secs`), runs the native-tools agent
    /// loop, and returns the synthesized result. Errors if the server
    /// isn't ready.
    pub async fn run_task(
        &self,
        instructions: String,
        thinking: super::agent::ThinkingMode,
    ) -> AppResult<String> {
        let snap = self.settings.current().offload;
        let server = {
            let guard = self.inner.lock().await;
            match guard.as_ref() {
                Some(r) if r.server.is_ready() => r.server.clone(),
                _ => {
                    return Err(AppError::OffloadNotReady(
                        "server is not running/ready — Start it first".into(),
                    ))
                }
            }
        };

        let timeout = Duration::from_secs(snap.offload_timeout_secs.max(30));
        let _permit = server.acquire_slot(timeout).await?;

        let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
        let ctx = super::tools::ToolCtx::new(
            snap.allowed_roots.clone(),
            snap.command_allowlist.clone(),
            &cwd,
        );
        let router = super::agent::NativeRouter {
            defs: super::tools::enabled_defs(&snap.tools),
            ctx,
        };
        let cfg = super::agent::AgentConfig {
            base_url: server.base_url(),
            model: None,
            max_steps: snap.max_steps.max(1),
            budget_tokens: server.per_slot_budget(snap.budget_high_water_pct),
            per_tool_result_token_cap: snap.per_tool_result_token_cap.max(256),
        };
        let task = super::agent::OffloadTask {
            instructions,
            context: None,
            thinking,
        };
        let deadline = std::time::Instant::now() + timeout;
        super::agent::run(server.client(), &cfg, &router, task, deadline).await
    }
}

/// Spawn the `llama-server` child, resolving the program via PATH and
/// piping its output to the tracing log so load progress is captured.
/// `kill_on_drop` guarantees no orphan if the supervisor is dropped.
fn spawn_child(cmd: &ServerCommand) -> AppResult<Child> {
    let binary = crate::pty::resolve_command(&cmd.program)?;
    let mut command = tokio::process::Command::new(&binary);
    command
        .args(&cmd.args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    let mut child = command
        .spawn()
        .map_err(|e| AppError::Spawn(format!("llama-server: {e}")))?;

    // Drain stdout/stderr into the log so they don't fill the OS pipe
    // buffer and stall the server.
    if let Some(out) = child.stdout.take() {
        tauri::async_runtime::spawn(log_stream(out, "stdout"));
    }
    if let Some(err) = child.stderr.take() {
        tauri::async_runtime::spawn(log_stream(err, "stderr"));
    }
    Ok(child)
}

async fn log_stream<R>(reader: R, label: &'static str)
where
    R: tokio::io::AsyncRead + Unpin,
{
    let mut lines = BufReader::new(reader).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        debug!(target: "offload_server", stream = label, "{line}");
    }
}
