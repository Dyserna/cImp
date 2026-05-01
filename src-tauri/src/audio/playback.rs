//! Audio playback via cpal (output stream) + rodio (queue + resampling).
//!
//! `cpal::Stream` (and therefore `rodio::OutputStream`) is `!Send` on most
//! platforms — the underlying audio handle is bound to the thread that
//! created it. To keep [`AudioOutput`] usable from any tokio task we run a
//! dedicated `audio` OS thread that owns the stream + sink and processes
//! commands off a `std::sync::mpsc` channel. The public type is just the
//! command sender plus a shared amplitude ring.

use std::sync::mpsc::{self, Receiver, SyncSender};
use std::sync::{Arc, RwLock};
use std::time::Duration;

use rodio::{OutputStream, Sink, Source};
use tracing::{debug, info, warn};

use crate::audio::amplitude::{AmplitudeTap, RingBuffer, RING_CAPACITY};
use crate::error::{AppError, AppResult};

#[derive(Debug)]
enum AudioCommand {
    Enqueue { samples: Vec<f32>, sample_rate: u32 },
    StopAll,
    #[allow(dead_code)] // M6 settings hook.
    SetVolume(f32),
}

pub struct AudioOutput {
    cmd_tx: mpsc::Sender<AudioCommand>,
    /// Shared with the audio thread so the visualizer (M5) can read recent
    /// samples post-resampling without going through the audio output path.
    #[allow(dead_code)]
    amplitude: Arc<RwLock<RingBuffer>>,
}

impl AudioOutput {
    pub fn new() -> AppResult<Self> {
        let (cmd_tx, cmd_rx) = mpsc::channel::<AudioCommand>();
        let amplitude = Arc::new(RwLock::new(RingBuffer::new(RING_CAPACITY)));

        let (init_tx, init_rx) = mpsc::sync_channel::<AppResult<()>>(1);
        let amp_for_thread = amplitude.clone();

        std::thread::Builder::new()
            .name("cctts-audio".into())
            .spawn(move || run_audio_thread(cmd_rx, amp_for_thread, init_tx))
            .map_err(|e| AppError::Audio(format!("spawn audio thread: {e}")))?;

        match init_rx.recv() {
            Ok(Ok(())) => Ok(Self { cmd_tx, amplitude }),
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

    #[allow(dead_code)] // M5 visualizer hook
    pub fn amplitude_tap(&self) -> AmplitudeTap {
        AmplitudeTap::from_arc(self.amplitude.clone())
    }

    #[allow(dead_code)] // Interrupt-on-input (M6 / M7).
    pub fn stop_all(&self) {
        let _ = self.cmd_tx.send(AudioCommand::StopAll);
    }

    #[allow(dead_code)] // Settings (M6).
    pub fn set_volume(&self, volume: f32) {
        let _ = self.cmd_tx.send(AudioCommand::SetVolume(volume));
    }
}

fn run_audio_thread(
    cmd_rx: Receiver<AudioCommand>,
    amplitude: Arc<RwLock<RingBuffer>>,
    init_tx: SyncSender<AppResult<()>>,
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

    info!("audio thread ready");
    let _ = init_tx.send(Ok(()));

    while let Ok(cmd) = cmd_rx.recv() {
        match cmd {
            AudioCommand::Enqueue { samples, sample_rate } => {
                let source = TappedSource::new(samples, sample_rate, amplitude.clone());
                sink.append(source);
            }
            AudioCommand::StopAll => sink.clear(),
            AudioCommand::SetVolume(v) => sink.set_volume(v),
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
