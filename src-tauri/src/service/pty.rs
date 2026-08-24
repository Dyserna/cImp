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
//!
//! ## The input pipeline (V42 Phase A1-3)
//!
//! The write half arrived later and is the other cross-module invariant this
//! module now holds: **every byte that reaches a tab's PTY passes through
//! [`PtyService::write_through`]** — TTS-marker pre-registration, the
//! typed-input accumulator, the keystroke/submit state signals, and the
//! registry write, in that order and under one registry lock. Two callers enter
//! it: [`PtyService::write`] (the keyboard, which checks the read-only lock
//! first) and the delegation engine (which holds the `Driven` lock itself and
//! would otherwise refuse its own write).
//!
//! Its whole coupling to Tauri was `AppState` — a struct of twenty fields it
//! used three of. The predicates that decide what the lock lets through
//! ([`read_only_refusal`] and the wheel-report family below) came with it,
//! along with their tests, which now live beside the pipeline they constrain
//! rather than in the wire boundary.

use std::path::Path;
use std::sync::atomic::Ordering;
use std::sync::Arc;

use tokio::sync::mpsc;

use crate::error::{AppError, AppResult};
use crate::pty::PtyHost;
use crate::service::sink::OutputSink;
use crate::settings::SettingsHandle;
use crate::state::{
    InputLengths, ReadOnlySource, ReadOnlyTabs, StateSignal, TabActivity, TabId,
};
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
    /// V39 Phase A: which tabs are locked, and by what. Read once per write.
    read_only: &'a ReadOnlyTabs,
    /// The per-tab unsent-input counters behind the idle-Listening heuristic.
    input_lengths: &'a InputLengths,
    /// Where `UserKeystroke` / `UserSubmit` go. In-process; the state manager,
    /// not Tauri, drains it.
    signals: &'a mpsc::Sender<StateSignal>,
}

