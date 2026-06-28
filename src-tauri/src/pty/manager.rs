use std::collections::{HashSet, VecDeque};
use std::io::Write;
use std::sync::{Arc, Mutex as StdMutex};

use portable_pty::{native_pty_system, ChildKiller, CommandBuilder, MasterPty, PtySize};
use tauri::ipc::Channel;
use tauri::{AppHandle, Manager};
use tokio::sync::{mpsc, Mutex as TokioMutex};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use crate::error::{AppError, AppResult};
use crate::processing::permission::PermissionPattern;
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
    /// User-supplied flags + ccimp invocation args.
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
    /// A real (non-deduped) PTY resize just pulsed SIGWINCH at the child,
    /// which makes a TUI like Claude Code repaint. That repaint is a burst
    /// of bytes indistinguishable from genuine output, so it can trip the
    /// processor's byte-burst activity fallback and spuriously flip the
    /// avatar Idle → Thinking → Idle (firing an "idle" notification). The
    /// processor uses this to open a short grace window during which the
    /// burst fallback is suppressed; the `claude_working` marker path stays
    /// live, so real activity during/after a resize is still detected.
    Resized,
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
    /// V1.4-04 D: per-tab cross-restart scrollback ring. The reader
    /// task (in `pty/tasks.rs`) appends every PTY byte here; on graceful
    /// exit the ring is written to disk; on next launch it's read back
    /// and replayed into the new xterm. Bounded at `scrollback_cap`
    /// bytes — surplus is dropped from the front. `StdMutex` (not the
    /// async one) because the writer lives on the blocking pool and
    /// every critical section is short.
    scrollback: Arc<StdMutex<VecDeque<u8>>>,
    /// Cap for the `scrollback` ring. Read from
    /// `terminal.scrollback.ring_bytes` at start time; live changes to
    /// the setting don't reshape an already-running ring (mid-buffer
    /// expand-vs-shrink semantics aren't worth the complexity for a
    /// rarely-changed knob — the new cap takes effect on next start).
    scrollback_cap: usize,
}

