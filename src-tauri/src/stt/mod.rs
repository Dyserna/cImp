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

/// Message to the transcription worker. Finished recordings (`Transcribe`)
/// arrive from the capture thread; `Preload`/`Unload` arrive from the IPC layer
/// when `stt.enabled` toggles, so the Whisper model is loaded on enable and
/// dropped (freeing CPU/GPU memory) on disable. All three ride one channel so
/// the worker can stay a simple single-`recv` loop.
pub(crate) enum WorkerMsg {
    Transcribe(Vec<f32>),
    Preload,
    Unload,
}

/// `.manage`d handle: the command channel into the capture thread plus a
/// control channel into the transcription worker (for model load/unload).
/// Recording state and transcripts flow back to the frontend via the
/// `stt-state` / `stt-transcription` events, not through this handle. The
/// `Mutex<Sender>` keeps it `Send + Sync` for `AppState`.
pub struct SttHandle {
    cmd_tx: Mutex<Sender<CaptureCmd>>,
    worker_tx: Mutex<Sender<WorkerMsg>>,
}

/// The receiver/shared-cell half handed to [`spawn`] in the Tauri `setup`
/// hook (which is where the AppHandle the threads need becomes available).
pub struct SttRuntime {
    cmd_rx: mpsc::Receiver<CaptureCmd>,
    /// Sender the capture thread uses to hand finished recordings to the worker.
    jobs_tx: Sender<WorkerMsg>,
    /// The worker's receiving end (jobs + preload/unload control).
    jobs_rx: mpsc::Receiver<WorkerMsg>,
    recording: Arc<AtomicBool>,
    state: Arc<RwLock<SttState>>,
    mic: Arc<RwLock<RingBuffer>>,
}

impl SttHandle {
    /// Build the handle and its paired runtime. The handle goes into
    /// `AppState`; the runtime is stashed until `setup` calls [`spawn`].
    pub fn new() -> (Self, SttRuntime) {
        let (cmd_tx, cmd_rx) = mpsc::channel();
        // One channel feeds the worker both transcription jobs (from capture)
        // and load/unload control (from the IPC layer); the handle keeps a
        // clone of the sender for the latter.
        let (jobs_tx, jobs_rx) = mpsc::channel::<WorkerMsg>();
        let worker_tx = jobs_tx.clone();
        let recording = Arc::new(AtomicBool::new(false));
        let state = Arc::new(RwLock::new(SttState::Idle));
        let mic = Arc::new(RwLock::new(RingBuffer::new(MIC_RING_CAPACITY)));
        let handle = Self {
            cmd_tx: Mutex::new(cmd_tx),
            worker_tx: Mutex::new(worker_tx),
        };
        let runtime = SttRuntime {
            cmd_rx,
            jobs_tx,
            jobs_rx,
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

    fn send_worker(&self, msg: WorkerMsg) {
        if let Ok(tx) = self.worker_tx.lock() {
            if tx.send(msg).is_err() {
                warn!(target: "stt", "transcription worker gone; control message dropped");
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

    /// Eagerly load the Whisper model (on the worker thread, off the UI). Sent
    /// when `stt.enabled` flips on so the first dictation isn't slowed by a
    /// cold model load.
    pub fn preload(&self) {
        self.send_worker(WorkerMsg::Preload);
    }

    /// Drop the loaded Whisper model, freeing its memory. Sent when
    /// `stt.enabled` flips off.
    pub fn unload(&self) {
        self.send_worker(WorkerMsg::Unload);
    }
}

/// Spawn the capture + transcription threads plus the mic-amplitude streamer.
/// Called once from the Tauri `setup` hook with the runtime half of
/// [`SttHandle::new`].
pub fn spawn(app: AppHandle, settings: SettingsHandle, runtime: SttRuntime) {
    // NOTE: the whisper/ggml logging-hook install lives on the worker thread
    // (worker.rs), NOT here. This `spawn` runs on the Tauri `setup` hook, which
    // is synchronous and must return quickly — with the Vulkan backend compiled
    // in, touching whisper/ggml symbols here triggers GPU backend init
    // (device enumeration) and stalls setup, so the main window never gets
    // shown or titled. Keep this function to channel/thread wiring only.
    worker::spawn_stt_worker(app.clone(), settings.clone(), runtime.jobs_rx, runtime.state.clone());
    spawn_mic_streamer(app.clone(), runtime.recording.clone(), runtime.mic.clone());
    capture::spawn_capture_thread(
        app,
        settings,
        runtime.cmd_rx,
        runtime.jobs_tx,
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