impl<'a> PtyService<'a> {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        registry: &'a TabRegistryHandle,
        settings: &'a SettingsHandle,
        tab_activity: &'a TabActivity,
        tts_segments: &'a mpsc::Sender<TtsRequest>,
        launch_cwd: &'a Path,
        invocation_args: &'a [String],
        read_only: &'a ReadOnlyTabs,
        input_lengths: &'a InputLengths,
        signals: &'a mpsc::Sender<StateSignal>,
    ) -> Self {
        Self {
            registry,
            settings,
            tab_activity,
            tts_segments,
            launch_cwd,
            invocation_args,
            read_only,
            input_lengths,
            signals,
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

    /// V1.4-04 D.3: snapshot a tab's PTY scrollback as raw bytes. Exposed for
    /// diagnostics and external use; the launch-replay path uses `start`'s own
    /// `Option<Vec<u8>>` return for efficiency. `NotStarted` if the tab has no
    /// live PTY.
    pub async fn scrollback(&self, tab: TabId) -> AppResult<Vec<u8>> {
        let registry = self.registry.lock().await;
        registry.scrollback_snapshot(tab).await
    }

    /// Tell a tab's PTY its viewport changed.
    pub async fn resize(&self, tab: TabId, rows: u16, cols: u16) -> AppResult<()> {
        let registry = self.registry.lock().await;
        registry.resize(tab, rows, cols).await
    }

    /// **The keyboard's write.** Everything a person types reaches the PTY
    /// through here.
    ///
    /// The reserved dashboard tabs are read-only — app-rendered with no PTY of
    /// their own — so any write to one is swallowed. Defense-in-depth behind
    /// the frontend's read-only guard; one shared predicate so a new reserved
    /// dashboard cannot miss the swallow.
    ///
    /// V39 Phase A (locked decision 4): the read-only refusal sits here, ahead
    /// of EVERY side effect in [`write_through`](Self::write_through) — the
    /// TTS-marker registration, the typed-input accumulator, the
    /// keystroke/submit state signals — because a refused keystroke must leave
    /// no trace: an input that never reached the PTY must not have moved the
    /// avatar to Listening or armed a TTS-echo suppression for text the model
    /// never saw. This is the enforcement point; the xterm widget's own gate is
    /// a courtesy that keeps the round trip out of the common case, and is not
    /// what makes the lock hold.
    ///
    /// **Terminal protocol replies are exempt.** xterm answers the *program's*
    /// own queries (cursor-position reports, device attributes, focus in/out)
    /// on this same channel; they are the terminal talking to the TUI, not the
    /// user typing, and refusing them wedges a harness that is waiting for one
    /// — precisely while a delegation is driving it. Same predicate the
    /// keystroke bookkeeping uses, so the two cannot disagree about what counts
    /// as user input.
    pub async fn write(&self, tab: TabId, input: String) -> AppResult<()> {
        if tab.is_reserved_dashboard() {
            return Ok(());
        }

        if let Some(source) = self.read_only.read_only(&tab) {
            // The driver's *name* (not its id) is what the refusal says, and
            // names live in the tab registry. Looked up only on the `Driven`
            // branch so a plain user lock costs no registry lock.
            let driver_name = match &source {
                ReadOnlySource::User => None,
                ReadOnlySource::Driven { by } => {
                    let registry = self.registry.lock().await;
                    registry.name_of(by)
                }
            };
            if let Some(reason) = read_only_refusal(&source, &input, driver_name.as_deref()) {
                tracing::debug!(?tab, %reason, "pty_write refused: tab is read-only");
                return Err(AppError::ReadOnly {
                    tab: tab.as_str().to_string(),
                    reason,
                });
            }
        }

        // The user's own keystrokes: whether this write submits is read off the
        // bytes, exactly as it always was.
        let submit = Submit::from_input(&input);
        self.write_through(&tab, input, submit).await
    }

    /// **The one input pipeline** (V39 cross-module invariant): every byte that
    /// reaches a tab's PTY passes through here — the TTS-marker
    /// pre-registration, the typed-input accumulator, the keystroke/submit
    /// state signals, and the registry write, in that order and under one
    /// registry lock.
    ///
    /// Split out of the write command in V39 Phase B so the delegation engine
    /// can reuse it. What the engine skips is **only** the read-only check in
    /// [`write`](Self::write), and only because that check is about the *user's
    /// keyboard*: the engine holds the `Driven` lock itself, so entering
    /// through `write` would have it refuse its own write. Everything else it
    /// must not skip — a delegated turn that bypassed the TTS-marker
    /// registration would have the worker's echo of the task spoken aloud, and
    /// one that bypassed `UserSubmit` would leave the avatar in the wrong state
    /// for a turn that really did start.
    pub(crate) async fn write_through(
        &self,
        tab: &TabId,
        input: String,
        // V39 review L-1: whether this write SUBMITS. See [`Submit`] — the
        // engine's paste carries newlines, and inferring it from the bytes
        // fired `UserSubmit` one write early.
        submit: Submit,
    ) -> AppResult<()> {
        let tab = tab.clone();

        // Take the registry lock once at the top so the keystroke / submit
        // counter updates and the final write run inside the same critical
        // section. Pre-V0.6 the counter Arc was cloned out under the read
        // lock and used after dropping it, racing with `close_tab` which
        // removes the counter. Holding the lock end-to-end eliminates that
        // window: if the tab was just closed, `registry.write` errors out
        // cleanly with `unknown tab` and no half-applied state remains.
        let registry = self.registry.lock().await;

        let existing = {
            // The counter is only the idle-Listening heuristic; a poisoned lock
            // must NOT gate input delivery, or a prior panic would silently drop
            // all keystrokes for the rest of the session. Recover the inner value
            // (matches the poison-recovery pattern used in `tts_stop`/`mutate`).
            let map = self.input_lengths.read().unwrap_or_else(|e| e.into_inner());
            map.get(&tab).cloned()
        };
        let len_counter = match existing {
            Some(c) => c,
            None => {
                // The state manager may not have drained `TabAdded` yet — there's
                // no happens-before between `create_*_tab` returning and the
                // manager inserting this tab's counter. A missing counter must NOT
                // gate input delivery (it's only the idle-Listening heuristic), or
                // the very first keystrokes into a just-created tab are silently
                // dropped. Lazily insert one; the manager's own insert uses
                // `or_insert_with`, so it won't clobber this.
                let mut map = self.input_lengths.write().unwrap_or_else(|e| e.into_inner());
                map.entry(tab.clone())
                    .or_insert_with(|| Arc::new(std::sync::atomic::AtomicI32::new(0)))
                    .clone()
            }
        };

        if !is_automatic_terminal_response(&input) {
            if submit.is_yes() {
                len_counter.store(0, Ordering::Relaxed);
                let _ = self
                    .signals
                    .try_send(StateSignal::UserSubmit { tab: tab.clone() });
            } else {
                apply_input_delta(&input, &len_counter);
                let _ = self
                    .signals
                    .try_send(StateSignal::UserKeystroke { tab: tab.clone() });
                // Note: typing does NOT interrupt TTS. By design, in-flight
                // speech is only stopped by Esc (`tts_stop`) or by switching
                // tabs (so the previous tab's audio doesn't bleed into the new
                // view). Keystrokes still drive avatar state via UserKeystroke.
            }
        }

        registry.write(tab, input.into_bytes()).await
    }
}

/// **Does this write submit the turn?** (V39 review L-1.)
///
/// It used to be inferred from the bytes in every case, and for a keyboard that
/// is right — a person's Enter IS the submit. For the delegation engine it is
/// not: the engine's PASTE contains the request's own newlines (a multi-line
/// request is one bracketed paste), so `contains_enter` fired on the paste, one
/// write EARLY. The `UserSubmit` that went out then cleared the worker's prompt
/// mirror and zeroed its input counter for a turn that had not been submitted
/// yet — while the engine's real submit, a lone CR a moment later, is what
/// actually starts it.
///
/// So the caller says. `pty_write` keeps the old inference; the engine passes
/// `No` for the paste and `Yes` for the submit.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Submit {
    Yes,
    No,
}

