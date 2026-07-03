use std::sync::atomic::Ordering;

use tauri::ipc::Channel;
use tauri::{AppHandle, Emitter, EventTarget, State};

use crate::error::{AppError, AppResult};
use crate::ipc::windows::{open_or_focus_settings, SETTINGS_LABEL};
use crate::ipc::AppState;
use crate::settings::{
    default_claude_local_tab, default_claude_tab, default_opencode_tab,
    AiToolTabConfig, Settings, TabConfig, CLAUDE_LOCAL_TAB_ID, CLAUDE_TAB_ID, OPENCODE_TAB_ID,
};
use crate::state::{StateSignal, TabId};

/// V1.4-04 D: pty_start now returns the persisted-scrollback bytes
/// from the previous session (if any). The frontend writes them to the
/// new xterm before the live channel binds so the user sees their
/// previous shell output above the fresh prompt. The bytes are also
/// seeded into the new ring buffer so a subsequent crash-restart
/// preserves continuity (capped at the ring size, naturally).
///
/// Returns `None` when:
///   - `terminal.scrollback.restore_on_launch` is `false`
///   - no persisted file exists for this tab (cold install, or
///     already consumed earlier in this session)
///   - reading the file failed (logged at warn; treated as cold start)
#[tauri::command]
pub async fn pty_start(
    app: AppHandle,
    state: State<'_, AppState>,
    tab: TabId,
    channel: Channel<String>,
    rows: u16,
    cols: u16,
) -> AppResult<Option<Vec<u8>>> {
    let cwd = state.launch.cwd.clone();
    let invocation_args = state.launch.extra_args.clone();
    let tts_tx = state.tts_segments.clone();
    let user_typed = state.user_typed_tts.clone();
    let settings = state.settings.clone();
    let restore_on_launch = settings.current().terminal.scrollback.restore_on_launch;

    {
        let registry = state.tabs.lock().await;
        registry
            .start_tab(
                app,
                tab.clone(),
                channel,
                rows,
                cols,
                &cwd,
                &invocation_args,
                tts_tx,
                user_typed,
                settings,
            )
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
    // tab_activate, …) behind disk latency / AV scans. Re-acquire only for the
    // seed, which does touch the registry.
    let restored = crate::pty::scrollback::read(&tab);
    if let Some(bytes) = &restored {
        let registry = state.tabs.lock().await;
        match registry.seed_scrollback(&tab, bytes).await {
            Ok(()) => crate::pty::scrollback::consume_after_read(&tab),
            Err(e) => {
                tracing::warn!(?tab, error = %e, "scrollback seed failed; on-disk copy retained for retry");
            }
        }
    }
    Ok(restored)
}

#[tauri::command]
pub async fn pty_restart(
    app: AppHandle,
    state: State<'_, AppState>,
    tab: TabId,
    channel: Channel<String>,
    rows: u16,
    cols: u16,
) -> AppResult<()> {
    let cwd = state.launch.cwd.clone();
    let invocation_args = state.launch.extra_args.clone();
    let tts_tx = state.tts_segments.clone();
    let user_typed = state.user_typed_tts.clone();
    let settings = state.settings.clone();
    let registry = state.tabs.lock().await;
    let result = registry
        .restart_tab(
            app,
            tab.clone(),
            channel,
            rows,
            cols,
            &cwd,
            &invocation_args,
            tts_tx,
            user_typed,
            settings,
        )
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

/// V1.4-03: re-point a still-running PTY's bytes at a fresh JS-side
/// `Channel<String>` without restarting the shell. The frontend invokes
/// this when the xterm.js Terminal is destroyed and recreated for a
/// renderer-category flip (background image toggled on or off). The
/// shell session, env, cwd, and any in-flight processes survive; only
/// the IPC channel is replaced.
#[tauri::command]
pub async fn pty_rebind_channel(
    state: State<'_, AppState>,
    tab: TabId,
    channel: Channel<String>,
) -> AppResult<()> {
    let registry = state.tabs.lock().await;
    registry.rebind_channel(tab, channel).await
}

/// V1.4-04 D.3: snapshot a tab's PTY scrollback as raw bytes. Exposed
/// for diagnostics and external use; the launch-replay path uses an
/// internal API (`pty_start` returning `Option<Vec<u8>>`) for
/// efficiency. Returns `NotStarted` if the tab has no live PTY.
#[tauri::command]
pub async fn pty_get_scrollback(
    state: State<'_, AppState>,
    tab: TabId,
) -> AppResult<Vec<u8>> {
    let registry = state.tabs.lock().await;
    registry.scrollback_snapshot(tab).await
}

#[tauri::command]
pub async fn pty_write(
    state: State<'_, AppState>,
    tab: TabId,
    input: String,
) -> AppResult<()> {
    // V8-03: the Offload Server tab is read-only — it has no PTY of its own
    // (its content is the live llama-server output stream), so swallow any
    // write. Defense-in-depth behind the frontend's read-only guard.
    if matches!(tab, TabId::OffloadServer | TabId::GraphMonitor) {
        return Ok(());
    }
    // Pre-register any TTS markers in the user's input so they don't fire
    // when echoed back by the TUI. Content-based; no per-tab scoping needed.
    // The set stores whitespace-normalized content so a width-driven echo
    // rewrap still matches.
    let typed_tags = extract_tts_contents(&input);
    if !typed_tags.is_empty() {
        if let Ok(mut set) = state.user_typed_tts.lock() {
            for content in typed_tags {
                let key = crate::processing::normalize_for_dedup(&content);
                if !key.is_empty() {
                    set.insert(key);
                }
            }
            // V0.6+ bound: drop the set when it grows past a generous cap.
            // Each entry is a normalized `[[TTS]]…[[/TTS]]` body that the
            // user typed or pasted; a long-lived session that pastes many
            // such blocks would otherwise leak a few hundred MB. Clearing
            // is a wider net than LRU eviction (a tiny window where an
            // echo could slip through) but doesn't pull a new dep, and
            // the worst-case symptom is one extra spoken segment.
            const USER_TYPED_TTS_CAP: usize = 4096;
            if set.len() > USER_TYPED_TTS_CAP {
                set.clear();
            }
        }
    }

    // Accumulate the user's plain typed line and, on Enter, register its
    // sentences too. Unlike the `[[TTS]]`-marker path above, this is what
    // suppresses the *unmarked* question echo in "speak all output" mode.
    note_typed_input(&state.user_input_buf, &state.user_typed_tts, &tab, &input);

    // Take the registry lock once at the top so the keystroke / submit
    // counter updates and the final write run inside the same critical
    // section. Pre-V0.6 the counter Arc was cloned out under the read
    // lock and used after dropping it, racing with `close_tab` which
    // removes the counter. Holding the lock end-to-end eliminates that
    // window: if the tab was just closed, `registry.write` errors out
    // cleanly with `unknown tab` and no half-applied state remains.
    let registry = state.tabs.lock().await;

    let existing = {
        let map = state
            .input_lengths
            .read()
            .map_err(|e| AppError::Pty(format!("input_lengths poisoned: {e}")))?;
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
            let mut map = state
                .input_lengths
                .write()
                .map_err(|e| AppError::Pty(format!("input_lengths poisoned: {e}")))?;
            map.entry(tab.clone())
                .or_insert_with(|| std::sync::Arc::new(std::sync::atomic::AtomicI32::new(0)))
                .clone()
        }
    };

    if !is_automatic_terminal_response(&input) {
        if contains_enter(&input) {
            len_counter.store(0, Ordering::Relaxed);
            let _ = state.state_signals.try_send(StateSignal::UserSubmit { tab: tab.clone() });
        } else {
            apply_input_delta(&input, &len_counter);
            let _ = state
                .state_signals
                .try_send(StateSignal::UserKeystroke { tab: tab.clone() });
            // Note: typing does NOT interrupt TTS. By design, in-flight
            // speech is only stopped by Esc (`tts_stop`) or by switching
            // tabs (so the previous tab's audio doesn't bleed into the new
            // view). Keystrokes still drive avatar state via UserKeystroke.
        }
    }

    registry.write(tab, input.into_bytes()).await
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

/// Accumulate the user's typed input per tab and, on Enter, fold its
/// sentences into the shared echo-suppression set so "speak all output"
/// mode doesn't read the question back when the TUI echoes it. Mirrors the
/// line editing `apply_input_delta` already understands (backspace, kill-line,
/// kill-word). ESC-led writes (arrow keys, function keys, bracketed paste) are
/// skipped wholesale, exactly like the length counter — so a pasted question
/// isn't captured, which is an accepted gap.
fn note_typed_input(
    buf_map: &std::sync::Mutex<std::collections::HashMap<TabId, String>>,
    user_typed: &std::sync::Mutex<std::collections::HashSet<String>>,
    tab: &TabId,
    input: &str,
) {
    if input.starts_with('\x1b') {
        return;
    }
    let Ok(mut map) = buf_map.lock() else {
        return;
    };
    let buf = map.entry(tab.clone()).or_default();
    for c in input.chars() {
        match c {
            '\r' | '\n' => {
                register_echo_sentences(user_typed, buf);
                buf.clear();
            }
            // Backspace / DEL.
            '\x08' | '\x7f' => {
                buf.pop();
            }
            // Ctrl-U (kill line) / Ctrl-C (abandon).
            '\x15' | '\x03' => buf.clear(),
            // Ctrl-W (kill previous word).
            '\x17' => {
                while buf.ends_with(' ') {
                    buf.pop();
                }
                while !buf.is_empty() && !buf.ends_with(' ') {
                    buf.pop();
                }
            }
            c if c.is_control() => {}
            c => buf.push(c),
        }
    }
    // Bound a line that's never submitted (e.g. user keeps typing, hits Esc).
    if buf.len() > 8192 {
        buf.clear();
    }
}

/// Register each sentence of `text` (whitespace-normalized) in the echo
/// set. Empty input is a no-op. Caps the set like the marker path does.
fn register_echo_sentences(
    user_typed: &std::sync::Mutex<std::collections::HashSet<String>>,
    text: &str,
) {
    if text.trim().is_empty() {
        return;
    }
    let Ok(mut set) = user_typed.lock() else {
        return;
    };
    for sentence in crate::processing::segment_sentences(text) {
        let key = crate::processing::normalize_for_dedup(&sentence);
        if !key.is_empty() {
            set.insert(key);
        }
    }
    const USER_TYPED_TTS_CAP: usize = 4096;
    if set.len() > USER_TYPED_TTS_CAP {
        set.clear();
    }
}

fn extract_tts_contents(input: &str) -> Vec<String> {
    const OPEN: &str = "[[TTS]]";
    const CLOSE: &str = "[[/TTS]]";
    let mut out = Vec::new();
    let mut i = 0;
    while let Some(open) = input[i..].find(OPEN) {
        let content_start = i + open + OPEN.len();
        let after = &input[content_start..];
        match after.find(CLOSE) {
            Some(close_rel) => {
                let content = after[..close_rel].to_string();
                if !content.is_empty() {
                    out.push(content);
                }
                i = content_start + close_rel + CLOSE.len();
            }
            None => break,
        }
    }
    out
}

/// Debug: synthesize and play `text` directly through the TTS worker, skipping
/// the processor. Routed as if it came from the active tab so the worker's
/// filter doesn't drop it.
#[tauri::command]
pub async fn tts_test(state: State<'_, AppState>, text: String) -> AppResult<()> {
    let active = state.tabs.lock().await.active();
    state
        .tts_segments
        .send(crate::tts::TtsRequest::Synthesize { tab: active, text, suppressible: false })
        .await
        .map_err(|e| AppError::Tts(format!("tts_test send: {e}")))?;
    Ok(())
}

/// Read arbitrary text aloud through the TTS worker, skipping the
/// processor. Backs the Ctrl+right-click "speak selection" gesture
/// (`behavior.speak_selection_on_right_click`). Routed as if it came
/// from the active tab so the worker's background-tab filter doesn't
/// drop it. Whitespace-only text is ignored — the frontend guards too,
/// but a backend skip keeps an empty synthesis off the worker.
#[tauri::command]
pub async fn tts_speak(state: State<'_, AppState>, text: String) -> AppResult<()> {
    if text.trim().is_empty() {
        return Ok(());
    }
    let active = state.tabs.lock().await.active();
    state
        .tts_segments
        .send(crate::tts::TtsRequest::Synthesize { tab: active, text, suppressible: false })
        .await
        .map_err(|e| AppError::Tts(format!("tts_speak send: {e}")))?;
    Ok(())
}

/// Read a terminal selection aloud as a read-along: `chunks` are the
/// sentence segments (pre-split on the frontend so the spoken text exactly
/// matches the highlighted text), synthesized and played in order. `session`
/// is a frontend-assigned monotonic id stored in the shared cell so the
/// worker can be told to abandon the read — `tts_stop` (Esc) zeroes the
/// cell, and a newer call overwrites it. The audio thread emits
/// `tts-selection-progress` events as it advances through the chunks so the
/// frontend can recede the highlight. Backs `behavior.speak_selection_on_right_click`.
#[tauri::command]
pub async fn tts_speak_selection(
    state: State<'_, AppState>,
    session: u64,
    chunks: Vec<String>,
) -> AppResult<()> {
    if chunks.is_empty() {
        return Ok(());
    }
    // Resolve the active tab FIRST (this awaits the registry lock), then arm
    // the session cell immediately before the send with no await in between.
    // Storing the session before the `.lock().await` left a window in which a
    // concurrent `tts_stop` (Esc) could zero the cell and then this command
    // would still proceed to send — racing the stop. With the store moved
    // after the await, an Esc that lands before this point simply means the
    // worker sees a superseding/zeroed cell and abandons, and one that lands
    // after is a clean supersede. The worker re-checks `speak_session` both
    // before and after each chunk's synthesis, so a stop during the read still
    // cancels the remaining chunks.
    let active = state.tabs.lock().await.active();
    state.speak_session.store(session, Ordering::SeqCst);
    state
        .tts_segments
        .send(crate::tts::TtsRequest::SpeakSelection { tab: active, session, chunks })
        .await
        .map_err(|e| AppError::Tts(format!("tts_speak_selection send: {e}")))?;
    Ok(())
}

/// Stop all TTS playback immediately and cancel any in-flight selection read.
/// Backs the Esc gesture: clears the audio sink (so queued chunks never play)
/// and zeroes the shared session cell (so the worker abandons the remaining
/// chunks it hasn't enqueued yet). The frontend clears its highlight on the
/// same Esc.
#[tauri::command]
pub async fn tts_stop(state: State<'_, AppState>) -> AppResult<()> {
    state.speak_session.store(0, Ordering::SeqCst);
    // Suppress the rest of the current AI-output burst's tagged segments
    // (those still queued or yet to arrive) until the next `ClaudeOutputStarted`
    // clears the flag. Notifications and selection reads are unaffected — they
    // ride other request variants the worker doesn't gate on this flag.
    state.ai_tts_suppressed.store(true, Ordering::SeqCst);
    // Recover the guard even if the lock is poisoned: this is the Esc
    // emergency-stop, so it must never silently no-op and leave audio playing
    // with no way to stop it from the UI. `into_inner` hands back the guard;
    // the data behind it (an `Option<AudioOutput>` handle) is not left in a
    // broken state by a panicking writer.
    let audio = state.audio.read().unwrap_or_else(|e| e.into_inner());
    if let Some(audio) = audio.as_ref().cloned() {
        audio.stop_all();
    }
    Ok(())
}

/// Pause or resume TTS playback without discarding queued audio. Backs the
/// bottom-bar selection-TTS pause/resume transport. The in-flight read's
/// session is left untouched (so resume continues exactly where it paused);
/// only the audio sink is paused.
#[tauri::command]
pub async fn tts_set_paused(state: State<'_, AppState>, paused: bool) -> AppResult<()> {
    // Recover a poisoned guard rather than swallowing it — a no-op pause/resume
    // would leave the transport controls dead with no signal why.
    let audio = state.audio.read().unwrap_or_else(|e| e.into_inner());
    if let Some(audio) = audio.as_ref().cloned() {
        audio.set_paused(paused);
    }
    Ok(())
}

// --- Speech-to-text (V6-01) -------------------------------------------------
//
// The handle just posts commands to the capture thread; recording/transcribe
// state transitions and the resulting transcript arrive on the frontend via
// the `stt-state` / `stt-transcription` events, not these return values.

/// Open the input device and begin capturing. No-op if already recording.
#[tauri::command]
pub async fn stt_start_recording(state: State<'_, AppState>) -> AppResult<()> {
    state.stt.start();
    Ok(())
}

/// Stop capturing and hand the recording to the transcription worker. The
/// transcript arrives later via the `stt-transcription` event.
#[tauri::command]
pub async fn stt_stop_recording(state: State<'_, AppState>) -> AppResult<()> {
    state.stt.stop();
    Ok(())
}

/// Stop capturing and discard the buffer (no transcription).
#[tauri::command]
pub async fn stt_cancel(state: State<'_, AppState>) -> AppResult<()> {
    state.stt.cancel();
    Ok(())
}

/// List the `ggml-*.bin` Whisper models present under `models/` for the
/// settings dropdown.
#[tauri::command]
pub async fn stt_list_models() -> AppResult<Vec<String>> {
    crate::stt::list_models()
}

/// List cpal input device names for the settings device picker. The frontend
/// prepends a "System default" entry (which maps to an empty `input_device`).
#[tauri::command]
pub async fn stt_list_input_devices() -> AppResult<Vec<String>> {
    crate::stt::list_input_devices()
}

/// Fetch the current Claude Code usage snapshot (session 5h + weekly 7d) for
/// the bottom-bar usage tracker. Returns `None` when usage can't be obtained
/// (not logged in, endpoint unreachable) — the frontend hides the widget in
/// that case. Polled by the frontend on `usage.poll_interval_secs`.
#[tauri::command]
pub async fn get_claude_usage() -> AppResult<crate::usage::UsageResult> {
    Ok(crate::usage::fetch_usage().await)
}

/// Sample the system-monitor stats (CPU / memory / GPU / network) for the
/// bottom-bar panel. Polled by the frontend on `system_stats.poll_interval_secs`
/// (default 1s); the frontend keeps its own history for the sparklines.
#[tauri::command]
pub async fn get_system_stats(
    state: State<'_, AppState>,
) -> AppResult<crate::sysmon::SystemStatsSnapshot> {
    // `sample()` blocks: it does a synchronous sysinfo refresh (incl.
    // `networks.refresh(true)`, which re-scans every interface) plus NVML
    // device queries. Run it on the blocking pool so the 1 Hz poll doesn't
    // stall other IPC futures on the async reactor thread.
    let sysmon = state.sysmon.clone();
    tauri::async_runtime::spawn_blocking(move || sysmon.sample())
        .await
        .map_err(|e| AppError::Ipc(format!("system stats join: {e}")))
}

#[tauri::command]
pub async fn pty_resize(
    state: State<'_, AppState>,
    tab: TabId,
    rows: u16,
    cols: u16,
) -> AppResult<()> {
    let registry = state.tabs.lock().await;
    registry.resize(tab, rows, cols).await
}

#[tauri::command]
pub async fn compose_content_changed(
    state: State<'_, AppState>,
    non_empty: bool,
) -> AppResult<()> {
    // Compose targets the currently active tab — its non-empty edge promotes
    // the active tab Idle→Listening and pins Listening while content remains.
    let active = state.tabs.lock().await.active();
    let _ = state
        .state_signals
        .try_send(StateSignal::ComposeContentChanged {
            tab: active,
            non_empty,
        });
    Ok(())
}

#[tauri::command]
pub async fn acknowledge_error(state: State<'_, AppState>, tab: TabId) -> AppResult<()> {
    let _ = state
        .state_signals
        .try_send(StateSignal::ErrorAcknowledged { tab });
    Ok(())
}



/// Activate a tab. Frontend calls this on click and on Ctrl+1/Ctrl+2; the
/// state manager broadcasts an `ActiveTabChanged` event so all subscribers
/// reconcile from a single source of truth. Does NOT persist the active
/// tab to settings — use `set_active_tab` for that.
#[tauri::command]
pub async fn tab_activate(state: State<'_, AppState>, tab: TabId) -> AppResult<()> {
    let mut registry = state.tabs.lock().await;
    registry.activate(tab).await
}

/// Activate a tab AND persist its id as `session.active_tab_id`. Used by
/// the frontend's tab-switch handler (click, Ctrl+1..9) so the user's
/// last-active tab is restored on next launch. The settings write is
/// debounced so a fast Ctrl+1/Ctrl+2 burst doesn't hammer the disk.
#[tauri::command]
pub async fn set_active_tab(state: State<'_, AppState>, tab: TabId) -> AppResult<()> {
    let id_string = tab.as_str().to_string();
    {
        let mut registry = state.tabs.lock().await;
        registry.activate(tab).await?;
    }
    // Atomic read-modify-write so a concurrent close_tab / settings_update
    // can't clobber this with a stale whole-struct snapshot (lost-update). The
    // outer `current()` check just skips the broadcast/save on a no-op
    // re-activation; the real write re-checks under the held lock.
    if state.settings.current().session.active_tab_id.as_deref() != Some(id_string.as_str()) {
        state.settings.mutate(move |snap| {
            snap.session.active_tab_id = Some(id_string);
        });
    }
    Ok(())
}

/// Snapshot the live tab list. Frontend calls this once on App mount to
/// seed its tabs store; subsequent runtime mutations arrive via the
/// `tab-created`/`tab-closed`/`tab-renamed` events broadcast through the
/// `avatar-state` channel. Avoids the race where setup-time TabCreated
/// emissions could fire before the webview's listener attaches.
#[tauri::command]
pub async fn list_tabs(
    state: State<'_, AppState>,
) -> AppResult<Vec<crate::tabs::TabMetaWire>> {
    let registry = state.tabs.lock().await;
    Ok(registry.list())
}

#[tauri::command]
pub async fn settings_get(state: State<'_, AppState>) -> AppResult<Settings> {
    Ok(state.settings.current())
}

/// Per-AI-tab default config. Used by the Settings window's "Reset to
/// default" buttons so the frontend doesn't have to mirror Rust-side
/// tab defaults.
///
/// Only AI tabs have a meaningful "default" in v1.2 — Shell tab defaults
/// depend on the host platform's auto-detected shell, and "reset" on a
/// user-created Shell tab is not a meaningful UX (use the New Shell Tab
/// dialog to spawn a fresh one). Shell ids return an error.
#[tauri::command]
pub async fn ai_tool_tab_defaults(tab: TabId) -> AppResult<AiToolTabConfig> {
    let config = match tab.as_str() {
        CLAUDE_TAB_ID => default_claude_tab(),
        CLAUDE_LOCAL_TAB_ID => default_claude_local_tab(),
        OPENCODE_TAB_ID => default_opencode_tab(),
        other => {
            return Err(AppError::Pty(format!(
                "ai_tool_tab_defaults: tab {other} has no AI defaults"
            )))
        }
    };
    match config {
        TabConfig::AiTool(c) => Ok(c),
        TabConfig::Shell(_) => Err(AppError::Pty(
            "ai_tool_tab_defaults: reserved id resolved to a shell config".into(),
        )),
    }
}

#[tauri::command]
pub async fn settings_update(
    state: State<'_, AppState>,
    mut settings: Settings,
) -> AppResult<()> {
    // Re-point bundled avatar videos at the (possibly just-changed) theme's
    // on-disk subfolder before broadcasting, so switching theme switches the
    // avatar. User overrides are preserved; see `apply_portable_avatar_paths`.
    crate::settings::apply_portable_avatar_paths(&mut settings);

    // Snapshot the pre-update feature flags so we can drive the reserved tabs'
    // live materialize/remove after the mutate. The integrity pass that
    // normally owns those tabs only runs at load, so without this a freshly
    // enabled feature's tab wouldn't appear until the next launch.
    let (was_graph, was_offload, was_stt, was_stt_device) = {
        let old = state.settings.current();
        (old.graph.enabled, old.offload.enabled, old.stt.enabled, old.stt.device)
    };

    // The Settings window holds a full snapshot and replaces wholesale, but it
    // never edits `layout` or `session` (those are driven only by the main
    // window's save_layout / set_active_tab commands). Preserve them from the
    // live state so a stale snapshot from the settings webview can't clobber a
    // layout the user just dragged or the active-tab the main window just set.
    // `tabs` IS legitimately edited here (TabBar reorder, ConfigureTabDialog,
    // reset-to-defaults), so it is taken from the incoming struct.
    state.settings.mutate(move |cur| {
        settings.layout = cur.layout.clone();
        settings.session = cur.session.clone();
        *cur = settings;
        // Keep the reserved feature tabs (Offload Server / Code Graph monitor)
        // present-iff-enabled in the persisted list.
        crate::settings::reconcile_reserved_tabs(cur);
    });

    // On an `stt.enabled` edge, load or unload the Whisper model so the toggle
    // actually frees/reclaims memory (not just hides the record button). When
    // the feature stays enabled but the device (GPU↔CPU) changed, preload
    // reloads the model on the new device — `needs_reload` in the worker
    // detects the device mismatch and rebuilds the context, freeing the old
    // device's memory. (Unlike TTS, the STT worker isn't a settings subscriber;
    // it's driven by these control messages, so the reload must be nudged here.)
    let now = state.settings.current();
    if now.stt.enabled != was_stt {
        if now.stt.enabled {
            state.stt.preload();
        } else {
            state.stt.unload();
        }
    } else if now.stt.enabled && now.stt.device != was_stt_device {
        state.stt.preload();
    }

    // On an actual enable/disable edge, mirror the change into the runtime so
    // the reserved tab appears/disappears live (tab bar + pane placement).
    if now.graph.enabled != was_graph || now.offload.enabled != was_offload {
        // Serialize against create/close_tab while we touch the registry.
        let _serializer = state.lifecycle_serializer.lock().await;
        if now.graph.enabled != was_graph {
            super::tab_lifecycle::sync_reserved_feature_tab(
                state.inner(),
                TabId::GraphMonitor,
                now.graph.enabled,
            )
            .await;
        }
        if now.offload.enabled != was_offload {
            super::tab_lifecycle::sync_reserved_feature_tab(
                state.inner(),
                TabId::OffloadServer,
                now.offload.enabled,
            )
            .await;
        }
    }
    Ok(())
}

// ── V8-01 local task offload ────────────────────────────────────────────
// The supervisor is managed as its own state (it needs the AppHandle for
// `offload-state` events, available only in the setup hook). These thin
// commands drive its lifecycle from the Settings UI.

/// Current offload server status (state + discovered `n_ctx`/slots/in-flight)
/// for the **primary** local backend (legacy single-status readout).
#[tauri::command]
pub async fn offload_status(
    supervisor: State<'_, std::sync::Arc<crate::offload::OffloadSupervisor>>,
) -> AppResult<crate::offload::OffloadState> {
    Ok(supervisor.status().await)
}

/// V8-02: per-backend status for every enabled backend in the pool (Local
/// process+health and Remote health-probe). Drives the Settings backends
/// editor's status rows.
#[tauri::command]
pub async fn offload_statuses(
    supervisor: State<'_, std::sync::Arc<crate::offload::OffloadSupervisor>>,
) -> AppResult<Vec<crate::offload::supervisor::BackendStatus>> {
    Ok(supervisor.statuses().await)
}

/// V8-02: start one named Local backend (idempotent).
#[tauri::command]
pub async fn offload_backend_start(
    supervisor: State<'_, std::sync::Arc<crate::offload::OffloadSupervisor>>,
    name: String,
) -> AppResult<()> {
    supervisor.inner().start_backend(&name).await
}

/// V8-02: stop one named Local backend (idempotent).
#[tauri::command]
pub async fn offload_backend_stop(
    supervisor: State<'_, std::sync::Arc<crate::offload::OffloadSupervisor>>,
    name: String,
) -> AppResult<()> {
    supervisor.stop_backend(&name).await;
    Ok(())
}

/// V8-02: restart (Reset) one named Local backend.
#[tauri::command]
pub async fn offload_backend_restart(
    supervisor: State<'_, std::sync::Arc<crate::offload::OffloadSupervisor>>,
    name: String,
) -> AppResult<()> {
    supervisor.inner().restart_backend(&name).await
}

/// Start the offload `llama-server` (idempotent).
#[tauri::command]
pub async fn offload_server_start(
    supervisor: State<'_, std::sync::Arc<crate::offload::OffloadSupervisor>>,
) -> AppResult<()> {
    supervisor.inner().start().await
}

/// Stop the offload `llama-server` (idempotent).
#[tauri::command]
pub async fn offload_server_stop(
    supervisor: State<'_, std::sync::Arc<crate::offload::OffloadSupervisor>>,
) -> AppResult<()> {
    supervisor.stop().await;
    Ok(())
}

/// Reset: kill + respawn with the current `server_command` (re-health,
/// re-read `n_ctx`/`np`).
#[tauri::command]
pub async fn offload_server_restart(
    supervisor: State<'_, std::sync::Arc<crate::offload::OffloadSupervisor>>,
) -> AppResult<()> {
    supervisor.inner().restart().await
}

/// Run a canned offload task against the local server and return its
/// answer (the Settings "Test offload" button).
#[tauri::command]
pub async fn offload_test(
    supervisor: State<'_, std::sync::Arc<crate::offload::OffloadSupervisor>>,
    instructions: String,
) -> AppResult<String> {
    let task = if instructions.trim().is_empty() {
        "Briefly confirm you are reachable and list the tools available to you.".to_string()
    } else {
        instructions
    };
    supervisor
        .inner()
        .run_task(task, crate::offload::agent::ThinkingMode::Auto)
        .await
}

/// V8-03: aggregate offload-service status — the honest global in-flight
/// count (now that the long-lived app sees every offload) and per-MCP-server
/// health rows. Drives the Settings warm-pool readout.
#[tauri::command]
pub async fn offload_service_status(
    service: State<'_, std::sync::Arc<crate::offload::OffloadService>>,
) -> AppResult<crate::offload::service::ServiceStatus> {
    Ok(service.status().await)
}

/// Reconcile the warm MCP host against the *current* settings and return the
/// fresh status. The Settings MCP editor calls this right after persisting an
/// add/remove/enable/disable so a server connects or drops live — no app
/// restart. Cheap when the pool is already warm (unchanged servers are kept).
#[tauri::command]
pub async fn offload_reload_mcp(
    service: State<'_, std::sync::Arc<crate::offload::OffloadService>>,
) -> AppResult<crate::offload::service::ServiceStatus> {
    service.warm_host().await;
    Ok(service.status().await)
}

/// V8-03: buffered `llama-server` output for a backend (primary when `name`
/// is omitted) — the read-only Settings log panel's initial fill. Live lines
/// arrive separately via the `offload-server-output` event.
#[tauri::command]
pub async fn offload_server_log(
    supervisor: State<'_, std::sync::Arc<crate::offload::OffloadSupervisor>>,
    name: Option<String>,
) -> AppResult<Vec<String>> {
    Ok(supervisor.server_logs(name))
}

/// V8-03: latest Offload Server dashboard snapshot — one row per enabled
/// backend (Local + Remote), each with slots, throughput, queue, context, and
/// request history. The initial fill for the dashboard; live updates arrive
/// via the `offload-server-metrics` event. Empty before the first poll.
#[tauri::command]
pub async fn offload_server_metrics(
    service: State<'_, std::sync::Arc<crate::offload::OffloadService>>,
) -> AppResult<Vec<crate::offload::metrics::BackendDashboard>> {
    Ok(service.server_metrics())
}

/// V9-01: known per-root code-graph status (idle/building/ready/error + row
/// counts). The initial fill for the graph status surface; live transitions
/// arrive via the `graph-status` event. Empty before the first build.
#[tauri::command]
pub async fn graph_status(
    service: State<'_, std::sync::Arc<crate::graph::GraphService>>,
) -> AppResult<Vec<crate::graph::GraphStatus>> {
    Ok(service.statuses())
}

/// V9-01: trigger a full rebuild of the project's code graph. `root` defaults
/// to the app's launch directory (the project cImp was opened in). Returns
/// immediately — the build runs on a worker thread and reports progress via
/// the `graph-status` event. A no-op when a build for that root is already in
/// flight. The store must be built before the `graph_*` MCP tools have data.
#[tauri::command]
pub async fn graph_rebuild(
    service: State<'_, std::sync::Arc<crate::graph::GraphService>>,
    root: Option<String>,
) -> AppResult<()> {
    let root = match root {
        Some(r) if !r.trim().is_empty() => std::path::PathBuf::from(r),
        _ => std::env::current_dir()
            .map_err(|e| AppError::Settings(format!("cwd: {e}")))?,
    };
    service.spawn_rebuild(root);
    Ok(())
}

/// V9-01 Phase G: force a full re-embed of the project's doc chunks (drops the
/// vector store, then backfills). The "Rebuild embeddings" action; no-op when
/// semantic search is off. `root` defaults to the launch directory.
#[tauri::command]
pub async fn graph_rebuild_embeddings(
    service: State<'_, std::sync::Arc<crate::graph::GraphService>>,
    root: Option<String>,
) -> AppResult<()> {
    let root = match root {
        Some(r) if !r.trim().is_empty() => std::path::PathBuf::from(r),
        _ => std::env::current_dir().map_err(|e| AppError::Settings(format!("cwd: {e}")))?,
    };
    service.spawn_rebuild_embeddings(root);
    Ok(())
}

/// V9-01: probe the configured embedding endpoint on demand (the monitor tab's
/// "Test connection" action). Returns reachability + the live vector dimension
/// or the exact connection error, without running a full embed backfill.
#[tauri::command]
pub async fn graph_test_embedder(
    service: State<'_, std::sync::Arc<crate::graph::GraphService>>,
) -> AppResult<crate::graph::EmbedderProbe> {
    Ok(service.test_embedder().await)
}

/// V9-01: recent graph tool calls (cloud Claude + offload worker), newest
/// first — the monitor tab's activity list.
#[tauri::command]
pub async fn graph_history() -> AppResult<Vec<crate::graph::GraphCall>> {
    Ok(crate::graph::graph_history())
}

/// V9-01: pause/resume the graph's incremental fs-watcher re-indexing. Paused
/// = file changes are ignored until resumed (a manual rebuild still works).
#[tauri::command]
pub async fn graph_set_watch_paused(
    service: State<'_, std::sync::Arc<crate::graph::GraphService>>,
    paused: bool,
) -> AppResult<bool> {
    Ok(service.set_watch_paused(paused))
}

/// Open `<portable-root>/logs/content/` in the host file manager. Creates the
/// folder first if it doesn't exist so the call doesn't 404 on a clean
/// install. Windows uses `explorer.exe`; macOS `open`; Linux
/// `xdg-open`. Errors are wrapped in `AppError::Settings` for a single
/// IPC error type.
#[tauri::command]
pub async fn content_open_folder() -> AppResult<()> {
    let dir = crate::content::dir();
    if let Err(e) = std::fs::create_dir_all(&dir) {
        return Err(AppError::Settings(format!(
            "create_dir_all {}: {e}",
            dir.display()
        )));
    }
    let result = if cfg!(target_os = "windows") {
        std::process::Command::new("explorer").arg(&dir).spawn()
    } else if cfg!(target_os = "macos") {
        std::process::Command::new("open").arg(&dir).spawn()
    } else {
        std::process::Command::new("xdg-open").arg(&dir).spawn()
    };
    result
        .map(|_| ())
        .map_err(|e| AppError::Settings(format!("open folder: {e}")))
}

/// Delete every file inside `<portable-root>/logs/content/`. Returns the
/// count of removed files. Per-file failures are logged backend-side
/// and do not abort the pass.
#[tauri::command]
pub async fn content_clear() -> AppResult<u32> {
    Ok(crate::content::delete_all())
}

#[tauri::command]
pub async fn list_voices() -> AppResult<Vec<String>> {
    use std::collections::BTreeSet;
    let mut out = BTreeSet::<String>::new();
    let dir = crate::tts::model_dir()?.join("voices");
    if let Ok(read) = std::fs::read_dir(&dir) {
        for entry in read.flatten() {
            let path = entry.path();
            if path.extension().and_then(|s| s.to_str()) != Some("bin") {
                continue;
            }
            if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
                out.insert(stem.to_string());
            }
        }
    }
    Ok(out.into_iter().collect())
}

#[tauri::command]
pub async fn open_settings_window(app: AppHandle) -> AppResult<()> {
    open_or_focus_settings(&app)
}

#[tauri::command]
pub async fn close_settings_window(app: AppHandle) -> AppResult<()> {
    use tauri::Manager;
    if let Some(w) = app.get_webview_window(SETTINGS_LABEL) {
        let _ = w.close();
    }
    Ok(())
}

/// Square off (or restore) the main window's corners. Windows 11 rounds
/// borderless windows via DWM regardless of CSS; the `tui-*` themes drop
/// the native decorations and want hard corners to match the ratatui
/// look, so the frontend calls this with `square = true` when a TUI theme
/// is active and `false` (default OS rounding) otherwise. No-op on
/// non-Windows platforms.
#[tauri::command]
pub fn set_window_square_corners(app: AppHandle, square: bool) -> AppResult<()> {
    #[cfg(windows)]
    {
        use tauri::Manager;
        let window = app
            .get_webview_window("main")
            .ok_or_else(|| AppError::Ipc("main window not found".into()))?;
        let hwnd = window
            .hwnd()
            .map_err(|e| AppError::Ipc(format!("hwnd: {e}")))?;

        // DWMWA_WINDOW_CORNER_PREFERENCE = 33; DWMWCP_DEFAULT = 0,
        // DWMWCP_DONOTROUND = 1. Declared inline so we don't pull in the
        // whole `windows` crate for a single attribute call.
        const DWMWA_WINDOW_CORNER_PREFERENCE: u32 = 33;
        const DWMWCP_DEFAULT: u32 = 0;
        const DWMWCP_DONOTROUND: u32 = 1;
        #[link(name = "dwmapi")]
        extern "system" {
            fn DwmSetWindowAttribute(
                hwnd: isize,
                attr: u32,
                pv: *const core::ffi::c_void,
                cb: u32,
            ) -> i32;
        }
        let pref: u32 = if square {
            DWMWCP_DONOTROUND
        } else {
            DWMWCP_DEFAULT
        };
        let hr = unsafe {
            DwmSetWindowAttribute(
                hwnd.0 as isize,
                DWMWA_WINDOW_CORNER_PREFERENCE,
                &pref as *const u32 as *const core::ffi::c_void,
                std::mem::size_of::<u32>() as u32,
            )
        };
        if hr < 0 {
            return Err(AppError::Ipc(format!(
                "DwmSetWindowAttribute failed: 0x{hr:08x}"
            )));
        }
    }
    #[cfg(not(windows))]
    {
        let _ = (app, square);
    }
    Ok(())
}

/// V1.4-07 A: open the Settings window scrolled to a specific tab's
/// section. The right-click "Configure tab" entry on AI tabs uses this
/// instead of the shell-only `ConfigureTabDialog.svelte`. Cold-open is
/// handled by storing the target id in `AppState.pending_settings_deep_link`
/// (the Settings window calls `consume_settings_deep_link` on mount);
/// hot-open by emitting a `settings-deep-link` event the Settings window
/// listens for. We do both so either path works without a race.
#[tauri::command]
pub async fn open_settings_window_to_tab(
    app: AppHandle,
    state: State<'_, AppState>,
    tab: String,
) -> AppResult<()> {
    if let Ok(mut slot) = state.pending_settings_deep_link.lock() {
        *slot = Some(tab.clone());
    }
    open_or_focus_settings(&app)?;
    let _ = app.emit_to(
        EventTarget::webview_window(SETTINGS_LABEL),
        "settings-deep-link",
        serde_json::json!({ "kind": "tab", "tab_id": tab }),
    );
    Ok(())
}

/// V1.4-07 A: pulled by `SettingsApp.svelte` on mount to read+clear any
/// pending deep-link target stored by `open_settings_window_to_tab`.
/// Returns `None` when no target is pending.
#[tauri::command]
pub async fn consume_settings_deep_link(state: State<'_, AppState>) -> AppResult<Option<String>> {
    Ok(state
        .pending_settings_deep_link
        .lock()
        .ok()
        .and_then(|mut g| g.take()))
}

/// Trigger a tab restart from another window (typically settings). The
/// Terminal component for the targeted tab owns the channel and sizing —
/// it does the actual `pty_restart` invocation. Routed as a frontend event
/// so the main window can keep all PTY-touching IPC in one place.
#[tauri::command]
pub async fn request_tab_restart(app: AppHandle, tab: TabId) -> AppResult<()> {
    app.emit_to(
        EventTarget::webview_window("main"),
        "tab-restart-requested",
        tab,
    )
    .map_err(|e| AppError::Ipc(format!("emit restart: {e}")))?;
    Ok(())
}

/// Restart a closed Shell tab. Driven by the closed-state overlay's
/// Enter-to-restart affordance (Phase 7). Reuses the existing
/// `tab-restart-requested` plumbing so the frontend Terminal can rebind
/// the bytes channel exactly as it does for the settings-window restart
/// path. The state manager clears the closed flag on the subsequent
/// `ShellRestarted` signal emitted from `TabRegistry::restart_tab`.
#[tauri::command]
pub async fn restart_shell_tab(app: AppHandle, tab: TabId) -> AppResult<()> {
    if !matches!(tab.kind(), crate::state::TabKind::Shell) {
        return Err(AppError::Ipc(format!(
            "restart_shell_tab: not a shell tab: {tab:?}"
        )));
    }
    app.emit_to(
        EventTarget::webview_window("main"),
        "tab-restart-requested",
        tab,
    )
    .map_err(|e| AppError::Ipc(format!("emit restart: {e}")))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::is_automatic_terminal_response as auto_reply;

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
