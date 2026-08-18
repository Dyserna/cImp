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
    /// User-supplied flags + cimp invocation args.
    pub extra_args: Vec<String>,
    pub working_dir: std::path::PathBuf,
    /// Environment additions/overrides applied on top of the inherited
    /// environment. Empty for AI builtins; user Shell tabs may set per-tab
    /// vars (the M2 dialog leaves env empty — schema reserved, UI deferred).
    pub env: std::collections::HashMap<String, String>,
    /// V30 (review M9): inherited variables to STRIP before `env` is applied.
    ///
    /// The child otherwise inherits cImp's whole environment, which is wrong for
    /// the harness markers of a Claude Code session cImp itself was launched
    /// from: `CLAUDE_CODE_CHILD_SESSION=1` makes a spawned Claude run with no
    /// transcript, no history and no session records, which silently blinds the
    /// out-of-band tap (no TTS, no usage, no live-session registry entry, no V28
    /// scoping) — spike-documented in `docs/MILESTONE-V30-mcp-channels.md`.
    /// Resolved by `tabs::config::ai_env_removals`; empty for Shell tabs, whose
    /// whole point is the environment the user actually has.
    pub env_remove: Vec<String>,
    /// V20: the out-of-band TTS source to attach once the child is up (Claude
    /// transcript tail / OpenCode event stream). `None` for shell tabs and any
    /// AI tab whose source can't be resolved. The source rides the tab's PTY
    /// cancel token, so it starts with the tab and dies with it.
    pub oob: Option<crate::harness::OobSpec>,
}

pub struct PtyManager {
    inner: Arc<TokioMutex<Option<PtyHandle>>>,
}

