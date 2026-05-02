#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod audio;
mod error;
mod ipc;
mod processing;
mod pty;
mod settings;
mod state;
mod tts;

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::atomic::AtomicI32;
use std::sync::{Arc, Mutex, RwLock};

use tauri::{AppHandle, Emitter, Manager, WindowEvent};
use tokio::sync::mpsc;
use tracing::{info, warn};
use tracing_subscriber::{fmt, prelude::*, EnvFilter};

use crate::audio::{spawn_amplitude_streamer, AudioOutput};
use crate::error::AppError;
use crate::ipc::commands::{
    close_settings_window, compose_content_changed, list_voices, open_settings_window,
    pty_resize, pty_restart, pty_start, pty_write, request_claude_code_restart,
    settings_get, settings_update, tts_test,
};
use crate::ipc::{AppState, LaunchContext};
use crate::pty::PtyManager;
use crate::settings::{Settings, SettingsHandle};
use crate::state::{spawn_state_manager, StateSignal};
use crate::tts::{spawn_tts_worker, TtsEngine};

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

    // Settings store. Always succeeds — missing/corrupt files become defaults
    // (and are written back). Cheap to clone (Arc inside) so each subsystem
    // gets its own handle.
    let settings_handle = settings::init();

    // TTS / audio pipeline. Failures are non-fatal — the app launches with
    // TTS silent and a warning logged. The processor task always has a live
    // sender; if the worker isn't running, sends just close the receiver
    // path (debug-logged once). Init is deferred to the Tauri `setup` hook
    // because `spawn_tts_worker` requires the Tauri/tokio runtime to be live.
    let (tts_tx, tts_rx) = mpsc::channel::<String>(64);
    let tts_rx_slot = Arc::new(Mutex::new(Some(tts_rx)));

    // State-machine input channel. Created up-front so AppState can hold a
    // sender; the receiver gets handed to the manager from the Tauri setup
    // hook (which is where we first have an AppHandle for emitting events).
    let (state_tx, state_rx) = mpsc::channel::<StateSignal>(64);
    let state_rx_slot = Arc::new(Mutex::new(Some(state_rx)));

    // Shared input-length tracker. AppState owns it; the state manager
    // gets a clone so it can poll it on its tick without a round-trip
    // through the signal channel.
    let input_length = Arc::new(AtomicI32::new(0));

    let audio_slot: Arc<RwLock<Option<Arc<AudioOutput>>>> = Arc::new(RwLock::new(None));

    let state = AppState {
        pty: PtyManager::new(),
        launch: LaunchContext {
            cwd: launch_cwd,
            extra_args,
        },
        tts_segments: tts_tx,
        user_typed_tts: Arc::new(Mutex::new(HashSet::new())),
        state_signals: state_tx.clone(),
        input_length: input_length.clone(),
        settings: settings_handle.clone(),
        audio: audio_slot.clone(),
    };

    let tts_rx_for_setup = tts_rx_slot.clone();
    let state_rx_for_setup = state_rx_slot.clone();
    let audio_state_tx = state_tx.clone();
    let input_length_for_setup = input_length.clone();
    let settings_for_setup = settings_handle.clone();
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .manage(state)
        .setup(move |app| {
            if let Some(rx) = state_rx_for_setup
                .lock()
                .ok()
                .and_then(|mut g| g.take())
            {
                spawn_state_manager(app.handle().clone(), rx, input_length_for_setup.clone());
            }
            if let Some(rx) = tts_rx_for_setup
                .lock()
                .ok()
                .and_then(|mut g| g.take())
            {
                init_tts_pipeline(
                    app.handle().clone(),
                    rx,
                    audio_state_tx.clone(),
                    settings_for_setup.clone(),
                    audio_slot.clone(),
                );
            }
            // Mirror settings broadcasts onto a frontend event so every
            // window (main, settings) updates reactively without
            // round-tripping through a manual fetch.
            spawn_settings_broadcast(app.handle().clone(), settings_for_setup.clone());
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            pty_start,
            pty_restart,
            pty_write,
            pty_resize,
            tts_test,
            settings_get,
            settings_update,
            list_voices,
            open_settings_window,
            close_settings_window,
            request_claude_code_restart,
            compose_content_changed,
        ])
        .on_window_event(|window, event| {
            if let WindowEvent::CloseRequested { api, .. } = event {
                let label = window.label().to_string();
                if label != "main" {
                    // Non-main windows close normally and don't shut down
                    // the PTY. Default behavior is fine here.
                    return;
                }
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

/// Best-effort TTS bring-up. The receiver `tts_rx` is consumed by the worker
/// only if both the audio device and the model files are available;
/// otherwise it is dropped and any subsequent `Sender::send` from the
/// processor will fail benignly.
fn init_tts_pipeline(
    app: AppHandle,
    tts_rx: mpsc::Receiver<String>,
    state_signals: mpsc::Sender<StateSignal>,
    settings: SettingsHandle,
    audio_slot: Arc<RwLock<Option<Arc<AudioOutput>>>>,
) {
    let audio = match AudioOutput::new(state_signals.clone(), settings.clone()) {
        Ok(a) => Arc::new(a),
        Err(e) => {
            warn!(error = %e, "audio output unavailable; TTS will be silent");
            // Audio init failure is itself an error condition for the
            // state machine — flag it so the avatar surfaces the problem.
            let _ = state_signals.try_send(StateSignal::AudioError);
            drop(tts_rx);
            return;
        }
    };

    // Publish the audio handle so pty_write can react to interrupt-on-input.
    if let Ok(mut slot) = audio_slot.write() {
        *slot = Some(audio.clone());
    }

    // Start the M5 amplitude streamer as soon as the audio device is up,
    // independent of whether the TTS engine succeeds. If the engine fails
    // there's never any audio so the streamer just sits idle.
    spawn_amplitude_streamer(app, audio.clone());

    let model_path = match tts::default_model_path() {
        Ok(p) => p,
        Err(e) => {
            warn!(error = %e, "cannot resolve model dir");
            drop(tts_rx);
            return;
        }
    };
    let initial_voice = settings.current().tts.voice;
    let voice_path = match tts::default_voice_path(&initial_voice) {
        Ok(p) => p,
        Err(e) => {
            warn!(error = %e, "cannot resolve voice path");
            drop(tts_rx);
            return;
        }
    };

    match TtsEngine::new(&model_path, &voice_path) {
        Ok(engine) => {
            spawn_tts_worker(engine, audio, tts_rx, state_signals, settings);
        }
        Err(AppError::ModelNotFound(_)) => {
            tts::report_missing_model_files();
            drop(tts_rx);
        }
        Err(e) => {
            warn!(error = %e, "TTS engine init failed; TTS disabled");
            let _ = state_signals.try_send(StateSignal::TtsError);
            drop(tts_rx);
        }
    }
}

/// Subscribe to the settings broadcast and re-emit each update as a
/// `settings-changed` Tauri event. The frontend listens on every window so
/// any open UI (main pane, settings window) stays in sync.
fn spawn_settings_broadcast(app: AppHandle, settings: SettingsHandle) {
    tauri::async_runtime::spawn(async move {
        let mut rx = settings.subscribe();
        // Emit the current value once so any window that opens after init
        // gets the up-to-date state without an extra `settings_get` call.
        let _ = app.emit("settings-changed", settings.current());
        loop {
            match rx.recv().await {
                Ok(s) => {
                    let _ = app.emit::<Settings>("settings-changed", s);
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                    warn!(skipped = n, "settings broadcast lagged");
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            }
        }
    });
}
