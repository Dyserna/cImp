use std::sync::atomic::Ordering;

use tauri::ipc::Channel;
use tauri::{AppHandle, Emitter, EventTarget, State};

use crate::error::{AppError, AppResult};
use crate::ipc::windows::{open_or_focus_settings, SETTINGS_LABEL};
use crate::ipc::AppState;
use crate::settings::Settings;
use crate::state::StateSignal;

#[tauri::command]
pub async fn pty_start(
    app: AppHandle,
    state: State<'_, AppState>,
    channel: Channel<String>,
    rows: u16,
    cols: u16,
) -> AppResult<()> {
    let cwd = state.launch.cwd.clone();
    let settings = state.settings.current();
    let extra_args = combine_extra_args(&state.launch.extra_args, &settings);
    let system_prompt = resolve_system_prompt(&settings);
    let tts_tx = state.tts_segments.clone();
    let user_typed = state.user_typed_tts.clone();
    let state_signals = state.state_signals.clone();
    state
        .pty
        .start(
            app,
            channel,
            &cwd,
            extra_args,
            system_prompt,
            rows,
            cols,
            tts_tx,
            user_typed,
            state_signals,
        )
        .await
}

/// Tear down + bring up the PTY with the latest settings (CLI flags +
/// CLAUDE.md path). Frontend supplies a fresh Channel (the previous one is
/// gone with the previous session) and the current term size.
#[tauri::command]
pub async fn pty_restart(
    app: AppHandle,
    state: State<'_, AppState>,
    channel: Channel<String>,
    rows: u16,
    cols: u16,
) -> AppResult<()> {
    state.pty.shutdown().await?;
    let cwd = state.launch.cwd.clone();
    let settings = state.settings.current();
    let extra_args = combine_extra_args(&state.launch.extra_args, &settings);
    let system_prompt = resolve_system_prompt(&settings);
    let tts_tx = state.tts_segments.clone();
    let user_typed = state.user_typed_tts.clone();
    let state_signals = state.state_signals.clone();
    state
        .pty
        .start(
            app,
            channel,
            &cwd,
            extra_args,
            system_prompt,
            rows,
            cols,
            tts_tx,
            user_typed,
            state_signals,
        )
        .await
}

