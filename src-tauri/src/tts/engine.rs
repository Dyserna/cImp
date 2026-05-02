//! Kokoro v1.0 ONNX wrapper. Single-owner: the [`TtsEngine`] is created once
//! at app startup and lives in the TTS worker task. `synthesize` takes
//! `&mut self` because `ort::Session::run` does — calls are serialized
//! through the worker's mpsc.

use std::path::Path;

use ort::execution_providers::{CPUExecutionProvider, CUDAExecutionProvider, ExecutionProvider};
use ort::session::{builder::GraphOptimizationLevel, Session};
use ort::value::Tensor;

use crate::error::{AppError, AppResult};
use crate::tts::phonemize::Phonemizer;
use crate::tts::voice::VoicePack;

pub const SAMPLE_RATE: u32 = 24_000;

#[derive(Debug)]
pub struct TtsRequest {
    pub text: String,
    pub request_id: u64,
}

#[derive(Debug)]
pub struct TtsResponse {
    pub request_id: u64,
    pub samples: Vec<f32>,
    pub sample_rate: u32,
}

pub struct TtsEngine {
    session: Session,
    voice: VoicePack,
    phonemizer: Phonemizer,
    speed: f32,
}

impl TtsEngine {
    pub fn new(model_path: &Path, voice_path: &Path) -> AppResult<Self> {
        if !model_path.exists() {
            return Err(AppError::ModelNotFound(model_path.display().to_string()));
        }

        let mut builder = Session::builder()
            .map_err(|e| AppError::Tts(format!("session builder: {e}")))?
            .with_optimization_level(GraphOptimizationLevel::Level3)
            .map_err(|e| AppError::Tts(format!("opt level: {e}")))?;

        // CPU is the default; opt in to CUDA via `CCTTS_GPU=cuda`. The CUDA
        // path works on Pascal/Volta/Turing/Ampere/Ada NVIDIA cards but is
        // broken on Blackwell (sm_120) with ORT 1.20's prebuilt — see
        // MAINTENANCE.md. Re-test both EPs when `ort` bumps to ORT 1.21+.
        let bound_ep = match std::env::var("CCTTS_GPU").ok().as_deref() {
            Some("cuda") => match CUDAExecutionProvider::default().register(&mut builder) {
                Ok(()) => "GPU (CUDA)",
                Err(e) => {
                    tracing::warn!(error = %e, "CUDA EP unavailable; falling back to CPU");
                    CPUExecutionProvider::default()
                        .register(&mut builder)
                        .map_err(|e| AppError::Tts(format!("CPU EP register: {e}")))?;
                    "CPU"
                }
            },
            Some(other) if !other.is_empty() => {
                tracing::warn!(value = %other, "unknown CCTTS_GPU value; using CPU");
                CPUExecutionProvider::default()
                    .register(&mut builder)
                    .map_err(|e| AppError::Tts(format!("CPU EP register: {e}")))?;
                "CPU"
            }
            _ => {
                CPUExecutionProvider::default()
                    .register(&mut builder)
                    .map_err(|e| AppError::Tts(format!("CPU EP register: {e}")))?;
                "CPU"
            }
        };

        let session = builder
            .commit_from_file(model_path)
            .map_err(|e| AppError::Tts(format!("load {}: {e}", model_path.display())))?;
        let voice = VoicePack::load(voice_path)?;
        let phonemizer = Phonemizer::new();
        tracing::info!(
            voice = voice.name(),
            bound = bound_ep,
            "TTS engine ready"
        );
        Ok(Self {
            session,
            voice,
            phonemizer,
            speed: 1.0,
        })
    }

    /// Reload the voicepack from disk. Used by the worker on a settings
    /// change; engine retains the current voice if reload fails.
    pub fn set_voice(&mut self, path: &Path) -> AppResult<()> {
        let new = VoicePack::load(path)?;
        self.voice = new;
        Ok(())
    }

    pub fn set_speed(&mut self, speed: f32) {
        self.speed = speed.max(0.1);
    }

    pub fn current_voice_name(&self) -> &str {
        self.voice.name()
    }

    pub fn synthesize(&mut self, req: TtsRequest) -> AppResult<TtsResponse> {
        let phonemes = self.phonemizer.phonemize(&req.text)?;
        if phonemes.raw_count == 0 {
            tracing::debug!(text = %req.text, "empty phoneme sequence; emitting silence");
            return Ok(TtsResponse {
                request_id: req.request_id,
                samples: Vec::new(),
                sample_rate: SAMPLE_RATE,
            });
        }

        let n = phonemes.padded_ids.len();
        let input_ids = Tensor::from_array(([1usize, n], phonemes.padded_ids.clone()))
            .map_err(|e| AppError::Tts(format!("input_ids tensor: {e}")))?;

        let style_vec = self.voice.style_for(phonemes.raw_count).to_vec();
        let style = Tensor::from_array(([1usize, VoicePack::style_dim()], style_vec))
            .map_err(|e| AppError::Tts(format!("style tensor: {e}")))?;

        let speed = Tensor::from_array(([1usize], vec![self.speed]))
            .map_err(|e| AppError::Tts(format!("speed tensor: {e}")))?;

        let outputs = self
            .session
            .run(ort::inputs![
                "input_ids" => input_ids,
                "style" => style,
                "speed" => speed,
            ])
            .map_err(|e| AppError::Tts(format!("inference: {e}")))?;

        let (_shape, samples) = outputs[0]
            .try_extract_tensor::<f32>()
            .map_err(|e| AppError::Tts(format!("extract output: {e}")))?;

        Ok(TtsResponse {
            request_id: req.request_id,
            samples: samples.to_vec(),
            sample_rate: SAMPLE_RATE,
        })
    }
}
