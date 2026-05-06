#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod audio;
mod error;
mod ipc;
mod notifications;
mod processing;
mod pty;
mod settings;
mod shell;
mod state;
mod tabs;
mod tts;

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, RwLock};

use tauri::{AppHandle, Emitter, Manager, WindowEvent};
use tokio::sync::{broadcast, mpsc, Mutex as TokioMutex};
use tracing::{info, warn};
use tracing_subscriber::{fmt, prelude::*, EnvFilter};

use crate::audio::{spawn_amplitude_streamer, AudioOutput};
use crate::error::AppError;
use crate::ipc::commands::{
    acknowledge_error, close_settings_window, compose_content_changed, list_tabs, list_voices,
    open_settings_window, pty_resize, pty_restart, pty_start, pty_write, request_tab_restart,
    restart_shell_tab, settings_get, settings_update, tab_activate, tab_default_settings,
    tts_test,
};
use crate::ipc::tab_lifecycle::{
    close_tab, create_shell_tab, default_shell_spec, get_shell_tab_config, reconfigure_shell_tab,
    rename_tab,
};
use crate::ipc::{AppState, LaunchContext};
use crate::settings::{Settings, SettingsHandle};
use crate::state::{spawn_state_manager, AiToolKind, StateEvent, StateSignal, TabId, TabKind, TabMeta};
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

    // Probe the platform default shell once and cache it for every Shell
    // tab launch path. The cache is an `Arc` so the registry, settings
    // window, and the new-shell-tab dialog (M2) can all share it without
    // re-running detection.
    let default_shell = Arc::new(shell::detect::default_shell());

    let settings_handle = settings::init();

    // TTS / audio pipeline. Failures are non-fatal — the app launches with
    // TTS silent and a warning logged. Init is deferred to the Tauri `setup`
    // hook because spawn_tts_worker requires the Tauri/tokio runtime.
    let (tts_tx, tts_rx) = mpsc::channel::<TtsRequest>(64);
    let tts_rx_slot = Arc::new(Mutex::new(Some(tts_rx)));

    // State-machine input channel.
    let (state_tx, state_rx) = mpsc::channel::<StateSignal>(64);
    let state_rx_slot = Arc::new(Mutex::new(Some(state_rx)));

    // In-process broadcast of every StateEvent the manager emits to the
    // frontend. Subscribed by the notification manager (V2-04) so it can
    // queue announcements off the same edges the avatar reacts to. Capacity
    // 64 matches the input channel; lag here means a notification missed an
    // edge, which the next event recovers naturally.
    let (state_event_tx, _) = broadcast::channel::<StateEvent>(64);

    // Launch-seed tab list. M2 supports runtime add/remove via the
    // state-manager's `TabAdded`/`TabRemoved` signals; this list only
    // determines which tabs spawn at app launch. M3 reads it from settings.
    let seed_tabs: Vec<TabId> = TabId::launch_seed();

    // Per-tab unsent-input length counters. Shared (Arc<RwLock<...>>) so
    // the state manager can grow/shrink the map at runtime while the IPC
    // layer reads counter Arcs by tab id.
    let input_lengths = crate::tabs::registry::make_input_lengths(&seed_tabs);

    let audio_slot: Arc<RwLock<Option<Arc<AudioOutput>>>> = Arc::new(RwLock::new(None));

    // Active-tab cell shared with the TTS worker (filters background-tab
    // synthesis requests) and the audio thread (tags TtsPlaybackStarted/
    // Stopped signals with the speaking tab). Synchronous so both consumers
    // can read it without runtime gymnastics.
    let initial_active = TabId::Claude;
    let tts_active: ActiveTab = Arc::new(RwLock::new(initial_active.clone()));

    // Static tab metadata for the launch seed. M2 keeps the same three
    // tabs at startup; runtime additions arrive via `create_shell_tab`.
    // The Shell-1 name comes from the interim `_shell_1_tmp` key so
    // user-edited names are honored on launch.
    let shell_1_name = settings_handle.current().shell_1_tmp.name.clone();
    let tab_metas: Vec<TabMeta> = vec![
        TabMeta {
            id: TabId::Claude,
            kind: TabKind::AiTool(AiToolKind::ClaudeCode),
            name: "Claude".to_string(),
        },
        TabMeta {
            id: TabId::Aider,
            kind: TabKind::AiTool(AiToolKind::Aider),
            name: "Aider".to_string(),
        },
        TabMeta {
            id: TabId::Shell("shell-1".to_string()),
            kind: TabKind::Shell,
            name: shell_1_name,
        },
    ];

    // Tab registry — one PtyManager per tab, lazy-spawn at frontend mount.
    let registry = TabRegistry::new(
        tab_metas.clone(),
        initial_active.clone(),
        tts_active.clone(),
        audio_slot.clone(),
        state_tx.clone(),
        default_shell.clone(),
        input_lengths.clone(),
    );
    let tabs_handle: TabRegistryHandle = Arc::new(TokioMutex::new(registry));

    // Clone the TTS sender once for the notification manager — AppState
    // gets the original. Both producers race to put work on the same mpsc;
    // the worker filters/synthesizes serially.
    let tts_tx_for_notifications = tts_tx.clone();

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
    let state_events_for_setup = state_event_tx.clone();
    let state_events_for_notifications = state_event_tx.clone();
    let audio_state_tx = state_tx.clone();
    let input_lengths_for_setup = input_lengths.clone();
    let settings_for_setup = settings_handle.clone();
    let settings_for_notifications = settings_handle.clone();
    let tts_active_for_setup = tts_active.clone();
    let tts_active_for_notifications = tts_active.clone();
    let initial_active_for_state = initial_active.clone();
    let initial_active_for_tts = initial_active.clone();
    let initial_active_for_notifications = initial_active.clone();
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
                    state_events_for_setup.clone(),
                    input_lengths_for_setup.clone(),
                    tab_metas.clone(),
                    initial_active_for_state,
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
                    initial_active_for_tts,
                );
                // Notification manager piggybacks on the audio output we
                // just built. If audio init failed above, audio_slot is
                // None and we skip — without audio there's nothing to
                // play and nothing to wait on.
                if let Some(audio) = audio_slot
                    .read()
                    .ok()
                    .and_then(|g| g.as_ref().cloned())
                {
                    notifications::spawn_notification_manager(
                        state_events_for_notifications.subscribe(),
                        audio,
                        tts_tx_for_notifications.clone(),
                        settings_for_notifications.clone(),
                        tts_active_for_notifications.clone(),
                        initial_active_for_notifications,
                    );
                }
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
            list_tabs,
            open_settings_window,
            close_settings_window,
            request_tab_restart,
            restart_shell_tab,
            compose_content_changed,
            acknowledge_error,
            tab_activate,
            create_shell_tab,
            close_tab,
            rename_tab,
            reconfigure_shell_tab,
            default_shell_spec,
            get_shell_tab_config,
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
            let _ = state_signals.try_send(StateSignal::AudioError { tab: initial_active.clone() });
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
            let _ = state_signals.try_send(StateSignal::TtsError { tab: initial_active.clone() });
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