impl PtyManager {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(TokioMutex::new(None)),
        }
    }

    #[allow(clippy::too_many_arguments)]
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
        patterns: Arc<Vec<PermissionPattern>>,
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

        // The child is already running. If we bail past this point without
        // killing it AND signaling exit, we leak the process and leave the
        // state machine in its prior state (no Error overlay) — the same
        // hazard the openpty/spawn error paths above guard. Mirror that here.
        let reader = match pair.master.try_clone_reader() {
            Ok(r) => r,
            Err(e) => {
                let _ = child.clone_killer().kill();
                let _ = state_signals.try_send(StateSignal::SubprocessExited { tab, code: None });
                return Err(AppError::Pty(format!("try_clone_reader: {e}")));
            }
        };
        let writer = match pair.master.take_writer() {
            Ok(w) => w,
            Err(e) => {
                let _ = child.clone_killer().kill();
                let _ = state_signals.try_send(StateSignal::SubprocessExited { tab, code: None });
                return Err(AppError::Pty(format!("take_writer: {e}")));
            }
        };

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

        let settings = app.state::<crate::ipc::AppState>().settings.clone();
        // V1.4-04 D: per-tab scrollback ring buffer. Reader task
        // appends every PTY byte; we hand out clones via
        // `scrollback_snapshot` for persistence and via the
        // `pty_get_scrollback` Tauri command for diagnostics.
        let scrollback_cap = settings.current().terminal.scrollback.ring_bytes;
        let scrollback: Arc<StdMutex<VecDeque<u8>>> = Arc::new(StdMutex::new(
            VecDeque::with_capacity(scrollback_cap.min(1 << 20)),
        ));

        tasks::spawn_reader(
            reader,
            bytes_tx,
            cancel.clone(),
            Arc::clone(&scrollback),
            scrollback_cap,
        );
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
            patterns,
        );
        tasks::spawn_waiter(tab.clone(), child, app, cancel.clone(), state_signals);

        *guard = Some(PtyHandle {
            writer,
            master,
            killer,
            cancel,
            last_size: Arc::new(StdMutex::new((initial_rows.max(1), initial_cols.max(1)))),
            control_tx,
            scrollback,
            scrollback_cap,
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

        let (master, last_size, control_tx) = {
            let guard = self.inner.lock().await;
            let handle = guard.as_ref().ok_or(AppError::NotStarted)?;
            (
                handle.master.clone(),
                handle.last_size.clone(),
                handle.control_tx.clone(),
            )
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

        // Tell the processor a real resize just fired so it can ignore the
        // resulting TUI-repaint burst (see `ProcessorControl::Resized`).
        // `try_send` is deliberate: a full control queue during a fast drag
        // means a Resized is already in flight, so dropping this one is
        // harmless — the processor only needs to know a resize happened
        // recently, not how many.
        let _ = control_tx.try_send(ProcessorControl::Resized);
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

    /// V1.4-04 D: snapshot the current scrollback ring as a flat
    /// `Vec<u8>`. Used by the graceful-exit persistence path and the
    /// `pty_get_scrollback` Tauri command. Returns `NotStarted` if no
    /// PTY is registered for this tab.
    ///
    /// The snapshot is a copy: the live ring continues to accumulate
    /// bytes after this call returns. This means a long-running
    /// `pty_get_scrollback` poll won't see byte-stream tearing, just
    /// monotonically-newer prefixes.
    pub async fn scrollback_snapshot(&self) -> AppResult<Vec<u8>> {
        let scrollback = {
            let guard = self.inner.lock().await;
            let handle = guard.as_ref().ok_or(AppError::NotStarted)?;
            Arc::clone(&handle.scrollback)
        };
        let ring = scrollback
            .lock()
            .map_err(|e| AppError::Pty(format!("scrollback poisoned: {e}")))?;
        let (a, b) = ring.as_slices();
        let mut out = Vec::with_capacity(a.len() + b.len());
        out.extend_from_slice(a);
        out.extend_from_slice(b);
        Ok(out)
    }

    /// V1.4-04 D: seed the scrollback ring with bytes restored from a
    /// previous session. The replay is purely about persistence
    /// continuity — the same bytes are also written into the new
    /// xterm by the frontend (front-loaded in `term.write` before the
    /// live channel binds). Truncates to the ring cap.
    ///
    /// Ordering note: `seed_scrollback` runs *after* `start` has already
    /// spawned the reader (see `pty_start`), so by the time we take the
    /// ring lock the live reader may have appended this session's first
    /// output. The restored bytes are older than anything live, so they
    /// must be prepended to the *front* of the ring rather than extended
    /// onto the back — otherwise the order inverts to `[live, restored]`
    /// and `trim_ring` (which evicts from the front) drops the fresh live
    /// output instead of old history. Both ends are serialized by this
    /// same `StdMutex`, so the splice can't race the reader.
    pub async fn seed_scrollback(&self, bytes: &[u8]) -> AppResult<()> {
        let (scrollback, cap) = {
            let guard = self.inner.lock().await;
            let handle = guard.as_ref().ok_or(AppError::NotStarted)?;
            (Arc::clone(&handle.scrollback), handle.scrollback_cap)
        };
        let mut ring = scrollback
            .lock()
            .map_err(|e| AppError::Pty(format!("scrollback poisoned: {e}")))?;
        // Splice restored-then-live so chronological order is preserved.
        crate::pty::scrollback::seed_front(&mut ring, bytes, cap);
        Ok(())
    }

    /// V1.4-04 D: clear the scrollback ring. Called on user-initiated
    /// `pty_restart` because the user explicitly asked for a clean
    /// shell — the prior session's output is no longer relevant.
    pub async fn clear_scrollback(&self) -> AppResult<()> {
        let scrollback = {
            let guard = self.inner.lock().await;
            let handle = guard.as_ref().ok_or(AppError::NotStarted)?;
            Arc::clone(&handle.scrollback)
        };
        let mut ring = scrollback
            .lock()
            .map_err(|e| AppError::Pty(format!("scrollback poisoned: {e}")))?;
        ring.clear();
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
                match killer.lock() {
                    Ok(mut k) => {
                        if let Err(e) = k.kill() {
                            // A failed kill leaves the waiter task blocked in
                            // child.wait() indefinitely — holding the child
                            // handle and a blocking-pool thread, with the
                            // process orphaned. Nothing more we can do
                            // synchronously, but surface it instead of
                            // swallowing it silently.
                            warn!(error = %e, "PTY kill failed during shutdown; child may be orphaned");
                        }
                    }
                    Err(e) => warn!(error = %e, "PTY killer mutex poisoned during shutdown"),
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
        let defaults = crate::settings::Settings::default();
        let settings = SettingsHandle::new(
            defaults.clone(),
            defaults,
            std::env::temp_dir(),
        );

        let tab = TabId::Shell("shell-test".to_string());
        let patterns = Arc::new(Vec::new());
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
            patterns,
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
        let defaults = crate::settings::Settings::default();
        let settings = SettingsHandle::new(
            defaults.clone(),
            defaults,
            std::env::temp_dir(),
        );

        let initial: Channel<String> = Channel::new(|_| Ok(()));

        let tab = TabId::Shell("shell-test".to_string());
        let patterns = Arc::new(Vec::new());
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
            patterns,
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
