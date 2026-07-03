mod engine;
mod phonemize;
mod voice;
mod worker;

pub use worker::spawn_tts_worker;

use std::path::PathBuf;
use std::sync::atomic::AtomicU64;
use std::sync::{Arc, RwLock};

use crate::error::{AppError, AppResult};
use crate::state::TabId;

pub const DEFAULT_VOICE: &str = "af_heart";
pub const MODEL_FILE: &str = "kokoro-v1.0.onnx";

/// Worker-level request type carried over the shared mpsc from each tab's
/// processing layer to the single TTS worker. The worker filters by the
/// shared `active` cell — requests for inactive tabs are dropped, satisfying
/// the v2 design's "active tab speaks; others are silent" rule.
///
/// `SynthesizeNotification` bypasses the active-tab filter: notifications
/// are *for* inactive tabs by definition (V2-04). The notification manager
/// is the only producer; regular processing-layer segments stay on the
/// `Synthesize` path.
///
/// `SpeakSelection` backs the Ctrl+right-click read-along gesture: a
/// pre-segmented list of sentence chunks synthesized and played in order,
/// each tagged with a `(session, index)` mark so the audio thread can emit
/// progress as it plays through them. `session` matches the value the
/// command stored in the shared [`SpeakSession`] cell — the worker re-checks
/// it before every chunk so an Esc/`tts_stop` (which zeroes the cell) or a
/// superseding read aborts the loop without playing the rest.
#[derive(Debug)]
pub enum TtsRequest {
    Synthesize {
        tab: TabId,
        text: String,
        /// `true` for AI-tag content from the processing layer — these are
        /// dropped while [`AiTtsSuppressed`] is set (Esc stops the rest of the
        /// current burst until new output). `false` for command-initiated
        /// speech (`tts_test`, `tts_speak`), which is never suppressed.
        suppressible: bool,
    },
    SynthesizeNotification { tab: TabId, text: String },
    SpeakSelection {
        tab: TabId,
        session: u64,
        chunks: Vec<String>,
    },
}

/// Shared "current selection-read session" id. `tts_speak_selection` stores
/// its session id here; `tts_stop` (and the Esc gesture behind it) zeroes it.
/// The TTS worker reads it before each chunk to decide whether to keep going.
/// 0 means "no active selection read".
pub type SpeakSession = Arc<AtomicU64>;

/// Shared "suppress AI-tag TTS" flag. Set by `tts_stop` (Esc) so the rest of
/// the current Claude output burst's `[[TTS]]` segments are dropped instead of
/// played; cleared by the state manager on the next `ClaudeOutputStarted` (new
/// output). Only gates `TtsRequest::Synthesize { suppressible: true }` —
/// notifications and selection reads are never affected.
pub type AiTtsSuppressed = Arc<std::sync::atomic::AtomicBool>;

/// Shared active-tab cell, read on every TTS request (worker) and on every
/// audio playback edge (audio thread). Synchronous because the audio thread
/// is a plain std::thread; the TTS worker holds the read lock for only a
/// few instructions, never across an await point.
pub type ActiveTab = Arc<RwLock<TabId>>;

/// Portable model dir: `<exe-dir>/../models/`. The Windows release zip ships
/// `kokoro-v1.0.onnx` and `voices/af_heart.bin` here so a fresh unzip works
/// out of the box. This is the only location the runtime looks at.
pub fn model_dir() -> AppResult<PathBuf> {
    let exe = std::env::current_exe()
        .map_err(|e| AppError::Tts(format!("current_exe failed: {e}")))?;
    let dir = exe
        .parent()
        .and_then(|p| p.parent())
        .ok_or_else(|| AppError::Tts("exe has no grandparent dir".into()))?;
    Ok(dir.join("models"))
}

pub fn default_model_path() -> AppResult<PathBuf> {
    Ok(model_dir()?.join(MODEL_FILE))
}

pub fn default_voice_path(voice: &str) -> AppResult<PathBuf> {
    Ok(model_dir()?.join("voices").join(format!("{voice}.bin")))
}

pub fn report_missing_model_files() {
    let dir = model_dir()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| "<exe-dir>/../models".to_string());
    tracing::warn!(target: "tts", "");
    tracing::warn!(target: "tts", "TTS disabled: Kokoro model files not found.");
    tracing::warn!(target: "tts", "Place these files under: {dir}");
    tracing::warn!(target: "tts", "  {MODEL_FILE}");
    tracing::warn!(target: "tts", "  voices/{DEFAULT_VOICE}.bin");
    tracing::warn!(target: "tts", "Sources:");
    tracing::warn!(
        target: "tts",
        "  https://huggingface.co/onnx-community/Kokoro-82M-v1.0-ONNX/resolve/main/onnx/model.onnx"
    );
    tracing::warn!(
        target: "tts",
        "  https://huggingface.co/onnx-community/Kokoro-82M-v1.0-ONNX/resolve/main/voices/{DEFAULT_VOICE}.bin"
    );
    tracing::warn!(target: "tts", "");
}
