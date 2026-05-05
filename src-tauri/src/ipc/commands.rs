use std::sync::atomic::Ordering;

use tauri::ipc::Channel;
use tauri::{AppHandle, Emitter, EventTarget, State};

use crate::error::{AppError, AppResult};
use crate::ipc::windows::{open_or_focus_settings, SETTINGS_LABEL};
use crate::ipc::AppState;
use crate::settings::Settings;
use crate::state::{StateSignal, TabId};

#[tauri::command]
pub async fn pty_start(
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
    registry
        .start_tab(
            app,
            tab,
            channel,
            rows,
            cols,
            &cwd,
            &invocation_args,
            tts_tx,
            user_typed,
            settings,
        )
        .await
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
    registry
        .restart_tab(
            app,
            tab,
            channel,
            rows,
            cols,
            &cwd,
            &invocation_args,
            tts_tx,
            user_typed,
            settings,
        )
        .await
}

#[tauri::command]
pub async fn pty_write(
    state: State<'_, AppState>,
    tab: TabId,
    input: String,
) -> AppResult<()> {
    // Pre-register any TTS markers in the user's input so they don't fire
    // when echoed back by the TUI. Content-based; no per-tab scoping needed.
    let typed_tags = extract_tts_contents(&input);
    if !typed_tags.is_empty() {
        if let Ok(mut set) = state.user_typed_tts.lock() {
            for content in typed_tags {
                set.insert(content);
            }
        }
    }

    let len_counter = state
        .input_lengths
        .get(&tab)
        .ok_or_else(|| AppError::Pty(format!("unknown tab {tab:?}")))?;

    if !is_automatic_terminal_response(&input) {
        if contains_enter(&input) {
            len_counter.store(0, Ordering::Relaxed);
            let _ = state.state_signals.try_send(StateSignal::UserSubmit { tab });
        } else {
            apply_input_delta(&input, len_counter);
            let _ = state
                .state_signals
                .try_send(StateSignal::UserKeystroke { tab });
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
/// reconcile from a single source of truth.
#[tauri::command]
pub async fn tab_activate(state: State<'_, AppState>, tab: TabId) -> AppResult<()> {
    let mut registry = state.tabs.lock().await;
    registry.activate(tab).await
}

#[tauri::command]
pub async fn settings_get(state: State<'_, AppState>) -> AppResult<Settings> {
    Ok(state.settings.current())
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

/// Trigger a Claude tab restart. Implemented as a frontend event because the
/// Terminal component owns the channel and sizing — it does the actual
/// `pty_restart` invocation on the Claude tab specifically.
#[tauri::command]
pub async fn request_claude_code_restart(app: AppHandle) -> AppResult<()> {
    app.emit_to(
        EventTarget::webview_window("main"),
        "claude-code-restart",
        (),
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
