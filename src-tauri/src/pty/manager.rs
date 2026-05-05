use std::collections::HashSet;
use std::io::Write;
use std::path::Path;
use std::sync::{Arc, Mutex as StdMutex};

use portable_pty::{native_pty_system, ChildKiller, CommandBuilder, MasterPty, PtySize};
use tauri::ipc::Channel;
use tauri::{AppHandle, Manager};
use tokio::sync::{mpsc, Mutex as TokioMutex};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info};

use crate::error::{AppError, AppResult};
use crate::pty::tasks;
use crate::state::{StateSignal, TabId};
use crate::tts::TtsRequest;

/// Per-tab launch parameters resolved by the registry before spawning. Keeps
/// the PtyManager binary-agnostic so each tab can plug in its own command.
pub struct PtyLaunchSpec {
    pub tab: TabId,
    /// Path to the resolved binary on disk (already passed through `which`).
    pub binary: std::path::PathBuf,
    /// Arguments inserted before `extra_args`. Used for `--append-system-
    /// prompt <content>` on the Claude tab.
    pub pre_args: Vec<String>,
    /// User-supplied flags + cctts invocation args.
    pub extra_args: Vec<String>,
    pub working_dir: std::path::PathBuf,
}

pub struct PtyManager {
    inner: Arc<TokioMutex<Option<PtyHandle>>>,
}

struct PtyHandle {
    writer: Arc<StdMutex<Box<dyn Write + Send>>>,
    master: Arc<StdMutex<Box<dyn MasterPty + Send>>>,
    killer: Arc<StdMutex<Box<dyn ChildKiller + Send + Sync>>>,
    cancel: CancellationToken,
    /// Last (rows, cols) we forwarded to the PTY. Lets `resize` short-
    /// circuit a no-op call so a same-size resize event from the WebView
    /// (e.g. mid-drag across DPI boundaries) doesn't pulse SIGWINCH at the
    /// child and trigger a TUI redraw.
    last_size: Arc<StdMutex<(u16, u16)>>,
}

