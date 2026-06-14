//! Offline speech-to-text (V6-01). Mirrors the `tts/` module's shape: a
//! single-owner engine on a worker thread, a `.manage`d [`SttHandle`], and IPC
//! commands registered in `main.rs`. Unlike TTS this is an audio *input* path
//! — a `cpal` capture stream (see [`capture`]) feeds a bundled Whisper model
//! (see [`engine`]) and the transcript lands in the compose overlay.
//!
//! Threading: capture and transcription each run on their own dedicated OS
//! thread (the `cpal::Stream` is `!Send`; Whisper inference is multi-second
//! blocking work). The handle is just channel senders plus shared state cells,
//! so it is `Send + Sync` and lives in `AppState`.

mod capture;
mod engine;
mod worker;

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex, RwLock};

use serde::Serialize;
use tauri::{AppHandle, Emitter};
use tracing::warn;

use crate::audio::amplitude::{AmplitudeTap, RingBuffer};
use crate::error::AppResult;
use crate::settings::SettingsHandle;

/// Default bundled model filename. Selectable in Settings; users can drop
/// other `ggml-*.bin` files into `models/`.
pub const DEFAULT_STT_MODEL: &str = "ggml-small.bin";

/// Mic amplitude ring capacity. ~1 s at 48 kHz — enough for the recording
/// waveform without unbounded memory regardless of the device's native rate.
const MIC_RING_CAPACITY: usize = 48_000;

/// Lifecycle state broadcast to the frontend via the `stt-state` event.
/// Serializes lowercase to match the TS `SttState` union.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum SttState {
    Idle,
    Recording,
    Transcribing,
    Error,
}

/// Capture-thread command, sent by the IPC layer through [`SttHandle`].
pub(crate) enum CaptureCmd {
    Start,
    Stop,
    Cancel,
}

/// `.manage`d handle: just the command channel into the capture thread.
/// Recording state and transcripts flow back to the frontend via the
/// `stt-state` / `stt-transcription` events, not through this handle. The
/// `Mutex<Sender>` keeps it `Send + Sync` for `AppState`.
pub struct SttHandle {
    cmd_tx: Mutex<Sender<CaptureCmd>>,
}

/// The receiver/shared-cell half handed to [`spawn`] in the Tauri `setup`
/// hook (which is where the AppHandle the threads need becomes available).
pub struct SttRuntime {
    cmd_rx: mpsc::Receiver<CaptureCmd>,
    recording: Arc<AtomicBool>,
    state: Arc<RwLock<SttState>>,
    mic: Arc<RwLock<RingBuffer>>,
}

impl SttHandle {
    /// Build the handle and its paired runtime. The handle goes into
    /// `AppState`; the runtime is stashed until `setup` calls [`spawn`].
    pub fn new() -> (Self, SttRuntime) {
        let (cmd_tx, cmd_rx) = mpsc::channel();
        let recording = Arc::new(AtomicBool::new(false));
        let state = Arc::new(RwLock::new(SttState::Idle));
        let mic = Arc::new(RwLock::new(RingBuffer::new(MIC_RING_CAPACITY)));
        let handle = Self {
            cmd_tx: Mutex::new(cmd_tx),
        };
        let runtime = SttRuntime {
            cmd_rx,
            recording,
            state,
            mic,
        };
        (handle, runtime)
    }

    fn send(&self, cmd: CaptureCmd) {
        if let Ok(tx) = self.cmd_tx.lock() {
            if tx.send(cmd).is_err() {
                warn!(target: "stt", "capture thread gone; command dropped");
            }
        }
    }

    pub fn start(&self) {
        self.send(CaptureCmd::Start);
    }

    pub fn stop(&self) {
        self.send(CaptureCmd::Stop);
    }

    pub fn cancel(&self) {
        self.send(CaptureCmd::Cancel);
    }
}