impl Submit {
    /// The keyboard's rule: any CR or LF in the bytes submits.
    pub(crate) fn from_input(input: &str) -> Self {
        if contains_enter(input) {
            Submit::Yes
        } else {
            Submit::No
        }
    }

    fn is_yes(self) -> bool {
        self == Submit::Yes
    }
}

/// V39 Phase A: the read-only decision for one write, as a value.
///
/// `Some(reason)` refuses; `None` lets the write through. Two rules, and the
/// second is the one worth pinning in a test:
///
/// 1. A locked tab refuses the user's input, naming the source.
/// 2. **Terminal protocol replies always pass.** xterm answers the running
///    program's own queries — cursor-position reports, device attributes,
///    focus in/out — over the same channel as keystrokes. Those are the
///    terminal talking to the TUI, not a person typing; a harness that asked
///    for the cursor position and never gets an answer can wedge, and it would
///    wedge exactly while a delegation is driving it. The same predicate the
///    keystroke bookkeeping uses decides this, so the two cannot disagree
///    about what counts as user input.
fn read_only_refusal(
    source: &ReadOnlySource,
    input: &str,
    driver_name: Option<&str>,
) -> Option<String> {
    if read_only_exempt(input) {
        return None;
    }
    Some(source.reason(driver_name))
}

/// Everything the read-only lock lets through. **Only** `read_only_refusal`
/// asks this — `is_automatic_terminal_response` keeps its own, unchanged
/// meaning for the keystroke/submit bookkeeping, which must go on treating a
/// wheel report as the non-typing event it always was.
///
/// Two exemptions, for two different reasons:
///
/// 1. The terminal answering the running program (see `read_only_refusal`).
/// 2. **Wheel reports: scrolling is reading.** A read-only tab exists so the
///    user can watch it, and in an alt-screen TUI the wheel is not local
///    scrollback — it is forwarded to the program as a mouse report, so a
///    swallowed wheel means a tab the user is allowed to watch but not scroll.
///    Mouse *clicks* stay refused: a click activates a control (choosing a
///    permission option, for one), which is exactly the input the lock is for.
fn read_only_exempt(input: &str) -> bool {
    is_automatic_terminal_response(input) || is_mouse_wheel(input)
}

/// Whether `input` is *nothing but* mouse-wheel reports.
///
/// Whole-input, and repeat-until-exhausted rather than "starts with": a chunk
/// that is a wheel report followed by typed text is refused, so the exemption
/// cannot be used to smuggle a keystroke past the lock. Repeats are allowed
/// because a fast scroll can arrive as several reports in one chunk, and
/// letting only the first through would drop the rest silently.
fn is_mouse_wheel(input: &str) -> bool {
    let mut rest = input;
    let mut seen = false;
    while !rest.is_empty() {
        match take_wheel_report(rest) {
            Some(next) => {
                seen = true;
                rest = next;
            }
            None => return false,
        }
    }
    seen
}

