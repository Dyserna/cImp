//! TTS worker task. Receives [`TtsRequest`]s from the per-tab processing
//! layers, filters by the shared active-tab cell (background-tab synthesis is
//! dropped — v2 design rule "TTS reflects what's currently shown"), runs the
//! survivor through [`TtsEngine`], and pushes resulting PCM into the shared
//! [`AudioOutput`].
//!
//! The worker also subscribes to [`SettingsHandle`] updates so a voice or
//! speed change applies to the very next synthesis (no engine restart), and so
//! toggling `tts.enabled` **loads or unloads** the Kokoro model live: the
//! engine is held as an `Option`, built lazily when the feature turns on and
//! dropped (freeing the ONNX session) when it turns off.

use std::collections::HashMap;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use crate::audio::{AudioOutput, ChunkMark};
use crate::error::AppError;
use crate::settings::{Settings, SettingsHandle};
use crate::state::{StateSignal, TabId};
use crate::tts::engine::{SynthesisRequest, TtsEngine};
use crate::tts::{ActiveTab, AiTtsSuppressed, SpeakSession, TtsRequest};

/// How long an identical synthesis failure stays muted after being logged in
/// full (#145).
///
/// A broken phonemizer fails EVERY segment with the same string, so the
/// unthrottled WARN buried the log under hundreds of identical lines while
/// adding nothing after the first. Five minutes because the line's job after
/// the first is to say *"this is still happening"*, not to count segments.
const SYNTH_FAILURE_MUTE: Duration = Duration::from_secs(300);

/// Cap on distinct failure strings the throttle remembers.
///
/// The key is an error message and messages can carry per-segment text, so the
/// map is a growth channel if left alone. When it fills, entries older than the
/// mute window go — which is exactly the set that can no longer suppress
/// anything, so pruning them changes no decision.
const SYNTH_FAILURE_KEYS: usize = 64;

/// The per-message clock behind [`admit_synthesis_failure`], allocated on
/// first use so a build that never fails a synthesis pays nothing.
static SYNTH_FAILURE_SEEN: Mutex<Option<HashMap<String, Instant>>> = Mutex::new(None);

/// Whether this failure message may be logged in full now, stamping it when it
/// may (#145).
///
/// **The first occurrence of a message always passes**, so a new failure is
/// never delayed behind an old one, and a condition that persists still leaves
/// a trail every [`SYNTH_FAILURE_MUTE`] rather than going silent. `now` is a
/// parameter so the window is testable without sleeping through it.
///
/// A poisoned lock admits: a log line is worth less than the panic that
/// unwrapping here would turn a logging decision into.
fn admit_synthesis_failure(message: &str, now: Instant) -> bool {
    let Ok(mut slot) = SYNTH_FAILURE_SEEN.lock() else {
        return true;
    };
    let seen = slot.get_or_insert_with(HashMap::new);
    if seen
        .get(message)
        .is_some_and(|last| now.duration_since(*last) < SYNTH_FAILURE_MUTE)
    {
        return false;
    }
    if seen.len() >= SYNTH_FAILURE_KEYS {
        seen.retain(|_, last| now.duration_since(*last) < SYNTH_FAILURE_MUTE);
    }
    seen.insert(message.to_string(), now);
    true
}

