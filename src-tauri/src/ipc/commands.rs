use std::sync::atomic::Ordering;

use tauri::ipc::Channel;
use tauri::{AppHandle, State};

use crate::error::AppResult;
use crate::ipc::AppState;
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
    let args = state.launch.extra_args.clone();
    let tts_tx = state.tts_segments.clone();
    let user_typed = state.user_typed_tts.clone();
    let state_signals = state.state_signals.clone();
    state
        .pty
        .start(app, channel, &cwd, args, rows, cols, tts_tx, user_typed, state_signals)
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
        .map_err(|e| crate::error::AppError::Tts(format!("tts_test send: {e}")))?;
    Ok(())
}

#[tauri::command]
pub async fn pty_resize(state: State<'_, AppState>, rows: u16, cols: u16) -> AppResult<()> {
    state.pty.resize(rows, cols).await
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