/// Consume one leading wheel report, returning what follows it.
///
/// Handles both encodings xterm can emit: SGR (`ESC [ < Cb ; Cx ; Cy M`) and
/// the legacy X10/normal one (`ESC [ M` + three bytes, each offset by 32 —
/// read as `char`s so xterm's UTF-8 extended coordinates don't split).
///
/// SGR wheel reports end in `M` only; xterm emits no release for a wheel, so a
/// `…m` form is not recognized and is refused like any other click release.
fn take_wheel_report(s: &str) -> Option<&str> {
    if let Some(body) = s.strip_prefix("\x1b[<") {
        let end = body.find('M')?;
        let (params, after) = body.split_at(end);
        let rest = &after['M'.len_utf8()..];
        let mut parts = params.split(';');
        let cb: u32 = parts.next()?.parse().ok()?;
        let _x: u32 = parts.next()?.parse().ok()?;
        let _y: u32 = parts.next()?.parse().ok()?;
        if parts.next().is_some() {
            return None;
        }
        return is_wheel_button(cb).then_some(rest);
    }
    if let Some(body) = s.strip_prefix("\x1b[M") {
        let mut chars = body.chars();
        let cb = (chars.next()? as u32).checked_sub(32)?;
        let _x = chars.next()?;
        let _y = chars.next()?;
        return is_wheel_button(cb).then_some(chars.as_str());
    }
    None
}

/// The wheel bit is 64 (buttons 64/65 vertical, 66/67 horizontal). Bit 32 is
/// motion, which a wheel never sets and a drag always does, so it must be
/// clear. Modifier bits (shift 4, meta 8, ctrl 16) may be set — ctrl+wheel is
/// still a wheel. Nothing at or above 128 is a mouse button.
fn is_wheel_button(cb: u32) -> bool {
    cb < 128 && (cb & 0b110_0000) == 0b100_0000
}

fn contains_enter(input: &str) -> bool {
    input.chars().any(|c| c == '\r' || c == '\n')
}

fn apply_input_delta(input: &str, length: &std::sync::atomic::AtomicI32) {
    if input.starts_with('\x1b') {
        return;
    }
    let mut current = length.load(Ordering::Relaxed);
    for c in input.chars() {
        match c {
            '\x08' | '\x7f' => current = (current - 1).max(0),
            '\x15' | '\x0b' => current = 0,
            '\x17' => current = (current - 4).max(0),
            c if c.is_control() => {}
            _ => current += 1,
        }
    }
    length.store(current, Ordering::Relaxed);
}

fn is_automatic_terminal_response(input: &str) -> bool {
    if input == "\x1b[I" || input == "\x1b[O" {
        return true;
    }
    let bytes = input.as_bytes();
    if bytes.len() < 3 || bytes[0] != 0x1b || bytes[1] != b'[' {
        return false;
    }
    let last = bytes[bytes.len() - 1];
    if !matches!(last, b'R' | b'c' | b'n') {
        return false;
    }
    bytes[2..bytes.len() - 1]
        .iter()
        .all(|&b| b.is_ascii_digit() || b == b';' || b == b'?')
}


#[cfg(test)]
mod tests {
    use super::is_automatic_terminal_response as auto_reply;
    use super::read_only_refusal;
    use super::*;
    use crate::state::{ReadOnlySource, TabId};

    /// Everything [`PtyService`] borrows, owned on the stack.
    ///
    /// The write path needs six of the nine handles and no PTY at all: what is
    /// under test is what happens BEFORE the registry write, which is where
    /// every rule in this module lives.
    struct WriteFixture {
        registry: crate::tabs::TabRegistryHandle,
        settings: crate::settings::SettingsHandle,
        tab_activity: crate::state::TabActivity,
        tts: mpsc::Sender<crate::tts::TtsRequest>,
        _tts_rx: mpsc::Receiver<crate::tts::TtsRequest>,
        read_only: crate::state::ReadOnlyTabs,
        input_lengths: crate::state::InputLengths,
        signals: mpsc::Sender<crate::state::StateSignal>,
        rx: mpsc::Receiver<crate::state::StateSignal>,
        cwd: std::path::PathBuf,
        args: Vec<String>,
    }

