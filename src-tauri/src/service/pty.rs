//! The PTY streaming use cases: start, restart, and re-point a live session's
//! output at a fresh sink.
//!
//! ## What the sizing spike found here
//!
//! The tab-lifecycle slice was coupled to Tauri only through `AppState`. This
//! one is coupled for real, and in a way the inventory ("one `Channel<String>`
//! per PTY bound to `AppHandle`") understates: the `AppHandle` threaded down
//! into [`crate::pty::PtyManager::start`] was doing **two unrelated jobs**.
//!
//! 1. *Emitter* — `spawn_waiter` ends a session with `app.emit("pty-exit", …)`.
//!    That is what [`EventSink`] is for, and it is one line.
//! 2. *Service locator* — four `app.state::<T>()` / `app.try_state::<T>()`
//!    lookups inside the spawn: the settings handle (twice), the warm
//!    `GraphService` for the transcript tap's memory writes, and the offload
//!    service's push registry for the session-push fanout. None of those is a
//!    UI concern; the `AppHandle` was standing in for a DI container, in the
//!    middle of the code the H1-R3 note asks to keep short and synchronous.
//!
//! Job 2 is the expensive half to unpick and the one an "events behind a trait"
//! plan would have missed entirely. It is now [`PtyHost`], resolved ONCE at the
//! Tauri boundary by [`PtyHost::from_app`] and passed down as a value — so the
//! lookups did not disappear, they collapsed into a single site that a test can
//! substitute. The settings handle stopped being looked up at all: the caller
//! already had it and was passing it in beside the handle it re-derived it
//! from.
//!
//! ## What did NOT change
//!
//! The synchronous stretch between the child spawn and the transcript tap's
//! registration (H1-R3): [`PtyHost`] is destructured on `start`'s first lines,
//! before the spawn, exactly as `PtyStart` already was, and nothing fallible
//! moved into the gap. `PtyHost::from_app` runs in the IPC wrapper, before the
//! registry lock is even taken.

use std::path::Path;
use std::sync::Arc;

use tokio::sync::mpsc;

use crate::error::{AppError, AppResult};
use crate::pty::PtyHost;
use crate::service::sink::OutputSink;
use crate::settings::SettingsHandle;
use crate::state::{TabActivity, TabId};
use crate::tabs::registry::TabStart;
use crate::tabs::TabRegistryHandle;
use crate::tts::TtsRequest;

/// The PTY use cases, over borrowed handles — same shape and rationale as
/// [`crate::service::tabs::TabService`].
pub struct PtyService<'a> {
    registry: &'a TabRegistryHandle,
    settings: &'a SettingsHandle,
    tab_activity: &'a TabActivity,
    tts_segments: &'a mpsc::Sender<TtsRequest>,
    launch_cwd: &'a Path,
    invocation_args: &'a [String],
}

impl<'a> PtyService<'a> {
    pub fn new(
        registry: &'a TabRegistryHandle,
        settings: &'a SettingsHandle,
        tab_activity: &'a TabActivity,
        tts_segments: &'a mpsc::Sender<TtsRequest>,
        launch_cwd: &'a Path,
        invocation_args: &'a [String],
    ) -> Self {
        Self {
            registry,
            settings,
            tab_activity,
            tts_segments,
            launch_cwd,
            invocation_args,
        }
    }

