#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod audio;
mod error;
mod ipc;
mod processing;
mod pty;
mod settings;
mod state;
mod tabs;
mod tts;

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, RwLock};

use tauri::{AppHandle, Emitter, Manager, WindowEvent};
use tokio::sync::{mpsc, Mutex as TokioMutex};
use tracing::{info, warn};
use tracing_subscriber::{fmt, prelude::*, EnvFilter};

use crate::audio::{spawn_amplitude_streamer, AudioOutput};
use crate::error::AppError;
use crate::ipc::commands::{
    acknowledge_error, close_settings_window, compose_content_changed, list_voices,
    open_settings_window, pty_resize, pty_restart, pty_start, pty_write, request_tab_restart,
    settings_get, settings_update, tab_activate, tab_default_settings, tts_test,
};
use crate::ipc::{AppState, LaunchContext};
use crate::settings::{Settings, SettingsHandle};
use crate::state::{spawn_state_manager, StateSignal, TabId};
use crate::tabs::{TabRegistry, TabRegistryHandle};
use crate::tts::{spawn_tts_worker, ActiveTab, TtsEngine, TtsRequest};

fn main() {
    let launch_cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let extra_args: Vec<String> = std::env::args().skip(1).collect();

    init_tracing();
    info!(
        cwd = %launch_cwd.display(),
        args = ?extra_args,
        "cctts starting"
    );

    let settings_handle = settings::init();

    // TTS / audio pipeline. Failures are non-fatal — the app launches with
    // TTS silent and a warning logged. Init is deferred to the Tauri `setup`
    // hook because spawn_tts_worker requires the Tauri/tokio runtime.
    let (tts_tx, tts_rx) = mpsc::channel::<TtsRequest>(64);
    let tts_rx_slot = Arc::new(Mutex::new(Some(tts_rx)));

    // State-machine input channel.
    let (state_tx, state_rx) = mpsc::channel::<StateSignal>(64);
    let state_rx_slot = Arc::new(Mutex::new(Some(state_rx)));

    // Per-tab unsent-input length counters.
    let input_lengths = crate::tabs::registry::make_input_lengths();

    let audio_slot: Arc<RwLock<Option<Arc<AudioOutput>>>> = Arc::new(RwLock::new(None));

    // Active-tab cell shared with the TTS worker (filters background-tab
    // synthesis requests) and the audio thread (tags TtsPlaybackStarted/
    // Stopped signals with the speaking tab). Synchronous so both consumers
    // can read it without runtime gymnastics.
    let initial_active = TabId::Claude;
    let tts_active: ActiveTab = Arc::new(RwLock::new(initial_active));

    // Tab registry — one PtyManager per tab, lazy-spawn at frontend mount.
    let registry = TabRegistry::new(
        initial_active,
        tts_active.clone(),
        audio_slot.clone(),
        state_tx.clone(),
    );
    let tabs_handle: TabRegistryHandle = Arc::new(TokioMutex::new(registry));

    let state = AppState {
        tabs: tabs_handle.clone(),
        launch: LaunchContext {
            cwd: launch_cwd,
            extra_args,
        },
        tts_segments: tts_tx,
        user_typed_tts: Arc::new(Mutex::new(HashSet::new())),
        state_signals: state_tx.clone(),
        input_lengths: input_lengths.clone(),
        settings: settings_handle.clone(),
        audio: audio_slot.clone(),
    };

    let tts_rx_for_setup = tts_rx_slot.clone();
    let state_rx_for_setup = state_rx_slot.clone();
    let audio_state_tx = state_tx.clone();
    let input_lengths_for_setup = input_lengths.clone();
    let settings_for_setup = settings_handle.clone();
    let tts_active_for_setup = tts_active.clone();
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .manage(state)
        .setup(move |app| {
            if let Some(rx) = state_rx_for_setup
                .lock()
                .ok()
                .and_then(|mut g| g.take())
            {
                spawn_state_manager(
                    app.handle().clone(),
                    rx,
                    input_lengths_for_setup.clone(),
                    initial_active,
                );
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
                    tts_active_for_setup.clone(),
                    initial_active,
                );
            }
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
            tab_default_settings,
            list_voices,
            open_settings_window,
            close_settings_window,
            request_tab_restart,
            compose_content_changed,
            acknowledge_error,
            tab_activate,
        ])
        .on_window_event(|window, event| {
            if let WindowEvent::CloseRequested { api, .. } = event {
                let label = window.label().to_string();
                if label != "main" {
                    return;
                }
                api.prevent_close();
                let window = window.clone();
                let app = window.app_handle().clone();
                tauri::async_runtime::spawn(async move {
                    let state = app.state::<AppState>();
                    let registry = state.tabs.lock().await;
                    registry.shutdown_all().await;
                    drop(registry);
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

fn init_tts_pipeline(
    app: AppHandle,
    tts_rx: mpsc::Receiver<TtsRequest>,
    state_signals: mpsc::Sender<StateSignal>,
    settings: SettingsHandle,
    audio_slot: Arc<RwLock<Option<Arc<AudioOutput>>>>,
    active: ActiveTab,
    initial_active: TabId,
) {
    let audio = match AudioOutput::new(state_signals.clone(), settings.clone(), active.clone()) {
        Ok(a) => Arc::new(a),
        Err(e) => {
            warn!(error = %e, "audio output unavailable; TTS will be silent");
            let _ = state_signals.try_send(StateSignal::AudioError { tab: initial_active });
            drop(tts_rx);
            return;
        }
    };

    if let Ok(mut slot) = audio_slot.write() {
        *slot = Some(audio.clone());
    }

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
            spawn_tts_worker(engine, audio, tts_rx, state_signals, settings, active);
        }
        Err(AppError::ModelNotFound(_)) => {
            tts::report_missing_model_files();
            drop(tts_rx);
        }
        Err(e) => {
            warn!(error = %e, "TTS engine init failed; TTS disabled");
            let _ = state_signals.try_send(StateSignal::TtsError { tab: initial_active });
            drop(tts_rx);
        }
    }
}

fn spawn_settings_broadcast(app: AppHandle, settings: SettingsHandle) {
    tauri::async_runtime::spawn(async move {
        let mut rx = settings.subscribe();
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