    impl WriteFixture {
        fn new() -> Self {
            use crate::state::{TabKind, TabMeta};
            let tab = TabId::from_str("claude");
            let (signals, rx) = mpsc::channel(64);
            let registry = std::sync::Arc::new(tokio::sync::Mutex::new(
                crate::tabs::TabRegistry::new(
                    vec![TabMeta {
                        id: tab.clone(),
                        kind: TabKind::AiTool,
                        name: "Claude".to_string(),
                    }],
                    tab.clone(),
                    std::sync::Arc::new(std::sync::RwLock::new(tab)),
                    signals.clone(),
                    std::sync::Arc::new(Vec::new()),
                ),
            ));
            let (tts, _tts_rx) = mpsc::channel(8);
            let defaults = crate::settings::Settings::default();
            let cwd = std::env::temp_dir();
            Self {
                registry,
                settings: crate::settings::SettingsHandle::new(
                    defaults.clone(),
                    defaults,
                    cwd.clone(),
                ),
                tab_activity: crate::state::TabActivity::default(),
                tts,
                _tts_rx,
                read_only: crate::state::ReadOnlyTabs::default(),
                input_lengths: crate::state::InputLengths::default(),
                signals,
                rx,
                cwd,
                args: Vec::new(),
            }
        }

        fn service(&self) -> PtyService<'_> {
            PtyService::new(
                &self.registry,
                &self.settings,
                &self.tab_activity,
                &self.tts,
                &self.cwd,
                &self.args,
                &self.read_only,
                &self.input_lengths,
                &self.signals,
            )
        }

