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
///
/// `SynthesizeNotification` bypasses the active-tab filter: notifications
/// are *for* inactive tabs by definition (V2-04). The notification manager
/// is the only producer; regular processing-layer segments stay on the
/// `Synthesize` path.
#[derive(Debug)]
pub enum TtsRequest {
    Synthesize { tab: TabId, text: String },
    SynthesizeNotification { tab: TabId, text: String },
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

/// Portable model dir: `<exe-dir>/../models/`. The Windows release zip ships
/// `kokoro-v1.0.onnx` and `voices/af_heart.bin` here so a fresh unzip works
/// without any APPDATA setup. APPDATA always wins over portable on duplicate
/// filenames so a user-installed file overrides a bundled one.
pub fn portable_model_dir() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let dir = exe.parent()?.parent()?.join("models");
    Some(dir)
}

pub fn default_model_path() -> AppResult<PathBuf> {
    let appdata = default_model_dir()?.join(MODEL_FILE);
    if appdata.exists() {
        return Ok(appdata);
    }
    if let Some(portable) = portable_model_dir() {
        let p = portable.join(MODEL_FILE);
        if p.exists() {
            return Ok(p);
        }
    }
    Ok(appdata)
}

pub fn default_voice_path(voice: &str) -> AppResult<PathBuf> {
    let file = format!("{voice}.bin");
    let appdata = default_model_dir()?.join("voices").join(&file);
    if appdata.exists() {
        return Ok(appdata);
    }
    if let Some(portable) = portable_model_dir() {
        let p = portable.join("voices").join(&file);
        if p.exists() {
            return Ok(p);
        }
    }
    Ok(appdata)
}

pub fn report_missing_model_files() {
    let dir = default_model_dir()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| "<config dir>".to_string());
    let portable = portable_model_dir().map(|p| p.display().to_string());
    tracing::warn!(target: "tts", "");
    tracing::warn!(target: "tts", "TTS disabled: Kokoro model files not found.");
    tracing::warn!(target: "tts", "Place these files under: {dir}");
    if let Some(p) = portable.as_deref() {
        tracing::warn!(target: "tts", "  (or under: {p})");
    }
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
