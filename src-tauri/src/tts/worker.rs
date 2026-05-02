//! TTS worker task. Receives plain text segments from the processing layer,
//! runs them through the [`TtsEngine`], and pushes resulting PCM into the
//! shared [`AudioOutput`]. Synthesis errors are logged and skipped — a single
//! bad segment does not take down the pipeline.
//!
//! The worker also subscribes to [`SettingsHandle`] updates so a voice or
//! speed change applies to the very next synthesis (no engine restart).

use std::sync::Arc;

use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use crate::audio::AudioOutput;
use crate::settings::{Settings, SettingsHandle};
use crate::state::StateSignal;
use crate::tts::engine::{TtsEngine, TtsRequest};

/// Shutdown signal: the worker exits when the segment channel is closed,
/// which happens automatically when the last `Sender` (held by `AppState`)
/// is dropped during app teardown. No cancellation token needed at this
/// scope.
///
/// Uses `tauri::async_runtime::spawn` rather than `tokio::spawn` directly so
/// the call site doesn't have to be inside an active tokio runtime — Tauri
/// owns the runtime and exposes a runtime-agnostic spawn entry point.
pub fn spawn_tts_worker(
    mut engine: TtsEngine,
    audio: Arc<AudioOutput>,
    mut rx: mpsc::Receiver<String>,
    state_signals: mpsc::Sender<StateSignal>,
    settings: SettingsHandle,
) {
    tauri::async_runtime::spawn(async move {
        let mut next_id: u64 = 0;
        let mut settings_rx = settings.subscribe();
        // Apply the current snapshot up front in case the user changed
        // speed/voice between init and the first segment.
        apply_settings(&mut engine, &settings.current());

        loop {
            tokio::select! {
                biased;
                changed = settings_rx.recv() => {
                    match changed {
                        Ok(s) => apply_settings(&mut engine, &s),
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                    }
                }
                seg = rx.recv() => {
                    let Some(text) = seg else { break };
                    next_id += 1;
                    let req = TtsRequest { text, request_id: next_id };
                    let started = std::time::Instant::now();
                    match engine.synthesize(req) {
                        Ok(resp) => {
                            let elapsed_ms = started.elapsed().as_millis();
                            debug!(
                                request_id = resp.request_id,
                                samples = resp.samples.len(),
                                elapsed_ms,
                                "tts synthesis ok"
                            );
                            audio.enqueue(resp.samples, resp.sample_rate);
                        }
                        Err(e) => {
                            // Per-segment failures are recoverable: the worker keeps
                            // accepting subsequent segments. We don't fire TtsError
                            // here (which would park the avatar in Error with no
                            // acknowledgment UI in M4). Engine init failures fire
                            // TtsError separately from main.rs.
                            let _ = &state_signals; // keep param: future fatal-error path
                            warn!(error = %e, "tts synthesis failed; skipping segment");
                        }
                    }
                }
            }
        }
        debug!("tts worker: segment or settings channel closed; exiting");
    });
}

fn apply_settings(engine: &mut TtsEngine, s: &Settings) {
    engine.set_speed(s.tts.speed);
    if engine.current_voice_name() != s.tts.voice {
        match crate::tts::default_voice_path(&s.tts.voice) {
            Ok(p) => match engine.set_voice(&p) {
                Ok(()) => info!(voice = %s.tts.voice, "tts: voice changed"),
                Err(e) => warn!(error = %e, voice = %s.tts.voice, "tts: voice swap failed"),
            },
            Err(e) => warn!(error = %e, voice = %s.tts.voice, "tts: cannot resolve voice path"),
        }
    }
}
