//! Audio playback via cpal (output stream) + rodio (queue + resampling).
//!
//! `cpal::Stream` (and therefore `rodio::OutputStream`) is `!Send` on most
//! platforms — the underlying audio handle is bound to the thread that
//! created it. To keep [`AudioOutput`] usable from any tokio task we run a
//! dedicated `audio` OS thread that owns the stream + sink and processes
//! commands off a `std::sync::mpsc` channel. The public type is just the
//! command sender plus a shared amplitude ring.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender};
use std::sync::{Arc, RwLock};
use std::time::Duration;

use rodio::{OutputStream, Sink, Source};
use tokio::sync::{mpsc as tokio_mpsc, Notify};
use tracing::{debug, info, warn};

use crate::audio::amplitude::{AmplitudeTap, RingBuffer, RING_CAPACITY};
use crate::error::{AppError, AppResult};
use crate::settings::SettingsHandle;
use crate::state::{StateSignal, TabId};
use crate::tts::ActiveTab;

/// How often the audio thread polls for sink-empty transitions while
/// playback is in flight. 50 ms is well under perceptual latency for the
/// avatar transition and cheap.
const PLAYBACK_POLL: Duration = Duration::from_millis(50);

#[derive(Debug)]
enum AudioCommand {
    Enqueue { samples: Vec<f32>, sample_rate: u32 },
    StopAll,
    SetVolume(f32),
}

pub struct AudioOutput {
    cmd_tx: mpsc::Sender<AudioCommand>,
    /// Shared with the audio thread so the visualizer (M5) can read recent
    /// samples post-resampling without going through the audio output path.
    amplitude: Arc<RwLock<RingBuffer>>,
    /// Mirrors the audio thread's `speaking` edge so the M5 amplitude
    /// streamer can skip IPC when the sink is empty without blocking on
    /// the audio thread itself.
    playing: Arc<AtomicBool>,
    /// Fired by the audio thread on every speaking → idle edge. The
    /// notification manager (V2-04) waits on this so it can drain queued
    /// announcements right when current TTS finishes.
    idle_notify: Arc<Notify>,
}

impl AudioOutput {
    pub fn new(
        state_signals: tokio_mpsc::Sender<StateSignal>,
        settings: SettingsHandle,
        active: ActiveTab,
    ) -> AppResult<Self> {
        let (cmd_tx, cmd_rx) = mpsc::channel::<AudioCommand>();
        let amplitude = Arc::new(RwLock::new(RingBuffer::new(RING_CAPACITY)));
        let playing = Arc::new(AtomicBool::new(false));
        let idle_notify = Arc::new(Notify::new());

        let (init_tx, init_rx) = mpsc::sync_channel::<AppResult<()>>(1);
        let amp_for_thread = amplitude.clone();
        let playing_for_thread = playing.clone();
        let idle_notify_for_thread = idle_notify.clone();
        let initial_volume = effective_volume(&settings.current().tts);

        std::thread::Builder::new()
            .name("cctts-audio".into())
            .spawn(move || {
                run_audio_thread(
                    cmd_rx,
                    amp_for_thread,
                    playing_for_thread,
                    idle_notify_for_thread,
                    init_tx,
                    state_signals,
                    initial_volume,
                    active,
                )
            })
            .map_err(|e| AppError::Audio(format!("spawn audio thread: {e}")))?;

        match init_rx.recv() {
            Ok(Ok(())) => {
                spawn_volume_subscriber(cmd_tx.clone(), settings);
                Ok(Self { cmd_tx, amplitude, playing, idle_notify })
            }
            Ok(Err(e)) => Err(e),
            Err(_) => Err(AppError::Audio("audio thread died during init".into())),
        }
    }

    pub fn enqueue(&self, samples: Vec<f32>, sample_rate: u32) {
        if samples.is_empty() {
            return;
        }
        if let Err(e) = self.cmd_tx.send(AudioCommand::Enqueue { samples, sample_rate }) {
            warn!(error = %e, "audio command channel closed; dropping samples");
        }
    }

    pub fn amplitude_tap(&self) -> AmplitudeTap {
        AmplitudeTap::from_arc(self.amplitude.clone())
    }

    /// True while the audio thread has audio queued in the sink. Mirrored
    /// off the same edge that fires TtsPlaybackStarted/Stopped, so the
    /// streamer's "skip when silent" check stays in sync with what the
    /// state machine sees.
    pub fn is_playing(&self) -> bool {
        self.playing.load(Ordering::Relaxed)
    }

    /// Wake-up primitive fired on every speaking → idle edge. Subscribers
    /// `await` `notify.notified()`; one notify per edge. `is_playing()`
    /// answers "are we idle right now"; this answers "tell me when we
    /// next become idle." The combination lets the notification manager
    /// re-check at the right moment without polling.
    pub fn idle_notify(&self) -> Arc<Notify> {
        self.idle_notify.clone()
    }

    pub fn stop_all(&self) {
        let _ = self.cmd_tx.send(AudioCommand::StopAll);
    }

    #[allow(dead_code)] // Direct setter; settings broadcast is the usual path.
    pub fn set_volume(&self, volume: f32) {
        let _ = self.cmd_tx.send(AudioCommand::SetVolume(volume));
    }
}

/// Read the active-tab cell synchronously. Falls back to Claude on a
/// poisoned lock — that's the v2 default and we only consult this on
/// playback edges, so a benign default keeps the avatar pipeline alive.
fn current_active(active: &ActiveTab) -> TabId {
    active
        .read()
        .map(|g| *g)
        .unwrap_or(TabId::Claude)
}