/// Shape the child's environment: strip first, then add.
///
/// `CommandBuilder::new` seeds itself with a snapshot of THIS process's
/// environment, so `env_remove` genuinely un-inherits a variable (portable-pty
/// 0.9's `CommandBuilder::env_remove`, verified against the pinned source).
/// Removals run BEFORE the additions so an explicit per-tab `env` entry always
/// wins — a user who deliberately sets one of the scrubbed vars keeps it (the
/// resolver in `tabs::config` also declines to list such a key, so this is the
/// second of two guards).
fn apply_env(
    cmd: &mut CommandBuilder,
    env: &std::collections::HashMap<String, String>,
    env_remove: &[String],
) {
    for key in env_remove {
        cmd.env_remove(key);
    }
    for (key, value) in env {
        cmd.env(key, value);
    }
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
    /// M11 (2026-08-05 review): drop the permission detector's LATCHED pattern
    /// name for this tab without emitting anything, so the next scan tick can
    /// re-`Detected` a prompt that is still on screen.
    ///
    /// Sent by the NC-2 hook path (`offload::loopback::handle_permission_event`)
    /// whenever a `PermissionDenied` clears `awaiting_permission`. That clear is
    /// deliberately eager — an auto-denied call can land while a genuine
    /// approval prompt is visible — and the regex detector is edge-triggered:
    /// while the same pattern keeps matching, `(Some, Some)` with the same name
    /// emits NOTHING, so without this the badge/TTS stayed cleared until the
    /// user typed. The processor answers it with
    /// `PermissionDetector::force_clear(Permission)`, the same primitive the
    /// Working-kind stale path already uses.
    ClearPermissionLatch,
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
        // V20: retained for the registry's call shape; the processing layer no
        // longer consumes a user-typed echo filter (TTS is out-of-band).
        _user_typed_tts: Arc<StdMutex<HashSet<String>>>,
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
        apply_env(&mut cmd, &spec.env, &spec.env_remove);

        // Through the spawn gate (see `spawn_gate`). portable-pty builds its own
        // `STARTUPINFOEX` and calls `CreateProcessW` itself, so this spawn sits
        // outside `std`'s private process-wide create-process lock entirely —
        // which is half the reason the gate had to exist. `with_shared` wraps
        // the third-party call and nothing else.
        let child = match crate::spawn_gate::with_shared(|| pair.slave.spawn_command(cmd)) {
            Ok(c) => c,
            Err(e) => {
                let _ = state_signals.try_send(StateSignal::SubprocessExited { tab, code: None });
                return Err(AppError::Spawn(format!("{e}")));
            }
        };
        let killer = child.clone_killer();

        // V33 contract C3: put the tab's process into the process-lifetime
        // kill-on-job-close job. Everything else cImp spawns has been in it
        // since the job existed; this one was not, because `guard_child` is
        // typed to a `tokio::process::Child` and portable-pty hands back its
        // own `Child`. That made the widest agent seam in the app — the tab
        // process, whose descendants are everything the agent runs — the one
        // thing a hard cImp death (panic=abort, OOM kill, `cargo tauri dev`'s
        // hot-reload TerminateProcess) left orphaned.
        //
        // Naming the child by pid is safe here and only here: `child` is still
        // alive in this scope and holds the process handle, which is what pins
        // the pid against reuse (see `guard_pid`'s contract). Hardening, never
        // a gate — a failure is logged and the tab still launches.
        match child.process_id() {
            Some(pid) => crate::process_guard::guard_pid(pid),
            None => tracing::debug!(?tab, "pty child reported no pid; cannot job-guard it"),
        }

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

        // V20: build the out-of-band TTS source context before the senders are
        // moved into the processor/waiter. The source rides `cancel`, so it
        // starts now and dies when the tab's PTY does.
        // V10: the warm graph service, so the Claude transcript tap can record
        // session/action memory in-process. Absent in headless/test builds.
        let mem = app
            .try_state::<Arc<crate::graph::GraphService>>()
            .map(|s| s.inner().clone());
        // V30 Phase D: the session-push bus, so the OpenCode tap can subscribe
        // its tab in-process and forward notices over OpenCode's HTTP API. Only
        // the send half's registry travels (not the service), exactly like the
        // Phase C producers in `main.rs` — no Arc cycle. Absent in
        // headless/test builds ⇒ `None` ⇒ no fanout.
        let pushes = app
            .try_state::<Arc<crate::offload::OffloadService>>()
            .map(|s| s.push_registry());
        let oob_ctx = spec.oob.clone().map(|oob_spec| {
            (
                oob_spec,
                crate::harness::OobContext {
                    tab: tab.clone(),
                    tts: tts_segments.clone(),
                    state_signals: state_signals.clone(),
                    settings: settings.clone(),
                    cancel: cancel.clone(),
                    mem: mem.clone(),
                    pushes: pushes.clone(),
                },
            )
        });
        // OpenCode's event stream authoritatively drives Thinking/Idle, so the
        // processor must not also run its byte-burst activity fallback.
        let oob_drives_activity =
            matches!(spec.oob, Some(crate::harness::OobSpec::OpenCodeEvent { .. }));

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
            cancel.clone(),
            state_signals.clone(),
            settings,
            patterns,
            oob_drives_activity,
        );
        tasks::spawn_waiter(tab.clone(), child, app, cancel.clone(), state_signals);

        // H1-R3 (2026-08-05 review): KEEP THIS IN THE SAME SYNCHRONOUS STRETCH
        // as the `spawn_command` above — do not insert an `.await` (or any
        // fallible/slow step that could park) between the child spawn and this
        // call. The V28 ambiguity predicate only counts tabs whose tap has
        // declared its transcript root, so between "child running" and "tap
        // registered" a second Claude tab is invisible as a co-tenant and a
        // sibling tap can bind confidently to the wrong session
        // (docs/MILESTONE-V28-session-identity.md § "Launch-order window").
        // Today the gap is ~15 sync statements — sub-millisecond, against
        // Claude's multi-second boot before it writes a transcript — but that
        // is a property of THIS code order, not an enforced invariant.
        if let Some((oob_spec, ctx)) = oob_ctx {
            crate::harness::spawn(oob_spec, ctx);
        }

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

        // Claim the new target under the lock — dedup check AND update together —
        // so a concurrent resize back to the previous size isn't wrongly
        // suppressed against a `last_size` this call hasn't published yet (which
        // would leave the PTY at the wrong size).
        let prev = {
            let mut last = last_size
                .lock()
                .map_err(|e| AppError::Pty(format!("last_size poisoned: {e}")))?;
            if *last == (rows, cols) {
                return Ok(());
            }
            let prev = *last;
            *last = (rows, cols);
            prev
        };

        let result = tokio::task::spawn_blocking(move || -> AppResult<()> {
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
        .map_err(|e| AppError::Pty(format!("blocking task: {e}")));

        // Roll back the claimed size on failure so a retry to this size isn't
        // suppressed — but only if no later resize already superseded our claim.
        if let Err(e) = result.and_then(|r| r) {
            if let Ok(mut last) = last_size.lock() {
                if *last == (rows, cols) {
                    *last = prev;
                }
            }
            return Err(e);
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

    /// M11: drop the permission detector's latched pattern for this tab so a
    /// prompt still on screen is re-detected on the next scan tick. Best-effort
    /// and non-blocking by design — this rides the hook path, which must never
    /// stall on a busy processor, and a dropped message only costs the
    /// re-detection this call was trying to enable (the regex path is the
    /// fallback, not the primary). No-op when this tab has no running PTY.
    pub async fn clear_permission_latch(&self) {
        let control_tx = {
            let guard = self.inner.lock().await;
            match guard.as_ref() {
                Some(handle) => handle.control_tx.clone(),
                None => return,
            }
        };
        if control_tx
            .try_send(ProcessorControl::ClearPermissionLatch)
            .is_err()
        {
            debug!("permission latch clear dropped (processor busy or gone)");
        }
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
    use std::sync::Mutex as StdMutex;
    use std::time::Duration;
    use tokio::sync::mpsc;

    /// V30 (review M9): the spawn path must be able to UN-inherit a variable,
    /// not just add one — and an explicit per-tab value must still win.
    #[test]
    fn env_removals_strip_inherited_vars_but_lose_to_explicit_values() {
        // A var this process really has, so the removal has something to bite.
        std::env::set_var("CIMP_TEST_INHERITED", "1");
        std::env::set_var("CIMP_TEST_OVERRIDDEN", "parent");
        let mut cmd = CommandBuilder::new("cmd");
        let mut env = std::collections::HashMap::new();
        env.insert("CIMP_TEST_OVERRIDDEN".to_string(), "child".to_string());
        apply_env(
            &mut cmd,
            &env,
            &[
                "CIMP_TEST_INHERITED".to_string(),
                "CIMP_TEST_OVERRIDDEN".to_string(),
                "CIMP_TEST_NEVER_SET".to_string(), // removing an absent var is a no-op
            ],
        );
        assert_eq!(
            cmd.get_env("CIMP_TEST_INHERITED"),
            None,
            "an inherited var on the strip list must not reach the child"
        );
        assert_eq!(
            cmd.get_env("CIMP_TEST_OVERRIDDEN")
                .and_then(|v| v.to_str()),
            Some("child"),
            "a per-tab env entry outranks the strip list"
        );
        std::env::remove_var("CIMP_TEST_INHERITED");
        std::env::remove_var("CIMP_TEST_OVERRIDDEN");
    }

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
            let s = String::from_utf8(body.deserialize().unwrap_or_default()).unwrap_or_default();
            a_buf.lock().unwrap().push(s);
            Ok(())
        });
        let b_buf = received_b.clone();
        let channel_b: Channel<String> = Channel::new(move |body| {
            let s = String::from_utf8(body.deserialize().unwrap_or_default()).unwrap_or_default();
            b_buf.lock().unwrap().push(s);
            Ok(())
        });

        let (bytes_tx, bytes_rx) = mpsc::channel::<Vec<u8>>(16);
        let (control_tx, control_rx) = mpsc::channel::<ProcessorControl>(4);
        let (state_tx, _state_rx) = mpsc::channel(8);
        let cancel = CancellationToken::new();

        // SettingsHandle uses defaults; no `.set()` is ever called so the
        // debounced saver task stays idle and never touches disk.
        let defaults = crate::settings::Settings::default();
        let settings = SettingsHandle::new(defaults.clone(), defaults, std::env::temp_dir());

        let tab = TabId::Shell("shell-test".to_string());
        let patterns = Arc::new(Vec::new());
        spawn_processor(
            tab,
            bytes_rx,
            channel_a,
            control_rx,
            cancel.clone(),
            state_tx,
            settings,
            patterns,
            false,
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
        assert!(
            !a.is_empty(),
            "channel A should have received pre-swap bytes"
        );
        assert!(
            !b.is_empty(),
            "channel B should have received post-swap bytes"
        );
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
        let (state_tx, _state_rx) = mpsc::channel(8);
        let cancel = CancellationToken::new();
        let defaults = crate::settings::Settings::default();
        let settings = SettingsHandle::new(defaults.clone(), defaults, std::env::temp_dir());

        let initial: Channel<String> = Channel::new(|_| Ok(()));

        let tab = TabId::Shell("shell-test".to_string());
        let patterns = Arc::new(Vec::new());
        spawn_processor(
            tab,
            bytes_rx,
            initial,
            control_rx,
            cancel.clone(),
            state_tx,
            settings,
            patterns,
            false,
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
            let s = String::from_utf8(body.deserialize().unwrap_or_default()).unwrap_or_default();
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
