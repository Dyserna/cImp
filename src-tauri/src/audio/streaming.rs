//! Amplitude streaming task: pulls recent samples from the [`AmplitudeTap`]
//! at ~60 Hz and forwards them to the frontend visualizer via the
//! `audio-amplitude` Tauri event.
//!
//! When the audio sink is empty, the task skips the read+emit entirely so
//! the IPC stays quiet during silence. The 16 ms cadence is chosen to match
//! a 60 Hz display; the frontend renders off `requestAnimationFrame` and
//! reads the latest payload from a mutable ref, so a missed tick costs at
//! most one frame of staleness.

use std::sync::Arc;
use std::time::Duration;

use tauri::{AppHandle, Emitter};
use tokio::time::interval;
use tracing::warn;

use crate::audio::AudioOutput;

/// Window of samples sent per tick. 1024 samples at 24 kHz ≈ 42 ms — wider
/// than one frame so the frontend's scrolling buffer always has fresh data
/// even if a tick is late.
const SAMPLE_WINDOW: usize = 1024;

/// 16 ms ≈ 60 Hz. The frontend renders at the display refresh rate; matching
/// here keeps the buffer fresh without paying for needless IPC.
const TICK: Duration = Duration::from_millis(16);

pub fn spawn_amplitude_streamer(app: AppHandle, audio: Arc<AudioOutput>) {
    let tap = audio.amplitude_tap();
    tauri::async_runtime::spawn(async move {
        let mut tick = interval(TICK);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tick.tick().await;
            // Skip while silent OR paused: a paused sink keeps `is_playing()`
            // true but no new samples arrive, so emitting would replay the
            // frozen buffer tail and the avatar would lip-sync to stale audio.
            if !audio.is_playing() || audio.is_paused() {
                continue;
            }
            let samples = tap.recent_samples(SAMPLE_WINDOW);
            if samples.is_empty() {
                continue;
            }
            if let Err(e) = app.emit("audio-amplitude", samples) {
                warn!(error = %e, "emit audio-amplitude failed");
            }
        }
    });
}
