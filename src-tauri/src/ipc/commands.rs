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
    state
        .pty
        .start(app, channel, &cwd, args, rows, cols)
        .await
}

#[tauri::command]
pub async fn pty_write(state: State<'_, AppState>, input: String) -> AppResult<()> {
    state.pty.write_input(input.into_bytes()).await
}

#[tauri::command]
pub async fn pty_resize(state: State<'_, AppState>, rows: u16, cols: u16) -> AppResult<()> {
    state.pty.resize(rows, cols).await
}