        fn signals(&mut self) -> Vec<crate::state::StateSignal> {
            let mut out = Vec::new();
            while let Ok(s) = self.rx.try_recv() {
                out.push(s);
            }
            out
        }
    }

    /// **A refused keystroke leaves no trace** (V39 locked decision 4).
    ///
    /// This is the property the refusal's POSITION encodes, and it was
    /// previously checkable only by locking a tab in the app and watching the
    /// avatar: an input that never reached the PTY must not have moved the tab
    /// to Listening or armed a TTS-echo suppression for text the model never
    /// saw. So the assertion is not just "it returned an error" — it is that
    /// the state-signal channel is silent.
    ///
    /// The control is the same write with the lock off: it fails at the
    /// registry (there is no live PTY in this fixture) but the keystroke signal
    /// HAS gone out by then, which is what makes the silence above meaningful
    /// rather than vacuous.
    #[tokio::test]
    async fn a_refused_keystroke_leaves_no_trace_and_an_accepted_one_does() {
        let mut f = WriteFixture::new();
        let tab = TabId::from_str("claude");

        f.read_only.set_user(&tab, true);
        let err = f
            .service()
            .write(tab.clone(), "hello".to_string())
            .await
            .expect_err("a locked tab refuses the keyboard");
        assert!(
            err.to_string().contains("read-only"),
            "the refusal names why: {err}"
        );
        assert!(
            f.signals().is_empty(),
            "a refused keystroke must not have moved the avatar"
        );

        // …and the terminal answering the program is not the keyboard: it
        // passes the lock, reaches the registry, and is not counted as typing.
        let _ = f.service().write(tab.clone(), "\x1b[24;80R".to_string()).await;
        assert!(
            f.signals().is_empty(),
            "a cursor-position report is not a keystroke"
        );

        f.read_only.set_user(&tab, false);
        let _ = f.service().write(tab.clone(), "hello".to_string()).await;
        assert!(
            matches!(
                f.signals().as_slice(),
                [crate::state::StateSignal::UserKeystroke { .. }]
            ),
            "an accepted keystroke reaches the state manager"
        );
    }

    /// **The reserved dashboards swallow input rather than erroring.** They are
    /// app-rendered and have no PTY, so a stray write must be a no-op — one
    /// shared predicate, so a dashboard added later cannot miss it.
    #[tokio::test]
    async fn a_write_to_a_reserved_dashboard_is_swallowed() {
        let mut f = WriteFixture::new();
        for tab in [
            TabId::GraphMonitor,
            TabId::Workbench,
            TabId::ToolActivity,
            TabId::Events,
        ] {
            f.service()
                .write(tab.clone(), "hello".to_string())
                .await
                .unwrap_or_else(|e| panic!("{tab:?} must swallow, not error: {e}"));
        }
        assert!(
            f.signals().is_empty(),
            "a swallowed write is not a keystroke either"
        );
    }

    /// **The keyboard's submit rule is unchanged, and it is the wrong rule for
    /// a paste** (V39 review L-1).
    ///
    /// A person's Enter IS the submit, so `pty_write` still reads it off the
    /// bytes. A delegation's paste carries the request's own newlines, and
    /// under the same rule it read as a submit one write early — which is why
    /// the engine STATES it instead of letting it be inferred.
    #[test]
    fn a_submit_is_inferred_for_the_keyboard_and_stated_by_the_engine() {
        use super::Submit;
        assert_eq!(Submit::from_input("hello"), Submit::No);
        assert_eq!(Submit::from_input(""), Submit::No);
        assert_eq!(Submit::from_input("\r"), Submit::Yes);
        assert_eq!(Submit::from_input("hello\n"), Submit::Yes);
        // The trap, spelled out: a bracketed paste of a two-line request looks
        // exactly like a submit to the byte rule.
        let paste = "\u{1b}[200~line one\nline two\u{1b}[201~";
        assert_eq!(
            Submit::from_input(paste),
            Submit::Yes,
            "the byte rule cannot tell a pasted newline from a pressed Enter"
        );
    }

    /// **V39 Phase A: a refusal always names why.** The frontend shows this
    /// string verbatim in a toast, so an empty or generic one leaves the user
    /// staring at a tab that has silently stopped accepting keys.
    #[test]
    fn a_read_only_write_is_refused_with_the_reason_named() {
        assert_eq!(
            read_only_refusal(&ReadOnlySource::User, "hello", None).as_deref(),
            Some("read-only (user)")
        );
        assert_eq!(
            read_only_refusal(
                &ReadOnlySource::Driven {
                    by: TabId::from_str("opencode")
                },
                "hello",
                Some("api-work"),
            )
            .as_deref(),
            Some("driven by api-work")
        );
    }

    /// **The lock is on the user's keyboard, not on the terminal protocol.**
    /// A TUI that queried the cursor position must still get its answer while
    /// the tab is locked — otherwise the very tab a delegation is driving is
    /// the one that wedges.
    #[test]
    fn terminal_protocol_replies_are_not_refused_by_the_lock() {
        for reply in ["\x1b[24;80R", "\x1b[?1;2c", "\x1b[0n", "\x1b[I", "\x1b[O"] {
            assert!(
                auto_reply(reply),
                "fixture must be an automatic reply: {reply:?}"
            );
            assert_eq!(
                read_only_refusal(&ReadOnlySource::User, reply, None),
                None,
                "the terminal's own answer was refused: {reply:?}"
            );
        }
    }

    /// **Scrolling is reading.** A read-only tab exists so the user can WATCH
    /// it, and in an alt-screen TUI the wheel is forwarded to the program as a
    /// mouse report rather than scrolling xterm's own buffer — so a swallowed
    /// wheel means a tab one may watch but not scroll.
    ///
    /// The fixture table is duplicated verbatim in
    /// `src/lib/delegation.test.ts`: the courtesy gate and this enforcement
    /// point must agree about every one of these, or one of them refuses a
    /// scroll the other allowed.
    #[test]
    fn mouse_wheel_passes_the_lock_under_either_source() {
        let driven = ReadOnlySource::Driven {
            by: TabId::from_str("opencode"),
        };
        for wheel in [
            "\x1b[<64;10;5M",              // wheel up (SGR)
            "\x1b[<65;10;5M",              // wheel down
            "\x1b[<66;1;1M",               // wheel left
            "\x1b[<67;1;1M",               // wheel right
            "\x1b[<80;3;4M",               // ctrl+wheel up (64 + modifier 16)
            "\x1b[<68;3;4M",               // shift+wheel up (64 + modifier 4)
            "\x1b[M`!!",                   // wheel up, legacy X10 encoding
            "\x1b[Ma!!",                   // wheel down, legacy X10 encoding
            "\x1b[<64;1;1M\x1b[<64;1;1M",  // a fast scroll, coalesced
        ] {
            assert!(
                super::is_mouse_wheel(wheel),
                "fixture must be a wheel report: {wheel:?}"
            );
            for source in [&ReadOnlySource::User, &driven] {
                assert_eq!(
                    read_only_refusal(source, wheel, Some("api-work")),
                    None,
                    "a locked tab refused a scroll ({source:?}): {wheel:?}"
                );
            }
        }
    }

    /// **A click is input.** It activates whatever control is under it — a
    /// permission option, for one — which is the whole reason the lock exists.
    /// Drag and motion go with it: a drag is a held button.
    #[test]
    fn mouse_clicks_drags_and_pastes_are_still_refused() {
        for click in [
            "\x1b[<0;10;5M",   // left press
            "\x1b[<0;10;5m",   // left release
            "\x1b[<1;1;1M",    // middle press
            "\x1b[<2;1;1M",    // right press
            "\x1b[<32;5;5M",   // drag with button 0 held (motion bit)
            "\x1b[<35;5;5M",   // bare motion
            "\x1b[M !!",       // left press, legacy X10 encoding
            "\x1b[M#!!",       // release, legacy X10 encoding
            "\x1b[200~x\x1b[201~", // a bracketed paste
        ] {
            assert!(
                !super::is_mouse_wheel(click),
                "fixture must not be a wheel report: {click:?}"
            );
            assert!(
                read_only_refusal(&ReadOnlySource::User, click, None).is_some(),
                "mouse/paste input slipped through the lock: {click:?}"
            );
        }
    }

    /// **The exemption cannot carry a passenger.** A chunk that is a wheel
    /// report *plus* typed text is not a wheel report — otherwise the lock
    /// would be one concatenation away from open.
    #[test]
    fn a_wheel_report_with_anything_else_attached_is_refused() {
        for smuggled in [
            "\x1b[<64;1;1My",              // wheel then a keystroke
            "y\x1b[<64;1;1M",              // keystroke then wheel
            "\x1b[<64;1;1M\r",             // wheel then Enter
            "\x1b[<64;1;1M\x1b[<0;1;1M",   // wheel then a click
            "\x1b[<64;1;1",                // truncated: no terminator
            "\x1b[M`!",                    // truncated X10: two coord bytes
            "",
        ] {
            assert!(
                !super::is_mouse_wheel(smuggled),
                "fixture must not be a wheel report: {smuggled:?}"
            );
            assert!(
                read_only_refusal(&ReadOnlySource::User, smuggled, None).is_some(),
                "input smuggled past the lock behind a wheel report: {smuggled:?}"
            );
        }
    }

    /// The wheel exemption is `read_only_refusal`'s alone: the keystroke and
    /// submit bookkeeping still sees a wheel report exactly as it always did
    /// (not an automatic terminal response), so nothing about avatar state or
    /// echo suppression moved with this.
    #[test]
    fn the_wheel_exemption_did_not_change_the_automatic_reply_predicate() {
        for wheel in ["\x1b[<64;10;5M", "\x1b[M`!!"] {
            assert!(!auto_reply(wheel), "{wheel:?}");
        }
    }

    /// …and the exemption is narrow: an escape sequence a *person* produced
    /// (arrow keys, Esc, a bracketed paste) is still input, and still refused.
    #[test]
    fn keyboard_escape_sequences_are_still_refused() {
        for keys in ["\x1b[A", "\x1b", "\r", "y", "\x1b[200~pasted\x1b[201~"] {
            assert!(
                !auto_reply(keys),
                "fixture must not be an automatic reply: {keys:?}"
            );
            assert!(
                read_only_refusal(&ReadOnlySource::User, keys, None).is_some(),
                "user input slipped through the lock: {keys:?}"
            );
        }
    }

    #[test]
    fn focus_events_are_auto() {
        assert!(auto_reply("\x1b[I"));
        assert!(auto_reply("\x1b[O"));
    }

    #[test]
    fn da_and_cpr_replies_are_auto() {
        assert!(auto_reply("\x1b[?1;2c"));
        assert!(auto_reply("\x1b[?62;1;2;6;9;15;22c"));
        assert!(auto_reply("\x1b[10;20R"));
        assert!(auto_reply("\x1b[?1n"));
        assert!(auto_reply("\x1b[5n"));
    }

    #[test]
    fn arrow_keys_are_not_auto() {
        assert!(!auto_reply("\x1b[A"));
        assert!(!auto_reply("\x1b[B"));
        assert!(!auto_reply("\x1b[C"));
        assert!(!auto_reply("\x1b[D"));
        assert!(!auto_reply("\x1b[H"));
        assert!(!auto_reply("\x1b[1~"));
    }

    #[test]
    fn printable_and_control_are_not_auto() {
        assert!(!auto_reply("a"));
        assert!(!auto_reply("hello"));
        assert!(!auto_reply("\r"));
        assert!(!auto_reply("\x7f"));
        assert!(!auto_reply("\t"));
        assert!(!auto_reply("\x1b"));
        assert!(!auto_reply("\x1bf"));
    }
}