impl PtyManager {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(TokioMutex::new(None)),
        }
    }

    pub async fn start(
        &self,
        app: AppHandle,
        spec: PtyLaunchSpec,
        output_channel: Channel<String>,
        initial_rows: u16,
        initial_cols: u16,
        tts_segments: mpsc::Sender<TtsRequest>,
        user_typed_tts: Arc<StdMutex<HashSet<String>>>,
        state_signals: mpsc::Sender<StateSignal>,
    ) -> AppResult<()> {
        let mut guard = self.inner.lock().await;
        if guard.is_some() {
            return Err(AppError::AlreadyStarted);
        }

        let tab = spec.tab;
        info!(?tab, path = %spec.binary.display(), "spawning subprocess");

        let pty_system = native_pty_system();
        let pair = match pty_system.openpty(PtySize {
            rows: initial_rows.max(1),
            cols: initial_cols.max(1),
            pixel_width: 0,
            pixel_height: 0,
        }) {
            Ok(p) => p,
            Err(e) => {
                // The waiter task (which normally emits SubprocessExited) is
                // never spawned on the spawn-error path; emit it here so the
                // state machine pins the tab to Error and the avatar reflects
                // the failure.
                let _ = state_signals.try_send(StateSignal::SubprocessExited { tab });
                return Err(AppError::Pty(format!("openpty: {e}")));
            }
        };

        let mut cmd = CommandBuilder::new(&spec.binary);
        for arg in &spec.pre_args {
            cmd.arg(arg);
        }
        for arg in &spec.extra_args {
            cmd.arg(arg);
        }
        cmd.cwd(&spec.working_dir);

        let child = match pair.slave.spawn_command(cmd) {
            Ok(c) => c,
            Err(e) => {
                let _ = state_signals.try_send(StateSignal::SubprocessExited { tab });
                return Err(AppError::Spawn(format!("{e}")));
            }
        };
        let killer = child.clone_killer();

        // Drop the slave end in the parent; the child inherits its own reference.
        drop(pair.slave);

        let reader = pair
            .master
            .try_clone_reader()
            .map_err(|e| AppError::Pty(format!("try_clone_reader: {e}")))?;
        let writer = pair
            .master
            .take_writer()
            .map_err(|e| AppError::Pty(format!("take_writer: {e}")))?;

        let master: Arc<StdMutex<Box<dyn MasterPty + Send>>> = Arc::new(StdMutex::new(pair.master));
        let writer: Arc<StdMutex<Box<dyn Write + Send>>> = Arc::new(StdMutex::new(writer));
        let killer: Arc<StdMutex<Box<dyn ChildKiller + Send + Sync>>> =
            Arc::new(StdMutex::new(killer));

        let cancel = CancellationToken::new();
        let (bytes_tx, bytes_rx) = mpsc::channel::<Vec<u8>>(256);

        tasks::spawn_reader(reader, bytes_tx, cancel.clone());
        let settings = app.state::<crate::ipc::AppState>().settings.clone();
        tasks::spawn_processor(
            tab,
            bytes_rx,
            output_channel,
            tts_segments,
            cancel.clone(),
            user_typed_tts,
            state_signals.clone(),
            settings,
        );
        tasks::spawn_waiter(tab, child, app, cancel.clone(), state_signals);

        *guard = Some(PtyHandle {
            writer,
            master,
            killer,
            cancel,
            last_size: Arc::new(StdMutex::new((initial_rows.max(1), initial_cols.max(1)))),
        });
        info!(
            ?tab,
            rows = initial_rows,
            cols = initial_cols,
            "PTY session started"
        );
        Ok(())
    }

    pub async fn write_input(&self, bytes: Vec<u8>) -> AppResult<()> {
        let writer = {
            let guard = self.inner.lock().await;
            let handle = guard.as_ref().ok_or(AppError::NotStarted)?;
            handle.writer.clone()
        };

        tokio::task::spawn_blocking(move || -> AppResult<()> {
            let mut w = writer
                .lock()
                .map_err(|e| AppError::Pty(format!("writer poisoned: {e}")))?;
            w.write_all(&bytes).map_err(AppError::Io)?;
            w.flush().map_err(AppError::Io)?;
            Ok(())
        })
        .await
        .map_err(|e| AppError::Pty(format!("blocking task: {e}")))??;
        Ok(())
    }

    pub async fn resize(&self, rows: u16, cols: u16) -> AppResult<()> {
        let rows = rows.max(1);
        let cols = cols.max(1);

        let (master, last_size) = {
            let guard = self.inner.lock().await;
            let handle = guard.as_ref().ok_or(AppError::NotStarted)?;
            (handle.master.clone(), handle.last_size.clone())
        };

        {
            let last = last_size
                .lock()
                .map_err(|e| AppError::Pty(format!("last_size poisoned: {e}")))?;
            if *last == (rows, cols) {
                return Ok(());
            }
        }

        tokio::task::spawn_blocking(move || -> AppResult<()> {
            let m = master
                .lock()
                .map_err(|e| AppError::Pty(format!("master poisoned: {e}")))?;
            m.resize(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|e| AppError::Pty(format!("resize: {e}")))?;
            Ok(())
        })
        .await
        .map_err(|e| AppError::Pty(format!("blocking task: {e}")))??;

        if let Ok(mut last) = last_size.lock() {
            *last = (rows, cols);
        }
        Ok(())
    }

    pub async fn shutdown(&self) -> AppResult<()> {
        let handle = {
            let mut guard = self.inner.lock().await;
            guard.take()
        };

        if let Some(handle) = handle {
            handle.cancel.cancel();
            let killer = handle.killer.clone();
            let _ = tokio::task::spawn_blocking(move || {
                if let Ok(mut k) = killer.lock() {
                    let _ = k.kill();
                }
            })
            .await;
            debug!("PTY session shut down");
        }
        Ok(())
    }

}

impl Default for PtyManager {
    fn default() -> Self {
        Self::new()
    }
}

/// Resolve a command name (e.g. "claude", "aider") via PATH.
pub fn resolve_command(name: &str) -> AppResult<std::path::PathBuf> {
    which::which(name).map_err(|_| AppError::CommandNotFound(name.to_string()))
}

#[allow(dead_code)] // kept for parity with v1; current callers go via `resolve_command`
pub fn working_dir_from(path: &Path) -> std::path::PathBuf {
    path.to_path_buf()
}
