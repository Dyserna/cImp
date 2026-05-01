#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod error;
mod ipc;
mod pty;

use std::path::PathBuf;

use tauri::{Manager, WindowEvent};
use tracing::info;
use tracing_subscriber::{fmt, prelude::*, EnvFilter};

use crate::ipc::commands::{pty_resize, pty_start, pty_write};
use crate::ipc::{AppState, LaunchContext};
use crate::pty::PtyManager;

fn main() {
    // Capture launch context before any initialization that could change cwd or
    // consume args. These get forwarded verbatim to the spawned `claude` subprocess.
    let launch_cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let extra_args: Vec<String> = std::env::args().skip(1).collect();

    init_tracing();
    info!(
        cwd = %launch_cwd.display(),
        args = ?extra_args,
        "cctts starting"
    );

    let state = AppState {
        pty: PtyManager::new(),
        launch: LaunchContext {
            cwd: launch_cwd,
            extra_args,
        },
    };

    tauri::Builder::default()
        .manage(state)
        .invoke_handler(tauri::generate_handler![pty_start, pty_write, pty_resize])
        .on_window_event(|window, event| {
            if let WindowEvent::CloseRequested { api, .. } = event {
                api.prevent_close();
                let window = window.clone();
                let app = window.app_handle().clone();
                tauri::async_runtime::spawn(async move {
                    let state = app.state::<AppState>();
                    let _ = state.pty.shutdown().await;
                    let _ = window.destroy();
                });
            }
        })
        .run(tauri::generate_context!())
        .expect("failed to launch tauri app");
}

fn init_tracing() {
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("info,cctts=debug"));
    tracing_subscriber::registry()
        .with(filter)
        .with(fmt::layer().with_target(true).with_thread_ids(false))
        .init();
}
