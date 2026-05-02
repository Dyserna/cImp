use std::collections::HashSet;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex as StdMutex};

use portable_pty::{native_pty_system, ChildKiller, CommandBuilder, MasterPty, PtySize};
use tauri::ipc::Channel;
use tauri::AppHandle;
use tokio::sync::{mpsc, Mutex as TokioMutex};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info};

use crate::error::{AppError, AppResult};
use crate::pty::tasks;
use crate::state::StateSignal;

fn resolve_claude_path() -> AppResult<PathBuf> {
    which::which("claude").map_err(|_| AppError::ClaudeNotFound)
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
    /// (e.g. mid-drag across DPI boundaries) doesn't pulse SIGWINCH at
    /// claude and trigger a TUI redraw.
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
        output_channel: Channel<String>,
        working_dir: &Path,
        extra_args: Vec<String>,
        initial_rows: u16,
        initial_cols: u16,
        tts_segments: tokio::sync::mpsc::Sender<String>,
        user_typed_tts: Arc<StdMutex<HashSet<String>>>,
        state_signals: tokio::sync::mpsc::Sender<StateSignal>,
    ) -> AppResult<()> {
        let mut guard = self.inner.lock().await;
        if guard.is_some() {
            return Err(AppError::AlreadyStarted);
        }

        let claude_path = resolve_claude_path()?;
        info!(path = %claude_path.display(), "resolved claude binary");

        let pty_system = native_pty_system();
        let pair = pty_system
            .openpty(PtySize {
                rows: initial_rows.max(1),
                cols: initial_cols.max(1),
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|e| AppError::Pty(format!("openpty: {e}")))?;

        let mut cmd = CommandBuilder::new(&claude_path);
        // Inject the TTS markup convention as an appended system prompt. This
        // is a runtime-only thing — we don't ship a CLAUDE.md, so the user's
        // own project context (whatever directory they launch cctts from)
        // stays untouched and the embedded claude still knows about the tags.
        cmd.arg("--append-system-prompt");
        cmd.arg(crate::tts::RUNTIME_SYSTEM_PROMPT);
        for arg in &extra_args {
            cmd.arg(arg);
        }
        cmd.cwd(working_dir);

        let child = pair
            .slave
            .spawn_command(cmd)
            .map_err(|e| AppError::Spawn(format!("{e}")))?;
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
        tasks::spawn_processor(
            bytes_rx,
            output_channel,
            tts_segments,
            cancel.clone(),
            user_typed_tts,
            state_signals.clone(),
        );
        tasks::spawn_waiter(child, app, cancel.clone(), state_signals);

        *guard = Some(PtyHandle {
            writer,
            master,
            killer,
            cancel,
            last_size: Arc::new(StdMutex::new((initial_rows.max(1), initial_cols.max(1)))),
        });
        info!(
            rows = initial_rows,
            cols = initial_cols,
            args = ?extra_args,
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

        // Same-size resize → skip. This filters the chain of micro-resize
        // events Windows fires when a window crosses a DPI boundary (and
        // any other ResizeObserver event where the terminal grid happens
        // to match what we already had).
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
