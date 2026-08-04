//! Kokoro v1.0 ONNX wrapper. Single-owner: the [`TtsEngine`] is created once
//! at app startup and lives in the TTS worker task. `synthesize` takes
//! `&mut self` because `ort::Session::run` does — calls are serialized
//! through the worker's mpsc.

use std::path::Path;

// `ort::ep` is rc.11's module for execution providers; the old
// `ort::execution_providers::*ExecutionProvider` aliases are `#[deprecated]`.
#[cfg(all(feature = "tts-cuda", not(feature = "tts-webgpu")))]
use ort::ep::CUDA;
#[cfg(feature = "tts-webgpu")]
use ort::ep::WebGPU;
use ort::ep::{ExecutionProvider, CPU};
use ort::session::{builder::GraphOptimizationLevel, builder::SessionBuilder, Session};
use ort::value::Tensor;

use crate::error::{AppError, AppResult};
use crate::settings::ProcessingDevice;
use crate::tts::phonemize::Phonemizer;
use crate::tts::voice::VoicePack;

pub const SAMPLE_RATE: u32 = 24_000;

/// Label for the compiled GPU backend, used when a GPU EP registers. The
/// backend is a compile-time choice (mutually-exclusive `tts-webgpu` /
/// `tts-cuda` Cargo features); only one of these consts exists per build.
#[cfg(feature = "tts-webgpu")]
const GPU_BACKEND: &str = "GPU (WebGPU)";
#[cfg(all(feature = "tts-cuda", not(feature = "tts-webgpu")))]
const GPU_BACKEND: &str = "GPU (CUDA)";

/// Register the compiled GPU execution provider. Exactly one EP is selected at
/// compile time; `tts-webgpu` wins if both features are (mis)configured on.
#[cfg(any(feature = "tts-webgpu", feature = "tts-cuda"))]
fn register_gpu_ep(builder: &mut SessionBuilder) -> AppResult<()> {
    let result = {
        #[cfg(feature = "tts-webgpu")]
        {
            WebGPU::default().register(builder)
        }
        #[cfg(all(feature = "tts-cuda", not(feature = "tts-webgpu")))]
        {
            CUDA::default().register(builder)
        }
    };
    result.map_err(|e| AppError::Tts(format!("GPU EP register: {e}")))
}

/// Engine-level synthesis request. Distinct from the worker-channel
/// [`crate::tts::TtsRequest`] (which carries a `TabId` for active-tab
/// filtering); this struct is what the engine itself consumes.
#[derive(Debug)]
pub struct SynthesisRequest {
    pub text: String,
    pub request_id: u64,
}

#[derive(Debug)]
pub struct SynthesisResponse {
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
    pub fn new(model_path: &Path, voice_path: &Path, device: ProcessingDevice) -> AppResult<Self> {
        if !model_path.exists() {
            return Err(AppError::ModelNotFound(model_path.display().to_string()));
        }

        let mut builder = Session::builder()
            .map_err(|e| AppError::Tts(format!("session builder: {e}")))?
            .with_optimization_level(GraphOptimizationLevel::Level3)
            .map_err(|e| AppError::Tts(format!("opt level: {e}")))?;

        let bound_ep = Self::register_execution_provider(&mut builder, device)?;

        let session = builder
            .commit_from_file(model_path)
            .map_err(|e| AppError::Tts(format!("load {}: {e}", model_path.display())))?;
        let voice = VoicePack::load(voice_path)?;
        let phonemizer = Phonemizer::new();
        tracing::info!(voice = voice.name(), bound = bound_ep, "TTS engine ready");
        Ok(Self {
            session,
            voice,
            phonemizer,
            speed: 1.0,
        })
    }