pub fn spawn_tts_worker(
    audio: Arc<AudioOutput>,
    mut rx: mpsc::Receiver<TtsRequest>,
    state_signals: mpsc::Sender<StateSignal>,
    settings: SettingsHandle,
    active: ActiveTab,
    speak_session: SpeakSession,
    ai_tts_suppressed: AiTtsSuppressed,
) {
    tauri::async_runtime::spawn(async move {
        let mut next_id: u64 = 0;
        let mut settings_rx = settings.subscribe();
        // Engine is loaded iff `tts.enabled`. Start from "off" and let
        // `apply_settings` drive the initial load (if enabled) so the
        // load/unload edge logic lives in exactly one place.
        let mut engine: Option<TtsEngine> = None;
        let mut tts_enabled = false;
        // Track the device the loaded engine was built on so a live change to
        // `tts.device` (GPU ↔ CPU) triggers a reload. Seeded from the current
        // setting so the initial `apply_settings` load doesn't self-trigger a
        // redundant reload.
        let mut tts_device = settings.current().tts.device;
        {
            let cur = settings.current();
            apply_settings(
                &mut engine,
                &mut tts_enabled,
                &mut tts_device,
                &cur,
                &state_signals,
                &active,
            )
            .await;
        }

        loop {
            tokio::select! {
                biased;
                changed = settings_rx.recv() => {
                    match changed {
                        Ok(s) => {
                            apply_settings(
                                &mut engine,
                                &mut tts_enabled,
                                &mut tts_device,
                                &s,
                                &state_signals,
                                &active,
                            )
                            .await
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                    }
                }
                seg = rx.recv() => {
                    let Some(req) = seg else { break };

                    // Ctrl+right-click read-along: a pre-segmented selection
                    // synthesized chunk-by-chunk and enqueued in order, each
                    // tagged so the audio thread can drive the highlight.
                    // Re-checks the shared session id around every (possibly
                    // long) synthesis so an Esc/`tts_stop` — which zeroes the
                    // cell — or a superseding read abandons the rest without
                    // playing it.
                    if let TtsRequest::SpeakSelection { tab, session, chunks } = req {
                        let count = chunks.len() as u32;
                        // TTS off / model unavailable: nothing to speak, but the
                        // frontend is showing a read-along highlight — emit the
                        // done sentinel so it clears instead of hanging.
                        if engine.is_none() {
                            debug!(session, "selection read: TTS disabled; clearing highlight");
                            let _ = state_signals.try_send(StateSignal::TtsSelectionProgress {
                                tab,
                                session,
                                index: count,
                            });
                            continue;
                        }
                        debug!(session, count, "selection read: received chunks");
                        let mut enqueued_any = false;
                        let mut cancelled = false;
                        for (i, text) in chunks.into_iter().enumerate() {
                            if speak_session.load(Ordering::SeqCst) != session {
                                debug!(session, "selection read cancelled before chunk {i}");
                                cancelled = true;
                                break;
                            }
                            // Preview + length captured before `text` moves into
                            // the request, so the per-chunk log can show which
                            // chunk produced how much audio (parity with the
                            // AI-output path, which logs every synthesis).
                            let nchars = text.chars().count();
                            let preview: String = text.chars().take(60).collect();
                            next_id += 1;
                            let synth_req = SynthesisRequest { text, request_id: next_id };
                            // Synthesis is CPU/GPU-bound ONNX work (hundreds of
                            // ms). Run it on the blocking pool, not this tokio
                            // worker, so IPC/audio/amplitude tasks aren't starved.
                            // Move the engine in and back out (it's used across
                            // loop iterations). The engine stays `Some` for the
                            // whole selection read: settings (and thus an unload)
                            // are only processed by the outer `select!`, which
                            // can't run while this `await` is in flight.
                            let mut eng = engine
                                .take()
                                .expect("engine present (checked at selection start)");
                            let (result, eng) = tokio::task::spawn_blocking(move || {
                                let r = eng.synthesize(synth_req);
                                (r, eng)
                            })
                            .await
                            .expect("tts selection synth task panicked");
                            engine = Some(eng);
                            match result {
                                Ok(resp) => {
                                    // A cancel may have arrived during synthesis;
                                    // skip the enqueue so no extra audio plays
                                    // after Esc.
                                    if speak_session.load(Ordering::SeqCst) != session {
                                        cancelled = true;
                                        break;
                                    }
                                    let samples = resp.samples.len();
                                    if !resp.samples.is_empty() {
                                        debug!(
                                            session, chunk = i, nchars, samples,
                                            "selection chunk synthesized; enqueuing"
                                        );
                                        audio.enqueue_marked(
                                            resp.samples,
                                            resp.sample_rate,
                                            ChunkMark { session, index: i as u32 },
                                        );
                                        enqueued_any = true;
                                    } else {
                                        // No audio for this chunk: it is silently
                                        // skipped and the highlight recedes over it
                                        // as if read. Warn (not debug) so a dropped
                                        // chunk is visible at default log levels.
                                        warn!(
                                            session, chunk = i, nchars, preview = %preview,
                                            "selection chunk produced no audio; skipping (text dropped)"
                                        );
                                    }
                                }
                                Err(e) => {
                                    warn!(error = %e, chunk = i, preview = %preview,
                                        "selection chunk synthesis failed; skipping (text dropped)");
                                }
                            }
                        }
                        if cancelled {
                            // Esc / supersede already cleared the frontend session
                            // and the sink; nothing more to do.
                        } else if enqueued_any {
                            // All chunks are queued. Tell the audio thread so it
                            // can emit the "done" sentinel once they've actually
                            // drained — NOT merely when the queue momentarily
                            // empties between chunks (which on a cold first read,
                            // where synthesis lags playback, would otherwise end
                            // the read after the first sentence).
                            audio.selection_finished(session, count);
                        } else {
                            // Nothing enqueued (all chunks empty/failed): the audio
                            // thread will never see this session, so emit the done
                            // sentinel directly so the frontend highlight clears.
                            let _ = state_signals.try_send(StateSignal::TtsSelectionProgress {
                                tab,
                                session,
                                index: count,
                            });
                        }
                        continue;
                    }

                    let (tab, text, is_notification, suppressible) = match req {
                        TtsRequest::Synthesize { tab, text, suppressible } => {
                            (tab, text, false, suppressible)
                        }
                        TtsRequest::SynthesizeNotification { tab, text } => (tab, text, true, false),
                        TtsRequest::SpeakSelection { .. } => unreachable!("handled above"),
                    };

                    // Esc-driven suppression: drop the rest of the current AI
                    // output burst's tagged segments until new output clears the
                    // flag. Only applies to suppressible (AI-tag) segments —
                    // notifications and command-initiated speech are never gated.
                    if suppressible && ai_tts_suppressed.load(Ordering::SeqCst) {
                        debug!(?tab, "tts: dropping AI segment (suppressed until new output)");
                        continue;
                    }

                    if !is_notification {
                        // Background-tab gate: if the request's tab is no longer
                        // active by the time we pick it up, drop it. This is the
                        // single-shared-channel filter the v2 design specifies —
                        // simpler than per-tab queues and avoids retaining stale
                        // segments to discard later. Notifications skip this
                        // gate by design: they exist precisely to announce
                        // events on tabs the user isn't currently looking at.
                        //
                        // The user-facing `behavior.speak_background_tabs`
                        // toggle opts out of this gate for tagged-content
                        // TTS (announcements remain governed by their own
                        // `announce_focused_tab` rule and never hit this
                        // gate at all).
                        // Benign fallback on a poisoned lock rather than
                        // `.expect()`: a panic here would permanently kill the
                        // TTS worker for the rest of the session. V40 Phase I:
                        // the fallback is `TabId::first_harness_default()` — the
                        // registry's guess — not the literal `TabId::from_str("claude")` the
                        // enum used to offer, which is the same "when in doubt,
                        // Claude" locked decision 2 removed everywhere else. It
                        // matches how the audio thread (`current_active`) and
                        // the notification manager handle the same poisoned
                        // lock.
                        let active_tab = active
                            .read()
                            .map(|g| g.clone())
                            .unwrap_or_else(|_| crate::state::TabId::first_harness_default());
                        let speak_background = settings.current().behavior.speak_background_tabs;
                        if tab != active_tab && !speak_background {
                            debug!(?tab, ?active_tab, "tts: dropping segment for inactive tab");
                            continue;
                        }
                    }

                    // TTS off (or model failed to load): drop the segment. The
                    // model is unloaded, so there is nothing to synthesize with.
                    let Some(mut eng) = engine.take() else {
                        debug!(?tab, "tts: feature disabled; dropping segment");
                        continue;
                    };
                    next_id += 1;
                    let synth_req = SynthesisRequest { text, request_id: next_id };
                    let started = std::time::Instant::now();
                    // Off the async runtime: blocking ONNX synthesis would
                    // otherwise stall IPC/audio. Engine moves in and back out.
                    let (result, eng) = tokio::task::spawn_blocking(move || {
                        let r = eng.synthesize(synth_req);
                        (r, eng)
                    })
                    .await
                    .expect("tts synth task panicked");
                    engine = Some(eng);
                    match result {
                        Ok(resp) => {
                            // Re-check suppression AFTER synthesis: an Esc may
                            // have arrived during the (few-hundred-ms) synth of
                            // this segment, which the pre-synthesis check above
                            // couldn't see. Without this, the segment that was
                            // mid-synthesis when Esc fired would still play —
                            // e.g. the second sentence of a multi-sentence tag.
                            if suppressible && ai_tts_suppressed.load(Ordering::SeqCst) {
                                debug!(?tab, "tts: dropping AI segment synthesized during suppress");
                                continue;
                            }
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
                            let message = e.to_string();
                            if admit_synthesis_failure(&message, Instant::now()) {
                                warn!(error = %message, "tts synthesis failed; skipping segment");
                            } else {
                                debug!(
                                    error = %message,
                                    "tts synthesis failed; skipping segment (repeat muted)"
                                );
                            }
                        }
                    }
                }
            }
        }
        debug!("tts worker: segment or settings channel closed; exiting");
    });
}

/// Reconcile the in-memory engine with the current settings:
/// - `tts.enabled` false→true loads the Kokoro model; true→false drops it
///   (unloading the ONNX session). `was_enabled` tracks the prior state so the
///   load/unload happens only on the edge.
/// - `tts.device` GPU↔CPU while enabled reloads the model on the newly-selected
///   device (dropping the old session first, freeing that device's memory).
///   `cur_device` tracks the loaded engine's device so the reload fires only on
///   the edge.
/// - while enabled, `speed`/`voice` changes apply to the next synthesis with no
///   engine restart.
async fn apply_settings(
    engine: &mut Option<TtsEngine>,
    was_enabled: &mut bool,
    cur_device: &mut crate::settings::ProcessingDevice,
    s: &Settings,
    state_signals: &mpsc::Sender<StateSignal>,
    active: &ActiveTab,
) {
    let now_enabled = s.tts.enabled;
    let device = s.tts.device;
    if now_enabled && !*was_enabled {
        if engine.is_none() {
            *engine = load_engine(s, state_signals, active).await;
        }
    } else if !now_enabled && *was_enabled {
        if engine.take().is_some() {
            info!("tts: disabled; Kokoro model unloaded");
        }
    } else if now_enabled && device != *cur_device && engine.is_some() {
        // Device changed while enabled: drop the old session before building
        // the new one so the old device's memory is freed first, then reload.
        info!(?device, "tts: device changed; reloading Kokoro model");
        *engine = None;
        *engine = load_engine(s, state_signals, active).await;
    }
    *was_enabled = now_enabled;
    *cur_device = device;

    if let Some(engine) = engine.as_mut() {
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
}

/// Build a [`TtsEngine`] for the current voice/speed. Returns `None` (and logs)
/// if the model dir / voice can't be resolved or the ONNX session fails to
/// build — mirroring the old startup behavior (missing model → quiet TTS).
///
/// The ONNX session build (graph load + Level3 optimization + execution-provider
/// / GPU init) is heavy blocking work — seconds on a GPU EP — so it runs on the
/// blocking pool, off the async runtime, exactly like synthesis does. Otherwise
/// an enable-toggle would park a runtime worker thread for the whole load.
async fn load_engine(
    s: &Settings,
    state_signals: &mpsc::Sender<StateSignal>,
    active: &ActiveTab,
) -> Option<TtsEngine> {
    let model_path = match crate::tts::default_model_path() {
        Ok(p) => p,
        Err(e) => {
            warn!(error = %e, "tts: cannot resolve model dir; TTS disabled");
            return None;
        }
    };
    let voice_path = match crate::tts::default_voice_path(&s.tts.voice) {
        Ok(p) => p,
        Err(e) => {
            warn!(error = %e, voice = %s.tts.voice, "tts: cannot resolve voice path; TTS disabled");
            return None;
        }
    };
    let speed = s.tts.speed;
    let device = s.tts.device;
    let built =
        tokio::task::spawn_blocking(move || TtsEngine::new(&model_path, &voice_path, device)).await;
    let result = match built {
        Ok(r) => r,
        Err(e) => {
            warn!(error = %e, "tts engine load task panicked; TTS disabled");
            return None;
        }
    };
    match result {
        Ok(mut engine) => {
            engine.set_speed(speed);
            info!(voice = %s.tts.voice, "tts: Kokoro model loaded");
            Some(engine)
        }
        Err(AppError::ModelNotFound(_)) => {
            crate::tts::report_missing_model_files();
            None
        }
        Err(e) => {
            warn!(error = %e, "tts engine init failed; TTS disabled");
            // Same poisoned-lock fallback as above: the registry's first
            // built-in tab, so the error is reported against a tab this build
            // actually ships.
            let tab = active
                .read()
                .map(|g| g.clone())
                .unwrap_or_else(|_| TabId::first_harness_default());
            let _ = state_signals.try_send(StateSignal::TtsError { tab });
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// #145: the same failure is logged once and then muted for the window; a
    /// different failure is never held behind it.
    ///
    /// Keys are unique to this test because the throttle's map is
    /// process-global (it is a logging decision, not state anything reads).
    #[test]
    fn an_identical_synthesis_failure_is_logged_once_per_window() {
        let stuck = "espeak: no data directory (test-a)";
        let other = "espeak: no data directory (test-b)";
        let t0 = Instant::now();

        assert!(admit_synthesis_failure(stuck, t0), "the first is always full");
        assert!(!admit_synthesis_failure(stuck, t0), "a repeat is muted");
        assert!(
            !admit_synthesis_failure(stuck, t0 + SYNTH_FAILURE_MUTE - Duration::from_secs(1)),
            "still inside the window"
        );
        assert!(
            admit_synthesis_failure(other, t0),
            "a different failure must not wait behind an unrelated one"
        );
        assert!(
            admit_synthesis_failure(stuck, t0 + SYNTH_FAILURE_MUTE),
            "a persisting condition has to say so again once the window passes"
        );
    }
}