    fn tab_start(
        &self,
        host: PtyHost,
        tab: TabId,
        output: Arc<dyn OutputSink>,
        rows: u16,
        cols: u16,
        start_gen: u64,
    ) -> TabStart<'a> {
        TabStart {
            host,
            tab,
            output,
            rows,
            cols,
            launch_cwd: self.launch_cwd,
            invocation_args: self.invocation_args,
            tts_segments: self.tts_segments.clone(),
            settings: self.settings.clone(),
            start_gen,
        }
    }

    /// Spawn a tab's subprocess and replay any scrollback persisted by the
    /// previous session.
    ///
    /// Returns the persisted bytes (if any). The frontend writes them to the
    /// new xterm before the live sink binds so the user sees their previous
    /// shell output above the fresh prompt. The bytes are also seeded into the
    /// new ring buffer so a subsequent crash-restart preserves continuity
    /// (capped at the ring size, naturally).
    ///
    /// `None` when:
    ///   - `terminal.scrollback.restore_on_launch` is `false`
    ///   - no persisted file exists for this tab (cold install, or already
    ///     consumed earlier in this session)
    ///   - reading the file failed (logged at warn; treated as cold start)
    pub async fn start(
        &self,
        host: PtyHost,
        tab: TabId,
        output: Arc<dyn OutputSink>,
        rows: u16,
        cols: u16,
    ) -> AppResult<Option<Vec<u8>>> {
        let restore_on_launch = self
            .settings
            .current()
            .terminal
            .scrollback
            .restore_on_launch;

        // V39 review R-5: seed the activity mirror for THIS start and carry its
        // generation into the spawn, so an exit belonging to an earlier start of
        // the same tab can be recognised as late rather than latched onto this one.
        let start_gen = self.tab_activity.begin_start(&tab);
        {
            let registry = self.registry.lock().await;
            registry
                .start_tab(self.tab_start(host, tab.clone(), output, rows, cols, start_gen))
                .await?;
        }

        // V1.4-04 D.5: read any persisted scrollback for this tab. Done
        // after a successful start so a spawn failure doesn't burn the
        // bytes. V0.6+: read-then-delete is split — we only delete the
        // on-disk file after `seed_scrollback` returns Ok, so a transient
        // seed failure (poisoned mutex, ring contention) leaves the file
        // in place for the next launch to retry rather than dropping the
        // user's scrollback between read and seed.
        if !restore_on_launch {
            return Ok(None);
        }
        // Read the scrollback file WITHOUT holding the registry lock: it's a
        // synchronous `fs::read` of the whole file and only needs `&tab`. Holding
        // the single registry TokioMutex across it (as the original code did)
        // stalls every other registry-touching command (pty_write, pty_resize,
        // tab_activate, …) behind disk latency / AV scans. Run it on the blocking
        // pool so a large scrollback under slow / AV-scanned disk doesn't stall
        // other IPC futures on this tokio worker either. Re-acquire the registry
        // lock only for the seed, which does touch the registry.
        let restored = {
            let tab_for_read = tab.clone();
            tokio::task::spawn_blocking(move || crate::pty::scrollback::read(&tab_for_read))
                .await
                .map_err(|e| AppError::Pty(format!("scrollback read join: {e}")))?
        };
        if let Some(bytes) = &restored {
            let registry = self.registry.lock().await;
            match registry.seed_scrollback(&tab, bytes).await {
                Ok(()) => crate::pty::scrollback::consume_after_read(&tab),
                Err(e) => {
                    tracing::warn!(?tab, error = %e, "scrollback seed failed; on-disk copy retained for retry");
                }
            }
        }
        Ok(restored)
    }

    /// Tear the tab's subprocess down and bring a fresh one up on `output`.
    pub async fn restart(
        &self,
        host: PtyHost,
        tab: TabId,
        output: Arc<dyn OutputSink>,
        rows: u16,
        cols: u16,
    ) -> AppResult<()> {
        // V39 review HIGH-3 + R-5: re-seed the activity mirror for the fresh
        // subprocess, BEFORE it is spawned.
        //
        // `TabActivity::exited` is latched, and the two signals that clear it do
        // not cover this path: `TabAdded` fires for a NEW tab, and `ShellRestarted`
        // is emitted for Shell-kind tabs only (`TabRegistry::restart_tab`). An AI
        // tab — the only kind a delegation can drive — therefore restarted into a
        // row still marked `exited`, and preflight refused it forever with "has no
        // running process".
        //
        // Before the spawn rather than after it (R-5), and with a generation the
        // spawn carries: clearing afterwards raced the old child's exit through the
        // state-manager mpsc, which re-latched `exited` on the process that had
        // just started. A failed restart re-latches it honestly — its own failure
        // path emits an exit under THIS generation.
        let start_gen = self.tab_activity.begin_start(&tab);
        let registry = self.registry.lock().await;
        let result = registry
            .restart_tab(self.tab_start(host, tab.clone(), output, rows, cols, start_gen))
            .await;
        // V1.4-04 D.6: on user-initiated restart, the prior session's
        // scrollback is no longer relevant. Clear the in-memory ring so
        // the next graceful-exit persist doesn't include stale bytes from
        // before the restart. Done regardless of whether the restart
        // succeeded — the user explicitly asked for a clean shell.
        if let Err(e) = registry.clear_scrollback(&tab).await {
            tracing::warn!(?tab, error = %e, "scrollback clear after restart failed");
        }
        result
    }

    /// V1.4-03: re-point a still-running PTY's bytes at a fresh sink without
    /// restarting the shell. The frontend invokes this when the xterm.js
    /// Terminal is destroyed and recreated for a renderer-category flip
    /// (background image toggled on or off). The shell session, env, cwd, and
    /// any in-flight processes survive; only the sink is replaced.
    pub async fn rebind(&self, tab: TabId, output: Arc<dyn OutputSink>) -> AppResult<()> {
        let registry = self.registry.lock().await;
        registry.rebind_channel(tab, output).await
    }
}
