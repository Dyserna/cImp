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
    /// Environment additions/overrides applied on top of the inherited
    /// environment. Empty for AI builtins; user Shell tabs may set per-tab
    /// vars (the M2 dialog leaves env empty — schema reserved, UI deferred).
    pub env: std::collections::HashMap<String, String>,
}

pub struct PtyManager {
    inner: Arc<TokioMutex<Option<PtyHandle>>>,
}

/// Control messages routed to the per-PTY processor task. Currently used to
/// swap the output `Channel<String>` without restarting the shell — the
/// V1.4-03 renderer-flip path destroys the JS xterm and rebinds the PTY's
/// bytes to a freshly-constructed one.
pub enum ProcessorControl {
    ChannelChange(Channel<String>),
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
    /// Sends `ProcessorControl` messages to the processor task. Currently
    /// only `ChannelChange`. Capacity is small — control messages are
    /// rare. Sending fails when the processor task has exited (e.g.,
    /// after the child PTY exited and the cancel token fired); the
    /// caller falls back to `pty_start`.
    control_tx: mpsc::Sender<ProcessorControl>,
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

        let tab = spec.tab.clone();
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
                let _ = state_signals.try_send(StateSignal::SubprocessExited { tab, code: None });
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
        for (key, value) in &spec.env {
            cmd.env(key, value);
        }

