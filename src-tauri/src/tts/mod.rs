mod engine;
mod phonemize;
mod voice;
mod worker;

pub use engine::TtsEngine;
#[allow(unused_imports)] // public API for upcoming milestones
pub use engine::{TtsRequest, TtsResponse, SAMPLE_RATE};
pub use worker::spawn_tts_worker;

use std::path::PathBuf;

use crate::error::{AppError, AppResult};

pub const DEFAULT_VOICE: &str = "af_heart";
pub const MODEL_FILE: &str = "kokoro-v1.0.onnx";

/// System prompt appended to the embedded `claude` invocation so the
/// runtime knows to wrap prose in `[[TTS]]...[[/TTS]]` markers. Embedded at
/// compile time via `include_str!` so the binary needs no external file.
pub const RUNTIME_SYSTEM_PROMPT: &str = include_str!("runtime_prompt.md");

/// Resolve the directory model files live in. On Windows this is
/// `%APPDATA%\cctts\models\`. Files within:
/// - `kokoro-v1.0.onnx`
/// - `voices/<voice>.bin`
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

/// Print a clear instruction block when model files are missing.
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
