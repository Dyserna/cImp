use tauri::ipc::Channel;
use tauri::{AppHandle, State};

use crate::error::AppResult;
use crate::ipc::AppState;

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
    state
        .pty
        .start(app, channel, &cwd, args, rows, cols, tts_tx, user_typed)
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
    state.pty.write_input(input.into_bytes()).await
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