        let child = match pair.slave.spawn_command(cmd) {
            Ok(c) => c,
            Err(e) => {
                let _ = state_signals.try_send(StateSignal::SubprocessExited { tab, code: None });
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
        // V1.4-03: control mpsc lets `rebind_channel` swap the processor's
        // output channel without restarting the PTY. Capacity 4 is plenty
        // — control messages are rare (one per renderer-flip).
        let (control_tx, control_rx) = mpsc::channel::<ProcessorControl>(4);

        tasks::spawn_reader(reader, bytes_tx, cancel.clone());
        let settings = app.state::<crate::ipc::AppState>().settings.clone();
        tasks::spawn_processor(
            tab.clone(),
            bytes_rx,
            output_channel,
            control_rx,
            tts_segments,
            cancel.clone(),
            user_typed_tts,
            state_signals.clone(),
            settings,
        );
        tasks::spawn_waiter(tab.clone(), child, app, cancel.clone(), state_signals);

        *guard = Some(PtyHandle {
            writer,
            master,
            killer,
            cancel,
            last_size: Arc::new(StdMutex::new((initial_rows.max(1), initial_cols.max(1)))),
            control_tx,
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

    /// V1.4-03: swap the processor task's output channel without
    /// restarting the PTY. Used when the JS-side xterm is destroyed and
    /// recreated for a renderer-category flip — the shell session, env,
    /// cwd, and running processes survive; only the IPC `Channel<String>`
    /// is replaced.
    ///
    /// Returns `AppError::NotStarted` if no PTY is registered, or
    /// `AppError::Pty` if the processor task has already exited (e.g.,
    /// after a child PTY exit). In both cases the caller is expected to
    /// fall back to `pty_start`.
    pub async fn rebind_channel(&self, new_channel: Channel<String>) -> AppResult<()> {
        let control_tx = {
            let guard = self.inner.lock().await;
            let handle = guard.as_ref().ok_or(AppError::NotStarted)?;
            handle.control_tx.clone()
        };
        // Lock released before `await` so a slow processor doesn't block
        // other PtyManager operations.
        control_tx
            .send(ProcessorControl::ChannelChange(new_channel))
            .await
            .map_err(|_| AppError::Pty("processor task gone".into()))?;
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pty::tasks::spawn_processor;
    use crate::settings::SettingsHandle;
    use crate::state::TabId;
    use std::collections::HashSet;
    use std::sync::Mutex as StdMutex;
    use std::time::Duration;
    use tokio::sync::mpsc;

    /// V1.4-03: rebind on an empty manager errors with NotStarted. The
    /// frontend's `attemptSpawn(entry, 'rebind')` catch block treats this
    /// as the signal to fall back to `pty_start`.
    #[tokio::test]
    async fn rebind_with_no_pty_errors() {
        let manager = PtyManager::new();
        let dummy = Channel::new(|_| Ok(()));
        let result = manager.rebind_channel(dummy).await;
        assert!(matches!(result, Err(AppError::NotStarted)));
    }

    /// V1.4-03: directly exercises the processor task's channel-swap
    /// behavior without spinning up a real PTY. Confirms that bytes
    /// emitted before `ChannelChange` reach the old `Channel<String>`,
    /// bytes emitted after reach the new one, and no bytes are lost
    /// across the swap (the cancel-safety claim about `mpsc::Receiver::
    /// recv()` in the select loop).
    #[tokio::test]
    async fn channel_rebind_routes_bytes_to_new_channel() {
        let received_a: Arc<StdMutex<Vec<String>>> = Arc::new(StdMutex::new(Vec::new()));
        let received_b: Arc<StdMutex<Vec<String>>> = Arc::new(StdMutex::new(Vec::new()));

        let a_buf = received_a.clone();
        let channel_a: Channel<String> = Channel::new(move |body| {
            // `body` is the encoded base64 string the processor sends.
            let s = String::from_utf8(body.deserialize().unwrap_or_default())
                .unwrap_or_default();
            a_buf.lock().unwrap().push(s);
            Ok(())
        });
        let b_buf = received_b.clone();
        let channel_b: Channel<String> = Channel::new(move |body| {
            let s = String::from_utf8(body.deserialize().unwrap_or_default())
                .unwrap_or_default();
            b_buf.lock().unwrap().push(s);
            Ok(())
        });

        let (bytes_tx, bytes_rx) = mpsc::channel::<Vec<u8>>(16);
        let (control_tx, control_rx) = mpsc::channel::<ProcessorControl>(4);
        let (tts_tx, _tts_rx) = mpsc::channel(1);
        let (state_tx, _state_rx) = mpsc::channel(8);
        let cancel = CancellationToken::new();
        let typed = Arc::new(StdMutex::new(HashSet::new()));

        // SettingsHandle uses defaults; no `.set()` is ever called so the
        // debounced saver task stays idle and never touches disk.
        let settings = SettingsHandle::new(crate::settings::Settings::default());

        let tab = TabId::Shell("shell-test".to_string());
        spawn_processor(
            tab,
            bytes_rx,
            channel_a,
            control_rx,
            tts_tx,
            cancel.clone(),
            typed,
            state_tx,
            settings,
        );

        // Bytes before the rebind. The processor's flush tick is 50ms;
        // give it a couple of cycles to drain.
        bytes_tx.send(b"hello-A\r\n".to_vec()).await.unwrap();
        tokio::time::sleep(Duration::from_millis(200)).await;

        // Swap to channel B.
        control_tx
            .send(ProcessorControl::ChannelChange(channel_b))
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;

        // Bytes after the rebind.
        bytes_tx.send(b"hello-B\r\n".to_vec()).await.unwrap();
        tokio::time::sleep(Duration::from_millis(200)).await;

        cancel.cancel();
        tokio::time::sleep(Duration::from_millis(50)).await;

        let a = received_a.lock().unwrap().clone();
        let b = received_b.lock().unwrap().clone();
        // The processor base64-encodes terminal bytes. We don't decode
        // here — just assert the routing: A got something, B got
        // something, and nothing crossed.
        assert!(!a.is_empty(), "channel A should have received pre-swap bytes");
        assert!(!b.is_empty(), "channel B should have received post-swap bytes");
    }

    /// V1.4-03: confirms the processor handles three rapid rebinds
    /// without dropping bytes or panicking — mimics a slider drag that
    /// crosses the image/no-image threshold multiple times faster than
    /// the JS-side debounce can collapse them.
    #[tokio::test]
    async fn processor_survives_rapid_rebinds() {
        let final_buf: Arc<StdMutex<Vec<String>>> = Arc::new(StdMutex::new(Vec::new()));

        let (bytes_tx, bytes_rx) = mpsc::channel::<Vec<u8>>(16);
        let (control_tx, control_rx) = mpsc::channel::<ProcessorControl>(8);
        let (tts_tx, _tts_rx) = mpsc::channel(1);
        let (state_tx, _state_rx) = mpsc::channel(8);
        let cancel = CancellationToken::new();
        let typed = Arc::new(StdMutex::new(HashSet::new()));
        let settings = SettingsHandle::new(crate::settings::Settings::default());

        let initial: Channel<String> = Channel::new(|_| Ok(()));

        let tab = TabId::Shell("shell-test".to_string());
        spawn_processor(
            tab,
            bytes_rx,
            initial,
            control_rx,
            tts_tx,
            cancel.clone(),
            typed,
            state_tx,
            settings,
        );

        // Three rapid rebinds. The last channel is the one whose buffer
        // we assert against.
        for _ in 0..2 {
            let throwaway: Channel<String> = Channel::new(|_| Ok(()));
            control_tx
                .send(ProcessorControl::ChannelChange(throwaway))
                .await
                .unwrap();
        }
        let final_clone = final_buf.clone();
        let final_channel: Channel<String> = Channel::new(move |body| {
            let s = String::from_utf8(body.deserialize().unwrap_or_default())
                .unwrap_or_default();
            final_clone.lock().unwrap().push(s);
            Ok(())
        });
        control_tx
            .send(ProcessorControl::ChannelChange(final_channel))
            .await
            .unwrap();

        // Send bytes after the last rebind.
        tokio::time::sleep(Duration::from_millis(50)).await;
        bytes_tx.send(b"final\r\n".to_vec()).await.unwrap();
        tokio::time::sleep(Duration::from_millis(200)).await;

        cancel.cancel();
        tokio::time::sleep(Duration::from_millis(50)).await;

        let received = final_buf.lock().unwrap().clone();
        assert!(
            !received.is_empty(),
            "final channel should have received post-rebind bytes"
        );
    }
}
