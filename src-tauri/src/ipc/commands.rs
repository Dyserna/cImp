use std::sync::atomic::Ordering;

use tauri::ipc::Channel;
use tauri::{AppHandle, Emitter, EventTarget, State};

use crate::error::{AppError, AppResult};
use crate::ipc::windows::{open_or_focus_settings, SETTINGS_LABEL};
use crate::ipc::AppState;
use crate::settings::{
    default_aider_tab, default_claude_tab, AiToolTabConfig, Settings, TabConfig, AIDER_TAB_ID,
    CLAUDE_TAB_ID,
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

    // V1.4-04 D.5: read-and-consume any persisted scrollback for this
    // tab. Done after a successful start so a spawn failure doesn't
    // burn the persisted bytes.
    if !restore_on_launch {
        return Ok(None);
    }
    let restored = crate::pty::scrollback::take(&tab);
    if let Some(bytes) = &restored {
        // Seed the new ring with the restored bytes so a subsequent
        // crash-restart still has them. Logging-only on error: the
        // user-visible replay (the returned bytes) succeeds regardless.
        if let Err(e) = registry.seed_scrollback(&tab, bytes).await {
            tracing::warn!(?tab, error = %e, "scrollback seed failed");
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
        }
    }

    // Clone the counter Arc out so we don't hold the read lock across the
    // subsequent .await on the registry. Counters are cheap to clone.
    let len_counter = {
        let map = state
            .input_lengths
            .read()
            .map_err(|e| AppError::Pty(format!("input_lengths poisoned: {e}")))?;
        map.get(&tab)
            .cloned()
            .ok_or_else(|| AppError::Pty(format!("unknown tab {tab:?}")))?
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
            // Interrupt-on-input only fires when the typed-into tab is the
            // one actually playing. The audio output is shared; we only
            // need the active-tab check at the registry level (the audio
            // belongs to whichever tab is active). Reading active is cheap.
            if state.settings.current().behavior.interrupt_on_input {
                let active = state.tabs.lock().await.active();
                if tab == active {
                    if let Ok(slot) = state.audio.read() {
                        if let Some(audio) = slot.as_ref() {
                            if audio.is_playing() {
                                audio.stop_all();
                            }
                        }
                    }
                }
            }
        }
    }

    let registry = state.tabs.lock().await;
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
        .send(crate::tts::TtsRequest::Synthesize { tab: active, text })
        .await
        .map_err(|e| AppError::Tts(format!("tts_test send: {e}")))?;
    Ok(())
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
    let mut snap = state.settings.current();
    if snap.session.active_tab_id.as_deref() != Some(id_string.as_str()) {
        snap.session.active_tab_id = Some(id_string);
        state.settings.set(snap);
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
/// constants (notably `RUNTIME_SYSTEM_PROMPT` for Claude's TTS instructions).
///
/// Only AI tabs have a meaningful "default" in v1.2 — Shell tab defaults
/// depend on the host platform's auto-detected shell, and "reset" on a
/// user-created Shell tab is not a meaningful UX (use the New Shell Tab
/// dialog to spawn a fresh one). Shell ids return an error.
#[tauri::command]
pub async fn ai_tool_tab_defaults(tab: TabId) -> AppResult<AiToolTabConfig> {
    let config = match tab.as_str() {
        CLAUDE_TAB_ID => default_claude_tab(),
        AIDER_TAB_ID => default_aider_tab(),
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
    settings: Settings,
) -> AppResult<()> {
    state.settings.set(settings);
    Ok(())
}

#[tauri::command]
pub async fn list_voices() -> AppResult<Vec<String>> {
    let dir = match crate::tts::default_model_dir() {
        Ok(d) => d.join("voices"),
        Err(_) => return Ok(Vec::new()),
    };
    let mut out = Vec::new();
    let read = match std::fs::read_dir(&dir) {
        Ok(r) => r,
        Err(_) => return Ok(Vec::new()),
    };
    for entry in read.flatten() {
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("bin") {
            continue;
        }
        if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
            out.push(stem.to_string());
        }
    }
    out.sort();
    Ok(out)
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
