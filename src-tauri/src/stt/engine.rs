//! Whisper engine wrapper (whisper.cpp via `whisper-rs`). Single-owner: one
//! [`SttEngine`] is constructed lazily on the first recording and lives in the
//! STT transcription worker thread. Each `transcribe` call creates a fresh
//! decoder state; the heavyweight `WhisperContext` (the loaded model) is
//! reused across calls and only rebuilt when the user picks a different model.
//!
//! GPU handling differs from `tts/engine.rs` (which is opt-in via
//! `CCTTS_GPU=cuda`). whisper.cpp's CUDA backend is a *compile-time* feature
//! (`stt-cuda`, on by default); when it's compiled in, STT uses the GPU
//! **by default** and falls back to CPU automatically if GPU init fails, so a
//! driver/device problem never disables dictation. `CCTTS_GPU=cpu` forces CPU.
//! When the binary is built CPU-only (`--no-default-features`), there is no GPU
//! backend and STT always runs on CPU.

use std::path::Path;

use whisper_rs::{FullParams, SamplingStrategy, WhisperContext, WhisperContextParameters};

use crate::error::{AppError, AppResult};

/// Whisper's required input sample rate. Capture resamples to this before
/// handing samples to [`SttEngine::transcribe`].
pub const WHISPER_SAMPLE_RATE: u32 = 16_000;

pub struct SttEngine {
    ctx: WhisperContext,
    /// The model filename this context was loaded from (e.g.
    /// "ggml-small.bin"). The worker compares it against the current
    /// setting to decide whether a reload is needed.
    model_file: String,
}

impl SttEngine {
    /// Load a GGML Whisper model. `model_file` is the bare filename used
    /// for the reload check; `model_path` is the resolved absolute path.
    pub fn new(model_path: &Path, model_file: String) -> AppResult<Self> {
        if !model_path.exists() {
            return Err(AppError::ModelNotFound(model_path.display().to_string()));
        }

        // GPU is the default whenever the CUDA backend is compiled in
        // (feature `stt-cuda`). `CCTTS_GPU=cpu` forces CPU. On a GPU init
        // failure we retry on CPU automatically.
        let force_cpu = std::env::var("CCTTS_GPU").as_deref() == Ok("cpu");
        let gpu_compiled = cfg!(feature = "stt-cuda");

        if gpu_compiled && !force_cpu {
            match Self::load_ctx(model_path, true) {
                Ok(ctx) => {
                    tracing::info!(target: "stt", model = %model_file, backend = "GPU (CUDA)", "STT engine ready");
                    return Ok(Self { ctx, model_file });
                }
                Err(e) => {
                    tracing::warn!(target: "stt", error = %e, "STT GPU init failed; falling back to CPU");
                }
            }
        }

        let ctx = Self::load_ctx(model_path, false)?;
        let backend = if gpu_compiled && !force_cpu {
            "CPU (GPU fallback)"
        } else {
            "CPU"
        };
        tracing::info!(target: "stt", model = %model_file, backend, "STT engine ready");
        Ok(Self { ctx, model_file })
    }

    fn load_ctx(model_path: &Path, use_gpu: bool) -> AppResult<WhisperContext> {
        let mut params = WhisperContextParameters::default();
        params.use_gpu(use_gpu);
        WhisperContext::new_with_params(model_path, params)
            .map_err(|e| AppError::Stt(format!("load {}: {e}", model_path.display())))
    }

    pub fn model_file(&self) -> &str {
        &self.model_file
    }

    /// Transcribe 16 kHz mono f32 samples. `language` is "auto" (detect) or a
    /// forced ISO code ("en", "he", …); `translate` selects Whisper's
    /// translate-to-English task. Returns the concatenated, trimmed text of
    /// all segments (empty string for silence / too-short audio).
    pub fn transcribe(
        &self,
        samples: &[f32],
        language: &str,
        translate: bool,
    ) -> AppResult<String> {
        let mut state = self
            .ctx
            .create_state()
            .map_err(|e| AppError::Stt(format!("create_state: {e}")))?;

        let mut params = FullParams::new(SamplingStrategy::Greedy { best_of: 1 });
        if language != "auto" {
            params.set_language(Some(language));
        }
        params.set_translate(translate);
        // whisper.cpp prints decoding progress / special tokens to stdout by
        // default — silence both so they don't pollute the terminal cctts is
        // wrapping.
        params.set_print_progress(false);
        params.set_print_realtime(false);
        params.set_print_special(false);
        params.set_print_timestamps(false);
        // Use all but one core, leaving headroom for the UI/PTY. Clamp to at
        // least 1 on single-core hosts.
        let threads = std::thread::available_parallelism()
            .map(|n| (n.get().saturating_sub(1)).max(1))
            .unwrap_or(1);
        params.set_n_threads(threads as i32);

        state
            .full(params, samples)
            .map_err(|e| AppError::Stt(format!("inference: {e}")))?;

        let n = state.full_n_segments();
        let mut text = String::new();
        for i in 0..n {
            if let Some(seg) = state.get_segment(i) {
                if let Ok(s) = seg.to_str_lossy() {
                    text.push_str(&s);
                }
            }
        }
        Ok(text.trim().to_string())
    }
}