/// Mute folds into volume: muted means volume = 0, unmuted means the
/// configured volume. The audio thread doesn't need to know about mute as a
/// separate concept.
fn effective_volume(tts: &crate::settings::TtsSettings) -> f32 {
    if tts.mute {
        0.0
    } else {
        tts.volume.clamp(0.0, 1.0)
    }
}

/// Subscribe to settings updates and forward volume/mute changes to the
/// audio thread. Lives for the process lifetime — when the broadcast
/// channel closes the loop ends naturally.
fn spawn_volume_subscriber(cmd_tx: mpsc::Sender<AudioCommand>, settings: SettingsHandle) {
    tauri::async_runtime::spawn(async move {
        let mut rx = settings.subscribe();
        let mut last = effective_volume(&settings.current().tts);
        loop {
            match rx.recv().await {
                Ok(s) => {
                    let v = effective_volume(&s.tts);
                    if (v - last).abs() > f32::EPSILON {
                        last = v;
                        let _ = cmd_tx.send(AudioCommand::SetVolume(v));
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            }
        }
    });
}

fn run_audio_thread(
    cmd_rx: Receiver<AudioCommand>,
    amplitude: Arc<RwLock<RingBuffer>>,
    playing: Arc<AtomicBool>,
    idle_notify: Arc<Notify>,
    init_tx: SyncSender<AppResult<()>>,
    state_signals: tokio_mpsc::Sender<StateSignal>,
    initial_volume: f32,
    active: ActiveTab,
) {
    // Open the device on this thread so the cpal::Stream stays bound here.
    let (stream, handle) = match OutputStream::try_default() {
        Ok(pair) => pair,
        Err(e) => {
            let _ = init_tx.send(Err(AppError::Audio(format!("default output stream: {e}"))));
            return;
        }
    };
    let sink = match Sink::try_new(&handle) {
        Ok(s) => s,
        Err(e) => {
            let _ = init_tx.send(Err(AppError::Audio(format!("sink: {e}"))));
            return;
        }
    };
    sink.set_volume(initial_volume);

    info!("audio thread ready");
    let _ = init_tx.send(Ok(()));

    // Track sink-empty edges so we emit TtsPlaybackStarted/Stopped exactly
    // once per stretch of audio. We poll via recv_timeout so playback edges
    // can be detected even when no command arrives.
    let mut speaking = false;
    loop {
        let cmd = match cmd_rx.recv_timeout(PLAYBACK_POLL) {
            Ok(c) => Some(c),
            Err(RecvTimeoutError::Timeout) => None,
            Err(RecvTimeoutError::Disconnected) => break,
        };

        if let Some(cmd) = cmd {
            match cmd {
                AudioCommand::Enqueue { samples, sample_rate } => {
                    let source = TappedSource::new(samples, sample_rate, amplitude.clone());
                    sink.append(source);
                }
                AudioCommand::StopAll => {
                    // rodio's `clear()` ALSO pauses the sink. Without an
                    // explicit `play()` after, the next `append()` lands in a
                    // paused queue — synthesized samples sit there forever
                    // and the user hears nothing. v2's tab-switch path makes
                    // this trivially reproducible (switch tab during/after
                    // TTS, switch back, ask for more TTS → silent).
                    sink.clear();
                    sink.play();
                }
                AudioCommand::SetVolume(v) => sink.set_volume(v),
            }
        }

        let now_speaking = !sink.empty();
        if now_speaking && !speaking {
            speaking = true;
            playing.store(true, Ordering::Relaxed);
            let tab = current_active(&active);
            let _ = state_signals.try_send(StateSignal::TtsPlaybackStarted { tab });
        } else if !now_speaking && speaking {
            speaking = false;
            playing.store(false, Ordering::Relaxed);
            let tab = current_active(&active);
            let _ = state_signals.try_send(StateSignal::TtsPlaybackStopped { tab });
            // Wake any task waiting on the next idle edge. `playing` is
            // already false above, so a `try_drain` running on this notify
            // will see `is_playing() == false` and proceed.
            idle_notify.notify_waiters();
        }
    }
    debug!("audio thread exiting");
    drop(sink);
    drop(stream);
}

/// Source that streams f32 mono samples to rodio while mirroring each one
/// into the amplitude ring buffer. Brief lock per sample; if it ever becomes
/// a contention point, swap for a lock-free ring.
struct TappedSource {
    samples: std::vec::IntoIter<f32>,
    sample_rate: u32,
    remaining: usize,
    amplitude: Arc<RwLock<RingBuffer>>,
}

impl TappedSource {
    fn new(samples: Vec<f32>, sample_rate: u32, amplitude: Arc<RwLock<RingBuffer>>) -> Self {
        let remaining = samples.len();
        Self {
            samples: samples.into_iter(),
            sample_rate,
            remaining,
            amplitude,
        }
    }
}

impl Iterator for TappedSource {
    type Item = f32;

    fn next(&mut self) -> Option<f32> {
        let s = self.samples.next()?;
        self.remaining = self.remaining.saturating_sub(1);
        if let Ok(mut ring) = self.amplitude.write() {
            ring.push(s);
        }
        Some(s)
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        (self.remaining, Some(self.remaining))
    }
}

impl Source for TappedSource {
    fn current_frame_len(&self) -> Option<usize> {
        Some(self.remaining)
    }

    fn channels(&self) -> u16 {
        1
    }

    fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    fn total_duration(&self) -> Option<Duration> {
        let secs = self.remaining as f64 / self.sample_rate as f64;
        Some(Duration::from_secs_f64(secs))
    }
}