#[tauri::command]
pub async fn pty_write(state: State<'_, AppState>, input: String) -> AppResult<()> {
    // Pre-register any TTS markers in the user's input. The scanner consults
    // this set when extracting tags from PTY output and skips contents that
    // match — so when Claude's TUI echoes the user's typed/pasted markers
    // back, they don't trigger TTS. Content-based, no timing involved.
    let typed_tags = extract_tts_contents(&input);
    if !typed_tags.is_empty() {
        if let Ok(mut set) = state.user_typed_tts.lock() {
            for content in typed_tags {
                set.insert(content);
            }
        }
    }
    // Translate user input into the right state signal and update the
    // input-length tracker the state manager polls for auto-leaving
    // Listening.
    //   - CR present → UserSubmit (Listening → Thinking) AND zero out the
    //     length (the line was sent).
    //   - Auto-reply (focus, DA, cursor-position) → no signal, no length
    //     change (these flap during window drags).
    //   - Plain keystroke → UserKeystroke; apply the per-byte delta to
    //     the length counter.
    if !is_automatic_terminal_response(&input) {
        if contains_enter(&input) {
            state.input_length.store(0, Ordering::Relaxed);
            let _ = state.state_signals.try_send(StateSignal::UserSubmit);
        } else {
            apply_input_delta(&input, &state.input_length);
            let _ = state.state_signals.try_send(StateSignal::UserKeystroke);
            // interrupt-on-input: typing during TTS playback aborts the
            // current speech so the user can take over. Lookup-only fast
            // path — if either flag is off or audio is silent, no-op.
            if state.settings.current().behavior.interrupt_on_input {
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
    state.pty.write_input(input.into_bytes()).await
}

/// Treat any CR (`\r`) in the input as a submit. xterm.js sends CR for the
/// Enter key by default; LF (`\n`) we also accept defensively in case the
/// frontend or a paste source uses it. We do NOT include LF in xterm's
/// usual key mapping; both being recognized just makes the signal robust.
fn contains_enter(input: &str) -> bool {
    input.chars().any(|c| c == '\r' || c == '\n')
}

/// Approximate the change to the unsent input buffer for one PTY write.
/// We can't see Claude's input box from here, so this models the common
/// edits and ignores the rest:
///
/// - Printable / unicode chars: +1 each
/// - Backspace (`\x08`) and DEL (`\x7f`): -1 (saturating at 0)
/// - Ctrl+U (`\x15`, kill line) and Ctrl+K (`\x0b`, kill to EOL): reset to 0
/// - Ctrl+W (`\x17`, kill word): -4 (rough average)
/// - ESC sequences (arrow keys, function keys, etc.): no change
/// - Other control bytes: no change
///
/// Result is never negative. Drift from the true buffer length is fine —
/// the counter is only used to gate one transition (Listening → Idle when
/// empty + idle).
fn apply_input_delta(input: &str, length: &std::sync::atomic::AtomicI32) {
    if input.starts_with('\x1b') {
        // ESC-prefixed sequence: arrows, function keys, etc. No content delta.
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

/// True if `input` looks like an xterm-side automatic reply rather than a
/// keystroke. We catch the common cases that flow through `term.onData`:
///
/// - Focus events: `\x1b[I`, `\x1b[O` (only when the app enables focus mode
///   via DECSET 1004; many TUIs do).
/// - CSI replies ending in `R` (cursor position), `c` (device attributes),
///   `n` (device status). These contain only digits, `;`, and an optional
///   `?` prefix in the parameter region.
///
/// Real keystrokes either don't start with `ESC [` at all (printables, CR,
/// BS, TAB, Esc alone) or end in different terminators (`A`/`B`/`C`/`D`/`H`
/// for arrows/home, `~` for function keys), so they aren't caught.
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
/// the processor. Lets us isolate audio/synthesis problems from tag-detection
/// problems.
#[tauri::command]
pub async fn tts_test(state: State<'_, AppState>, text: String) -> AppResult<()> {
    state
        .tts_segments
        .send(text)
        .await
        .map_err(|e| AppError::Tts(format!("tts_test send: {e}")))?;
    Ok(())
}

#[tauri::command]
pub async fn pty_resize(state: State<'_, AppState>, rows: u16, cols: u16) -> AppResult<()> {
    state.pty.resize(rows, cols).await
}

// --- settings IPC -----------------------------------------------------------

#[tauri::command]
pub async fn settings_get(state: State<'_, AppState>) -> AppResult<Settings> {
    Ok(state.settings.current())
}

/// Replace the full settings struct. Backend broadcasts the new value to
/// every subscriber and triggers a debounced disk save.
#[tauri::command]
pub async fn settings_update(
    state: State<'_, AppState>,
    settings: Settings,
) -> AppResult<()> {
    state.settings.set(settings);
    Ok(())
}

/// Enumerate available Kokoro voices by scanning the voicepack directory.
/// Returns names without the `.bin` extension. An empty list means no
/// voicepacks are installed (or the directory is missing) — callers should
/// fall back to a single-entry list with the default voice.
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

/// Close the settings window if it's open. Used by the close button inside
/// the settings UI itself.
#[tauri::command]
pub async fn close_settings_window(app: AppHandle) -> AppResult<()> {
    use tauri::Manager;
    if let Some(w) = app.get_webview_window(SETTINGS_LABEL) {
        let _ = w.close();
    }
    Ok(())
}

/// Trigger a Claude Code subprocess restart. Implemented as an event
/// emitted to the main window — the Terminal component owns the channel and
/// rows/cols, so it does the actual `pty_restart` invocation. Decoupling it
/// this way means the settings window doesn't need access to the terminal
/// state.
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

fn combine_extra_args(launch: &[String], settings: &Settings) -> Vec<String> {
    launch
        .iter()
        .chain(settings.claude_code.extra_cli_args.iter())
        .filter(|s| !s.is_empty())
        .cloned()
        .collect()
}

fn resolve_system_prompt(settings: &Settings) -> String {
    if let Some(p) = settings.claude_code.claude_md_path.as_ref() {
        if p.exists() {
            match std::fs::read_to_string(p) {
                Ok(text) => return text,
                Err(e) => tracing::warn!(
                    error = %e,
                    path = %p.display(),
                    "claude_md_path read failed; using embedded prompt"
                ),
            }
        } else {
            tracing::warn!(path = %p.display(), "claude_md_path does not exist; using embedded prompt");
        }
    }
    crate::tts::RUNTIME_SYSTEM_PROMPT.to_string()
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
        // Alt-style: ESC + char (no [)
        assert!(!auto_reply("\x1bf"));
    }
}