    /// Register the execution provider and return its label. When a GPU backend
    /// is compiled in (`tts-webgpu` / `tts-cuda`), the `device` setting selects
    /// GPU vs CPU: `Gpu` registers the GPU EP and falls back to CPU
    /// automatically if its registration fails (no usable GPU, driver issue, …)
    /// — so the same binary runs everywhere, mirroring `stt/engine.rs`; `Cpu`
    /// forces CPU. The `device` setting is authoritative (the old `CIMP_GPU`
    /// env override is gone). Built with no GPU feature, this is always CPU.
    ///
    /// NB: a successful GPU registration means the EP is *active*, not that
    /// every op runs on the GPU — the WebGPU EP can place unsupported ops on CPU.
    #[cfg(any(feature = "tts-webgpu", feature = "tts-cuda"))]
    fn register_execution_provider(
        builder: &mut SessionBuilder,
        device: ProcessingDevice,
    ) -> AppResult<&'static str> {
        if device == ProcessingDevice::Cpu {
            Self::register_cpu(builder)?;
            return Ok("CPU (forced)");
        }
        match register_gpu_ep(builder) {
            Ok(()) => Ok(GPU_BACKEND),
            Err(e) => {
                tracing::warn!(error = %e, "TTS GPU EP unavailable; falling back to CPU");
                Self::register_cpu(builder)?;
                Ok("CPU (GPU fallback)")
            }
        }
    }

    #[cfg(not(any(feature = "tts-webgpu", feature = "tts-cuda")))]
    fn register_execution_provider(
        builder: &mut SessionBuilder,
        _device: ProcessingDevice,
    ) -> AppResult<&'static str> {
        Self::register_cpu(builder)?;
        Ok("CPU")
    }

    fn register_cpu(builder: &mut SessionBuilder) -> AppResult<()> {
        CPU::default()
            .register(builder)
            .map_err(|e| AppError::Tts(format!("CPU EP register: {e}")))
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

    pub fn synthesize(&mut self, req: SynthesisRequest) -> AppResult<SynthesisResponse> {
        let phonemes = self.phonemizer.phonemize(&req.text)?;
        if phonemes.raw_count == 0 {
            tracing::debug!(text = %req.text, "empty phoneme sequence; emitting silence");
            return Ok(SynthesisResponse {
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

        // Guard against a model that produces no outputs (corrupt/wrong file
        // in `models/`): `outputs[0]` would otherwise panic and permanently
        // kill the TTS worker. Every other failure here is a graceful Err.
        if outputs.len() == 0 {
            return Err(AppError::Tts("model produced no outputs".into()));
        }
        let (_shape, samples) = outputs[0]
            .try_extract_tensor::<f32>()
            .map_err(|e| AppError::Tts(format!("extract output: {e}")))?;

        // Sanitize non-finite samples. A corrupt model or a silent EP fallback
        // can emit NaN/inf, which clicks in the sink, poisons RMS metering
        // (sumsq += NaN stays NaN), and serializes as invalid JSON over the
        // amplitude IPC (NaN/Infinity aren't valid JSON). Clamp to silence.
        let samples: Vec<f32> = samples
            .iter()
            .map(|&s| if s.is_finite() { s } else { 0.0 })
            .collect();

        Ok(SynthesisResponse {
            request_id: req.request_id,
            samples,
            sample_rate: SAMPLE_RATE,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// End-to-end synthesis smoke test on whatever EP is compiled in (CPU by
    /// default; `GPU (WebGPU)` under `--features tts-webgpu`). Asserts the model
    /// loads and produces real (non-silent) audio — the failure mode for an
    /// unsupported-op silent fallback or a broken ConvTranspose is silence/
    /// garbage. Ignored by default: needs the model files under `<repo>/models`
    /// and, on a GPU build, pulls the GPU.
    ///
    /// Run:        `cargo test --bin cimp [--features tts-webgpu] -- --ignored --nocapture synthesizes`
    /// CPU baseline: pass `ProcessingDevice::Cpu` below.
    #[test]
    #[ignore]
    fn synthesizes() {
        let _ = tracing_subscriber::fmt()
            .with_env_filter(
                tracing_subscriber::EnvFilter::try_from_default_env()
                    .unwrap_or_else(|_| "info".into()),
            )
            .with_test_writer()
            .try_init();

        // <repo>/models — CARGO_MANIFEST_DIR is src-tauri.
        let repo = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("src-tauri has a parent");
        let model = repo.join("models").join(crate::tts::MODEL_FILE);
        let voice = repo
            .join("models")
            .join("voices")
            .join(format!("{}.bin", crate::tts::DEFAULT_VOICE));
        assert!(model.exists(), "model missing at {}", model.display());
        assert!(voice.exists(), "voice missing at {}", voice.display());

        // The bound EP is logged by `TtsEngine::new` ("TTS engine ready
        // bound=…"); the tracing subscriber above surfaces it under --nocapture.
        let mut engine =
            TtsEngine::new(&model, &voice, ProcessingDevice::Gpu).expect("engine init");

        let text = "Hello world. This is a text to speech test.";
        // First synth pays one-time shader compilation on GPU backends; the
        // long-lived engine warms once, so this also exercises that path.
        let t = std::time::Instant::now();
        let resp = engine
            .synthesize(SynthesisRequest {
                text: text.into(),
                request_id: 1,
            })
            .expect("synthesis");
        let synth_ms = t.elapsed().as_millis();

        let n = resp.samples.len();
        let peak = resp.samples.iter().fold(0f32, |m, &s| m.max(s.abs()));
        let rms = if n > 0 {
            (resp.samples.iter().map(|s| s * s).sum::<f32>() / n as f32).sqrt()
        } else {
            0.0
        };
        let dur_s = n as f32 / resp.sample_rate as f32;
        eprintln!("=== samples: {n} ({dur_s:.2}s audio) | first synth {synth_ms} ms | peak {peak:.4} | rms {rms:.4} ===");

        assert!(n > 0, "no samples produced");
        assert!(peak > 0.05, "output peak too low ({peak}) — likely silence");
        assert!(rms > 0.005, "output rms too low ({rms}) — likely silence");
    }
}
