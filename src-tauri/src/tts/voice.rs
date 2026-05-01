//! Voicepack loading. Each Kokoro voice ships as a flat `f32` dump that
//! conceptually has shape `(N, 1, 256)`: N style vectors of 256 floats each,
//! indexed by the unpadded token count of the utterance. We load with a
//! single read + `bytemuck` reinterpretation — no extra deps.
//!
//! N is detected from the file size rather than hardcoded; in practice
//! Kokoro v1.0 voicepacks have N = 510 (matching the model's 510-token
//! limit), but the upstream docs say 511 in some places. Auto-detection
//! sidesteps the off-by-one.

use std::path::Path;

use crate::error::{AppError, AppResult};

const STYLE_DIM: usize = 256;
const MIN_ROWS: usize = 64; // sanity floor

pub struct VoicePack {
    /// Flat storage of shape `(rows, STYLE_DIM)`.
    embeddings: Vec<f32>,
    rows: usize,
    name: String,
}

impl VoicePack {
    pub fn load(path: &Path) -> AppResult<Self> {
        let bytes = std::fs::read(path)
            .map_err(|e| AppError::ModelNotFound(format!("{}: {e}", path.display())))?;
        if bytes.len() % (4 * STYLE_DIM) != 0 {
            return Err(AppError::Tts(format!(
                "voicepack {} length {} is not a multiple of {} f32 bytes",
                path.display(),
                bytes.len(),
                4 * STYLE_DIM
            )));
        }
        let floats: &[f32] = bytemuck::cast_slice(&bytes);
        let rows = floats.len() / STYLE_DIM;
        if rows < MIN_ROWS {
            return Err(AppError::Tts(format!(
                "voicepack {}: only {} rows; expected at least {}",
                path.display(),
                rows,
                MIN_ROWS
            )));
        }
        let name = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("unknown")
            .to_string();
        tracing::info!(voice = %name, rows, "voicepack loaded");
        Ok(Self {
            embeddings: floats.to_vec(),
            rows,
            name,
        })
    }

    /// Style embedding for an utterance with `token_count` unpadded tokens.
    /// Clamped to the available range.
    pub fn style_for(&self, token_count: usize) -> &[f32] {
        let idx = token_count.min(self.rows - 1);
        &self.embeddings[idx * STYLE_DIM..(idx + 1) * STYLE_DIM]
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub const fn style_dim() -> usize {
        STYLE_DIM
    }
}
