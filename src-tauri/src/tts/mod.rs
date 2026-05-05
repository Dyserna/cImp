mod engine;
mod phonemize;
mod voice;
mod worker;

pub use engine::TtsEngine;
#[allow(unused_imports)]
pub use engine::{SynthesisRequest, SynthesisResponse, SAMPLE_RATE};
pub use worker::spawn_tts_worker;

use std::path::PathBuf;
use std::sync::{Arc, RwLock};

use crate::error::{AppError, AppResult};
use crate::state::TabId;

pub const DEFAULT_VOICE: &str = "af_heart";
pub const MODEL_FILE: &str = "kokoro-v1.0.onnx";

/// Embedded TTS markup convention. Each tab's `tts_injection.instructions`
/// defaults to this string; the user can override per tab in settings.
pub const RUNTIME_SYSTEM_PROMPT: &str = include_str!("runtime_prompt.md");

/// Worker-level request type carried over the shared mpsc from each tab's
/// processing layer to the single TTS worker. The worker filters by the
/// shared `active` cell — requests for inactive tabs are dropped, satisfying
/// the v2 design's "active tab speaks; others are silent" rule.
#[derive(Debug)]
pub enum TtsRequest {
    Synthesize { tab: TabId, text: String },
}

/// Shared active-tab cell, read on every TTS request (worker) and on every
/// audio playback edge (audio thread). Synchronous because the audio thread
/// is a plain std::thread; the TTS worker holds the read lock for only a
/// few instructions, never across an await point.
pub type ActiveTab = Arc<RwLock<TabId>>;

pub fn default_model_dir() -> AppResult<PathBuf> {
    Ok(dirs::config_dir()
        .ok_or_else(|| AppError::Tts("no config dir on this platform".into()))?
        .join("cctts")
        .join("models"))
}

pub fn default_model_path() -> AppResult<PathBuf> {
    Ok(default_model_dir()?.join(MODEL_FILE))
}

pub fn default_voice_path(voice: &str) -> AppResult<PathBuf> {
    Ok(default_model_dir()?
        .join("voices")
        .join(format!("{voice}.bin")))
}

pub fn report_missing_model_files() {
    let dir = default_model_dir()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| "<config dir>".to_string());
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
