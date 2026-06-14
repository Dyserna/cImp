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
use crate::stt::{set_state, SttState};

/// Recordings shorter than this (in 16 kHz samples) are treated as "didn't
/// catch that" — too brief to be real speech. ~150 ms.
const MIN_SAMPLES: usize = (WHISPER_SAMPLE_RATE as usize) * 3 / 20;

pub(crate) fn spawn_stt_worker(
    app: AppHandle,
    settings: SettingsHandle,
    jobs_rx: Receiver<Vec<f32>>,
    state: Arc<RwLock<SttState>>,
) {
    std::thread::Builder::new()
        .name("cctts-stt-worker".into())
        .spawn(move || {
            let mut engine: Option<SttEngine> = None;
            while let Ok(samples) = jobs_rx.recv() {
                let cfg = settings.current().stt;

                if samples.len() < MIN_SAMPLES {
                    debug!(target: "stt", frames = samples.len(), "recording too short; emitting empty transcript");
                    emit_transcript(&app, "");
                    set_state(&app, &state, SttState::Idle);
                    continue;
                }

                // (Re)load the engine if absent or the model changed.
                if engine.as_ref().map(|e| e.model_file() != cfg.model_file).unwrap_or(true) {
                    match load_engine(&cfg.model_file) {
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

fn load_engine(model_file: &str) -> crate::error::AppResult<SttEngine> {
    let path = crate::stt::default_model_path(model_file)?;
    SttEngine::new(&path, model_file.to_string())
}

fn emit_transcript(app: &AppHandle, text: &str) {
    if let Err(e) = app.emit("stt-transcription", serde_json::json!({ "text": text })) {
        warn!(target: "stt", error = %e, "emit stt-transcription failed");
    }
}
