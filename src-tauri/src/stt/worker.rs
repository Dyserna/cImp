//! STT transcription worker. Owns the [`SttEngine`] (constructed lazily on
//! the first recording) and runs inference on a dedicated OS thread so the
//! multi-second blocking `whisper_full` call never ties up a tokio runtime
//! worker. Receives finished 16 kHz mono recordings from the capture thread,
//! transcribes them, and emits `stt-transcription` + `stt-state` events.
//!
//! The engine is reloaded only when the user switches `stt.model_file` (the
//! model is baked into the `WhisperContext`); `language` and
//! `translate_to_english` are cheap per-call `FullParams` and need no reload.

use std::sync::mpsc::Receiver;
use std::sync::{Arc, RwLock};

use tauri::{AppHandle, Emitter};
use tracing::{debug, info, warn};

use crate::settings::SettingsHandle;
use crate::stt::engine::{SttEngine, WHISPER_SAMPLE_RATE};
use crate::stt::{set_state, SttState, WorkerMsg};

/// Recordings shorter than this (in 16 kHz samples) are treated as "didn't
/// catch that" — too brief to be real speech. ~150 ms.
const MIN_SAMPLES: usize = (WHISPER_SAMPLE_RATE as usize) * 3 / 20;

pub(crate) fn spawn_stt_worker(
    app: AppHandle,
    settings: SettingsHandle,
    jobs_rx: Receiver<WorkerMsg>,
    state: Arc<RwLock<SttState>>,
) {
    std::thread::Builder::new()
        .name("cimp-stt-worker".into())
        .spawn(move || {
            // NOTE: the whisper/ggml logging hooks are installed lazily in
            // `load_engine` (right before the first engine creation), NOT here.
            // Touching any ggml symbol with the Vulkan backend compiled in
            // initializes the GPU backend (loads the Vulkan driver, enumerates
            // devices); doing that at startup — even on this worker thread —
            // races the main thread's WebView2/window init and the app's main
            // window never appears. Deferring it to the first recording keeps
            // startup clean: this thread just blocks on `recv` until then.
            let mut engine: Option<SttEngine> = None;
            while let Ok(msg) = jobs_rx.recv() {
                let samples = match msg {
                    WorkerMsg::Transcribe(samples) => samples,
                    WorkerMsg::Unload => {
                        // `stt.enabled` flipped off: drop the model, freeing its
                        // (potentially GPU) memory. Lazy-reloads on the next
                        // recording or `Preload`.
                        if engine.take().is_some() {
                            info!(target: "stt", "STT disabled; Whisper model unloaded");
                        }
                        continue;
                    }
                    WorkerMsg::Preload => {
                        // `stt.enabled` flipped on: warm the model in the
                        // background so the first dictation isn't slowed by a
                        // cold load. Best-effort — a missing model just logs and
                        // is retried (and reported) on the first real recording.
                        let cfg = settings.current().stt;
                        if engine.as_ref().map(|e| needs_reload(e, &cfg)).unwrap_or(true) {
                            match load_engine(&cfg.model_file, cfg.device) {
                                Ok(e) => {
                                    info!(target: "stt", model = %cfg.model_file, device = ?cfg.device, "Whisper model preloaded");
                                    engine = Some(e);
                                }
                                Err(e) => {
                                    debug!(target: "stt", error = %e, model = %cfg.model_file, "preload deferred (model unavailable)");
                                    engine = None;
                                }
                            }
                        }
                        continue;
                    }
                };
                let cfg = settings.current().stt;

                if samples.len() < MIN_SAMPLES {
                    debug!(target: "stt", frames = samples.len(), "recording too short; emitting empty transcript");
                    emit_transcript(&app, "");
                    set_state(&app, &state, SttState::Idle);
                    continue;
                }

                // (Re)load the engine if absent or the model / device changed.
                if engine.as_ref().map(|e| needs_reload(e, &cfg)).unwrap_or(true) {
                    match load_engine(&cfg.model_file, cfg.device) {
                        Ok(e) => engine = Some(e),
                        Err(crate::error::AppError::ModelNotFound(_)) => {
                            crate::stt::report_missing_model_files(&cfg.model_file);
                            engine = None;
                            set_state(&app, &state, SttState::Error);
                            continue;
                        }
                        Err(e) => {
                            warn!(target: "stt", error = %e, model = %cfg.model_file, "engine load failed");
                            engine = None;
                            set_state(&app, &state, SttState::Error);
                            continue;
                        }
                    }
                }

                let started = std::time::Instant::now();
                let result = engine
                    .as_ref()
                    .expect("engine present after load")
                    .transcribe(&samples, &cfg.language, cfg.translate_to_english);

                match result {
                    Ok(text) => {
                        info!(
                            target: "stt",
                            chars = text.len(),
                            elapsed_ms = started.elapsed().as_millis(),
                            "transcription ok"
                        );
                        emit_transcript(&app, &text);
                        set_state(&app, &state, SttState::Idle);
                    }
                    Err(e) => {
                        warn!(target: "stt", error = %e, "transcription failed");
                        set_state(&app, &state, SttState::Error);
                    }
                }
            }
            debug!(target: "stt", "stt worker: jobs channel closed; exiting");
        })
        .expect("spawn stt worker thread");
}

/// True when the loaded engine no longer matches the current settings — a
/// different model file OR a different device (GPU↔CPU). Either triggers a
/// rebuild of the `WhisperContext` on the next recording / preload.
fn needs_reload(engine: &SttEngine, cfg: &crate::settings::SttSettings) -> bool {
    engine.model_file() != cfg.model_file || engine.device() != cfg.device
}

fn load_engine(
    model_file: &str,
    device: crate::settings::ProcessingDevice,
) -> crate::error::AppResult<SttEngine> {
    // Route whisper/ggml C-side logging through Rust (off raw stdout/stderr).
    // Installed here, on the first engine load (a recording), so the GPU
    // backend init it triggers never runs during app startup. Idempotent.
    whisper_rs::install_logging_hooks();
    let path = crate::stt::default_model_path(model_file)?;
    SttEngine::new(&path, model_file.to_string(), device)
}

fn emit_transcript(app: &AppHandle, text: &str) {
    if let Err(e) = app.emit("stt-transcription", serde_json::json!({ "text": text })) {
        warn!(target: "stt", error = %e, "emit stt-transcription failed");
    }
}
