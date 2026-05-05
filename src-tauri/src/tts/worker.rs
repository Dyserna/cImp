//! TTS worker task. Receives [`TtsRequest`]s from the per-tab processing
//! layers, filters by the shared active-tab cell (background-tab synthesis is
//! dropped — v2 design rule "TTS reflects what's currently shown"), runs the
//! survivor through [`TtsEngine`], and pushes resulting PCM into the shared
//! [`AudioOutput`].
//!
//! The worker also subscribes to [`SettingsHandle`] updates so a voice or
//! speed change applies to the very next synthesis (no engine restart).

use std::sync::Arc;

use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use crate::audio::AudioOutput;
use crate::settings::{Settings, SettingsHandle};
use crate::state::StateSignal;
use crate::tts::engine::{SynthesisRequest, TtsEngine};
use crate::tts::{ActiveTab, TtsRequest};

pub fn spawn_tts_worker(
    mut engine: TtsEngine,
    audio: Arc<AudioOutput>,
    mut rx: mpsc::Receiver<TtsRequest>,
    state_signals: mpsc::Sender<StateSignal>,
    settings: SettingsHandle,
    active: ActiveTab,
) {
    tauri::async_runtime::spawn(async move {
        let mut next_id: u64 = 0;
        let mut settings_rx = settings.subscribe();
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
                    let Some(req) = seg else { break };

                    let (tab, text, is_notification) = match req {
                        TtsRequest::Synthesize { tab, text } => (tab, text, false),
                        TtsRequest::SynthesizeNotification { tab, text } => (tab, text, true),
                    };

                    if !is_notification {
                        // Background-tab gate: if the request's tab is no longer
                        // active by the time we pick it up, drop it. This is the
                        // single-shared-channel filter the v2 design specifies —
                        // simpler than per-tab queues and avoids retaining stale
                        // segments to discard later. Notifications skip this
                        // gate by design: they exist precisely to announce
                        // events on tabs the user isn't currently looking at.
                        let active_tab = *active.read().expect("active tab poisoned");
                        if tab != active_tab {
                            debug!(?tab, ?active_tab, "tts: dropping segment for inactive tab");
                            continue;
                        }
                    }

                    next_id += 1;
                    let synth_req = SynthesisRequest { text, request_id: next_id };
                    let started = std::time::Instant::now();
                    match engine.synthesize(synth_req) {
                        Ok(resp) => {
                            let elapsed_ms = started.elapsed().as_millis();
                            debug!(
                                request_id = resp.request_id,
                                samples = resp.samples.len(),
                                elapsed_ms,
                                kind = if is_notification { "notification" } else { "segment" },
                                "tts synthesis ok"
                            );
                            audio.enqueue(resp.samples, resp.sample_rate);
                        }
                        Err(e) => {
                            let _ = &state_signals; // future fatal-error path
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
