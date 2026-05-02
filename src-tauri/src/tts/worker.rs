//! TTS worker task. Receives plain text segments from the processing layer,
//! runs them through the [`TtsEngine`], and pushes resulting PCM into the
//! shared [`AudioOutput`]. Synthesis errors are logged and skipped — a single
//! bad segment does not take down the pipeline.

use std::sync::Arc;

use tokio::sync::mpsc;
use tracing::{debug, warn};

use crate::audio::AudioOutput;
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
) {
    tauri::async_runtime::spawn(async move {
        let mut next_id: u64 = 0;
        while let Some(text) = rx.recv().await {
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
        debug!("tts worker: segment channel closed; exiting");
    });
}