/// Spawn the capture + transcription threads plus the mic-amplitude streamer.
/// Called once from the Tauri `setup` hook with the runtime half of
/// [`SttHandle::new`].
pub fn spawn(app: AppHandle, settings: SettingsHandle, runtime: SttRuntime) {
    let (jobs_tx, jobs_rx) = mpsc::channel::<Vec<f32>>();
    worker::spawn_stt_worker(app.clone(), settings.clone(), jobs_rx, runtime.state.clone());
    spawn_mic_streamer(app.clone(), runtime.recording.clone(), runtime.mic.clone());
    capture::spawn_capture_thread(
        app,
        settings,
        runtime.cmd_rx,
        jobs_tx,
        runtime.recording,
        runtime.state,
        runtime.mic,
    );
}

/// Mirror of `audio::spawn_amplitude_streamer` for the mic side: while
/// recording, push recent capture samples to the frontend `mic-amplitude`
/// event at ~60 Hz so the recording waveform animates. Idle (not recording)
/// → no read, no emit, keeping the IPC quiet.
fn spawn_mic_streamer(
    app: AppHandle,
    recording: Arc<AtomicBool>,
    mic: Arc<RwLock<RingBuffer>>,
) {
    use std::time::Duration;
    const SAMPLE_WINDOW: usize = 1024;
    let tap = AmplitudeTap::from_arc(mic);
    tauri::async_runtime::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_millis(16));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tick.tick().await;
            if !recording.load(Ordering::SeqCst) {
                continue;
            }
            let samples = tap.recent_samples(SAMPLE_WINDOW);
            if samples.is_empty() {
                continue;
            }
            if let Err(e) = app.emit("mic-amplitude", samples) {
                warn!(target: "stt", error = %e, "emit mic-amplitude failed");
            }
        }
    });
}

/// Store and broadcast a new lifecycle state. Shared by the capture and
/// worker threads (both hold an [`AppHandle`]).
pub(crate) fn set_state(app: &AppHandle, cell: &RwLock<SttState>, new: SttState) {
    if let Ok(mut s) = cell.write() {
        *s = new;
    }
    if let Err(e) = app.emit("stt-state", serde_json::json!({ "state": new })) {
        warn!(target: "stt", error = %e, "emit stt-state failed");
    }
}

/// STT models live in the same portable dir as Kokoro: `<exe-dir>/../models/`.
/// Enumerate the `ggml-*.bin` files there for the settings dropdown.
pub fn list_models() -> AppResult<Vec<String>> {
    let dir = crate::tts::model_dir()?;
    let mut out = Vec::new();
    if let Ok(rd) = std::fs::read_dir(&dir) {
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            if name.starts_with("ggml-") && name.ends_with(".bin") {
                out.push(name);
            }
        }
    }
    out.sort();
    Ok(out)
}

/// cpal input device names (for the settings device picker). The "System
/// default" sentinel is the empty-string `input_device` setting; the IPC
/// command prepends a label for it.
pub fn list_input_devices() -> AppResult<Vec<String>> {
    use cpal::traits::{DeviceTrait, HostTrait};
    let host = cpal::default_host();
    let mut out = Vec::new();
    if let Ok(devices) = host.input_devices() {
        for d in devices {
            if let Ok(name) = d.name() {
                out.push(name);
            }
        }
    }
    out.sort();
    out.dedup();
    Ok(out)
}

pub fn default_model_path(model_file: &str) -> AppResult<PathBuf> {
    let name = if model_file.is_empty() {
        DEFAULT_STT_MODEL
    } else {
        model_file
    };
    Ok(crate::tts::model_dir()?.join(name))
}

/// Mirror of `tts::report_missing_model_files` for the STT model. Logged when
/// the configured model is absent so the user knows what to drop in `models/`.
pub fn report_missing_model_files(model_file: &str) {
    let dir = crate::tts::model_dir()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| "<exe-dir>/../models".to_string());
    let name = if model_file.is_empty() {
        DEFAULT_STT_MODEL
    } else {
        model_file
    };
    warn!(target: "stt", "");
    warn!(target: "stt", "STT disabled: Whisper model file not found.");
    warn!(target: "stt", "Place this file under: {dir}");
    warn!(target: "stt", "  {name}");
    warn!(target: "stt", "Source (ggml Whisper models):");
    warn!(
        target: "stt",
        "  https://huggingface.co/ggerganov/whisper.cpp/resolve/main/{name}"
    );
    warn!(target: "stt", "");
}
