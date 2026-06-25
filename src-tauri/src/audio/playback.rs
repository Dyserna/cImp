//! Audio playback via cpal (output stream) + rodio (queue + resampling).
//!
//! `cpal::Stream` (and therefore `rodio::OutputStream`) is `!Send` on most
//! platforms — the underlying audio handle is bound to the thread that
//! created it. To keep [`AudioOutput`] usable from any tokio task we run a
//! dedicated `audio` OS thread that owns the stream + sink and processes
//! commands off a `std::sync::mpsc` channel. The public type is just the
//! command sender plus a shared amplitude ring.

use std::collections::VecDeque;
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

/// Identifies one sentence chunk of a Ctrl+right-click selection read. The
/// audio thread carries one per enqueued selection source so it can emit a
/// playback "begin" edge as that chunk reaches the front of the queue.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChunkMark {
    pub session: u64,
    pub index: u32,
}

#[derive(Debug)]
enum AudioCommand {
    Enqueue {
        samples: Vec<f32>,
        sample_rate: u32,
        /// `Some` for selection-read chunks (drives progress events), `None`
        /// for ordinary AI-output / notification audio.
        mark: Option<ChunkMark>,
    },
    StopAll,
    SetVolume(f32),
    /// Pause (`true`) or resume (`false`) the sink without discarding queued
    /// audio. Backs the selection-TTS pause/resume transport. While paused the
    /// sink keeps its sources, so `sink.len()`/`empty()` are unchanged and no
    /// playback or selection-progress edges fire until resumed.
    SetPaused(bool),
    /// The worker has enqueued every chunk of selection-read `session`
    /// (`count` total). The audio thread emits the `index == count` "done"
    /// sentinel only once this has arrived AND the session's audio has drained
    /// — so a momentary gap between chunks (synthesis lagging playback on a
    /// cold read) is not mistaken for end-of-read.
    SelectionFinished { session: u64, count: u32 },
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
            .name("ccimp-audio".into())
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
        self.enqueue_inner(samples, sample_rate, None);
    }

    /// Enqueue a selection-read chunk tagged with its [`ChunkMark`]. The audio
    /// thread emits a `TtsSelectionProgress` edge as this chunk reaches the
    /// front of the queue and again when it (and the whole read) drains.
    pub fn enqueue_marked(&self, samples: Vec<f32>, sample_rate: u32, mark: ChunkMark) {
        self.enqueue_inner(samples, sample_rate, Some(mark));
    }

    fn enqueue_inner(&self, samples: Vec<f32>, sample_rate: u32, mark: Option<ChunkMark>) {
        if samples.is_empty() {
            return;
        }
        if sample_rate == 0 {
            // A zero rate would break rodio's resampler and the duration math;
            // a synthesizer that reports it has produced no usable audio.
            warn!("audio: dropping chunk with zero sample rate");
            return;
        }
        if let Err(e) = self
            .cmd_tx
            .send(AudioCommand::Enqueue { samples, sample_rate, mark })
        {
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

    /// Pause or resume playback without discarding queued audio.
    pub fn set_paused(&self, paused: bool) {
        let _ = self.cmd_tx.send(AudioCommand::SetPaused(paused));
    }

    /// Signal that every chunk of a selection read has been enqueued. Lets the
    /// audio thread emit the read's "done" sentinel only after the audio truly
    /// drains (not on a between-chunk gap).
    pub fn selection_finished(&self, session: u64, count: u32) {
        let _ = self.cmd_tx.send(AudioCommand::SelectionFinished { session, count });
    }

}

/// Read the active-tab cell synchronously. Falls back to Claude on a
/// poisoned lock — that's the v2 default and we only consult this on
/// playback edges, so a benign default keeps the avatar pipeline alive.
fn current_active(active: &ActiveTab) -> TabId {
    active
        .read()
        .map(|g| g.clone())
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
    // The tab that started the current stretch of speech. Captured on the
    // idle→speaking edge and reused on the speaking→idle edge so a tab switch
    // mid-speech doesn't misattribute the stop (audio is no longer stopped on
    // tab switch — only Esc stops it).
    let mut speaking_tab: Option<TabId> = None;
    // Selection-read progress: a per-source mark queue that mirrors the
    // rodio sink's FIFO. `None` entries stand in for ordinary AI/notification
    // audio so the deque length always equals `sink.len()` — that invariant
    // is what lets us map a `sink.len()` drop to "the front source finished".
    let mut marks: VecDeque<Option<ChunkMark>> = VecDeque::new();
    let mut prev_len: usize = 0;
    // The selection chunk we last told the frontend is "now playing", so we
    // emit a begin edge only when the front actually changes.
    let mut last_front: Option<ChunkMark> = None;
    // Set when the worker has enqueued every chunk of a read. The "done"
    // sentinel fires only once this is set AND that session's marks have all
    // drained — never on a mere between-chunk gap.
    let mut pending_done: Option<(u64, u32)> = None;
    loop {
        let cmd = match cmd_rx.recv_timeout(PLAYBACK_POLL) {
            Ok(c) => Some(c),
            Err(RecvTimeoutError::Timeout) => None,
            Err(RecvTimeoutError::Disconnected) => break,
        };

        if let Some(cmd) = cmd {
            match cmd {
                AudioCommand::Enqueue { samples, sample_rate, mark } => {
                    let source = TappedSource::new(samples, sample_rate, amplitude.clone());
                    sink.append(source);
                    marks.push_back(mark);
                }
                AudioCommand::StopAll => {
                    // Reached only via Esc (`tts_stop`) now — tab switches no
                    // longer stop audio. rodio's `clear()` ALSO pauses the
                    // sink. Without an explicit `play()` after, the next
                    // `append()` lands in a paused queue — synthesized samples
                    // sit there forever and the user hears nothing (Esc during
                    // TTS, then a new read → silent).
                    sink.clear();
                    sink.play();
                    // Drop the mark queue too: a stop cancels any in-flight
                    // selection read, and the frontend clears its highlight on
                    // the same Esc that triggered the stop.
                    marks.clear();
                    last_front = None;
                    pending_done = None;
                    // The sink was just emptied; rebase the drain tracker so the
                    // next tick doesn't see a phantom drop (`len < prev_len`) and
                    // pop marks belonging to a freshly-enqueued selection.
                    prev_len = 0;
                }
                AudioCommand::SetVolume(v) => sink.set_volume(v),
                AudioCommand::SetPaused(paused) => {
                    if paused {
                        sink.pause();
                    } else {
                        sink.play();
                    }
                }
                AudioCommand::SelectionFinished { session, count } => {
                    pending_done = Some((session, count));
                }
            }
        }

        // Selection-read progress. Compare the sink's queue length to the
        // previous tick: any drop means that many front sources drained. We
        // pop the matching marks but do NOT treat a drop as end-of-read —
        // end-of-read is decided below by `pending_done` + an empty session.
        let len = sink.len();
        if len < prev_len {
            for _ in 0..(prev_len - len) {
                marks.pop_front();
            }
        }
        prev_len = len;

        // Tell the frontend which selection chunk is now at the front of the
        // queue (i.e. playing). Only on an actual change of front. A gap with
        // no front (audio drained between chunks) leaves `last_front` as-is, so
        // the highlight holds on the last sentence until the next one begins
        // rather than flickering off.
        let front = marks.front().copied().flatten();
        if let Some(f) = front {
            if last_front != Some(f) {
                last_front = Some(f);
                let tab = current_active(&active);
                let _ = state_signals.try_send(StateSignal::TtsSelectionProgress {
                    tab,
                    session: f.session,
                    index: f.index,
                });
            }
        }

        // End-of-read: once the worker has signalled it enqueued everything
        // (`pending_done`) and none of that session's chunks remain in the
        // queue, emit the `index == count` sentinel so the frontend clears the
        // highlight. This is robust to between-chunk gaps on a cold read.
        if let Some((session, count)) = pending_done {
            let session_remaining = marks
                .iter()
                .any(|m| matches!(m, Some(mk) if mk.session == session));
            if !session_remaining {
                let tab = current_active(&active);
                let _ = state_signals.try_send(StateSignal::TtsSelectionProgress {
                    tab,
                    session,
                    index: count,
                });
                pending_done = None;
                if last_front.map(|f| f.session) == Some(session) {
                    last_front = None;
                }
            }
        }

        let now_speaking = !sink.empty();
        // `playing` is authoritative and cheap — keep it exact every tick so a
        // `try_drain` sees the truth immediately, regardless of whether the
        // avatar edge below has been delivered yet. Notify idle waiters on the
        // true→false transition.
        let was_playing = playing.swap(now_speaking, Ordering::Relaxed);
        if was_playing && !now_speaking {
            idle_notify.notify_waiters();
        }

        // Deliver the avatar Started/Stopped edges WITHOUT blocking this loop —
        // this is the only thread servicing the command queue (Esc/StopAll,
        // volume, pause), so a blocking_send into a transiently-full
        // `state_signals` would freeze all audio control. We also must not
        // *drop* the Stopped edge (it would pin the avatar in Speaking), so we
        // retry on the next poll tick: `speaking` only advances once the edge
        // actually lands. The local state never sends an unbalanced edge, so a
        // Started that never delivered means no Stopped is owed.
        if now_speaking && !speaking {
            // Remember which tab owns this stretch of speech so the matching
            // stop edge is tagged with the SAME tab (tab switches mid-speech no
            // longer stop audio, so reading `current_active` at the stop edge
            // could mis-tag it).
            let tab = current_active(&active);
            match state_signals.try_send(StateSignal::TtsPlaybackStarted { tab: tab.clone() }) {
                Ok(()) => {
                    speaking = true;
                    speaking_tab = Some(tab);
                }
                // Consumer gone: advance anyway so we don't spin retrying.
                Err(tokio_mpsc::error::TrySendError::Closed(_)) => {
                    speaking = true;
                    speaking_tab = Some(tab);
                }
                // Full: leave `speaking` false and retry on the next tick.
                Err(tokio_mpsc::error::TrySendError::Full(_)) => {}
            }
        } else if !now_speaking && speaking {
            let tab = speaking_tab.clone().unwrap_or_else(|| current_active(&active));
            match state_signals.try_send(StateSignal::TtsPlaybackStopped { tab }) {
                Ok(()) => {
                    speaking = false;
                    speaking_tab = None;
                }
                Err(tokio_mpsc::error::TrySendError::Closed(_)) => {
                    speaking = false;
                    speaking_tab = None;
                }
                Err(tokio_mpsc::error::TrySendError::Full(_)) => {}
            }
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
        if self.sample_rate == 0 {
            // Unknown duration rather than a divide-by-zero: `secs` would be
            // infinite and `Duration::from_secs_f64` panics on a non-finite.
            return None;
        }
        let secs = self.remaining as f64 / self.sample_rate as f64;
        Some(Duration::from_secs_f64(secs))
    }
}
