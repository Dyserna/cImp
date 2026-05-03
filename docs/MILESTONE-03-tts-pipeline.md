# Milestone 3: TTS Pipeline

## Goal

Replace the stub TTS logger from Milestone 2 with actual speech synthesis using Kokoro via ONNX Runtime, and play the resulting audio through the system default output device. After this milestone, the application speaks Claude Code's tagged output.

## Why This Milestone Now

We've validated that tagged content can be extracted reliably (Milestone 2). The natural next step is to make it audible. Doing TTS before the avatar pane means we can verify the audio pipeline (latency, queue management, volume, mute) on its own merits, and the avatar work in Milestones 4 and 5 already has a real audio signal to react to.

## Phonemization Risk: Spike First

Kokoro requires phoneme input, not raw text. This is the highest-risk technical question in the project. Before committing to the full milestone, **spike on phonemization** to determine the path.

### Spike Tasks

1. Investigate current Rust crates for English G2P (grapheme-to-phoneme):
   - Look at what Rust Kokoro implementations like `kokoros` use
   - Investigate `espeak-ng` Rust bindings (`espeakng`, `espeak-rs`, or similar)
   - Check whether a pure-Rust G2P library exists at sufficient quality
2. Build a small standalone proof of concept that takes a short English sentence, produces phonemes, and feeds them to Kokoro ONNX, producing audio
3. Verify the output sounds correct on a few test sentences

### Decision Criteria

- **Pure-Rust path works cleanly**: proceed with in-process synthesis as designed. This is the preferred outcome.
- **Pure-Rust path works but with quality issues**: proceed with in-process but document the limitations; revisit if it bothers the user
- **Pure-Rust path doesn't work or is too brittle**: fall back to a small Python sidecar that handles phonemization (and possibly synthesis too). Update DESIGN.md to reflect this. The sidecar adds complexity but isolates the messy part.

Do not spend more than a day or two on this spike. If it's clearly going to take a week to get right in Rust, the sidecar is the right call.

## Scope

### In Scope (Assuming Pure-Rust Path)

- A `tts` module that consumes the `ProcessingEvent::TtsSegment` stream from Milestone 2
- Kokoro model loading from a local ONNX file, with CUDA execution provider when available and CPU fallback
- Phonemization of input sentences to the format Kokoro expects
- Voice embedding loading (single hardcoded voice for this milestone, e.g., `af_bella`)
- ONNX inference producing raw PCM samples (24kHz, mono, f32)
- An `audio` module managing playback via `cpal` and `rodio`:
  - Output stream opened at app launch on system default device
  - Audio queue (sentence N+1 synthesizes while sentence N plays, queued back-to-back)
  - Volume control (defaulted to 1.0, hardcoded for this milestone)
  - Mute control (hardcoded false for this milestone)
- An amplitude tap exposing recent samples for the future visualizer (build the API now; consumers come in Milestone 5)
- Graceful handling of synthesis errors (single segment failure does not crash; logged and skipped)
- Graceful handling of audio device errors (logged; future milestones will surface these as error states)

### In Scope (If Sidecar Path)

- All of the above, except phonemization and possibly synthesis happens in a Python subprocess
- The subprocess is launched at app startup, communicates over stdin/stdout (JSON or msgpack) or a local Unix socket / named pipe
- Subprocess lifecycle managed cleanly: started before first synthesis, killed on app shutdown
- Subprocess receives text segments, returns PCM bytes
- Treat the sidecar as an implementation detail of the `tts` module — the public API is the same

### Out of Scope

- Multiple voice options (Milestone 6 settings)
- Configurable speed (Milestone 6)
- Volume / mute UI (Milestone 6 settings)
- Interrupt-on-input behavior (Milestone 6 / 7, requires settings + integration)
- Error state propagation to avatar (Milestones 4–7)

## Acceptance Criteria

1. When Claude Code emits `[[TTS]]some text[[/TTS]]`, the user hears "some text" spoken in the configured voice
2. Multiple sentences in a single block are spoken sequentially with no gap between them
3. Time from `[[/TTS]]` detection to audible output is well under 1 second on the user's hardware (RTX 5090). On other hardware it may be longer; this is acceptable
4. Subsequent sentences begin synthesizing while the previous one plays (parallel synthesis, sequential playback)
5. The amplitude tap API exists and returns recent sample data when queried; a debug endpoint or log statement can be used to verify
6. If a single synthesis fails, the failure is logged and the application continues to function for subsequent segments
7. If the audio device is unavailable at app launch, the application starts anyway and logs the issue (TTS will be silent until restart, which is acceptable for v1)
8. Closing the application cleanly stops audio playback and shuts down the synthesis pipeline (no zombie threads, no audio glitches on exit)
9. Works on both Windows and Linux

## Implementation Approach

### Module Structure

```
src-tauri/src/
  tts/
    mod.rs           # public API
    engine.rs        # Kokoro ONNX wrapper
    phonemize.rs     # phonemization (or sidecar wrapper if applicable)
    voice.rs         # voice embedding loading
  audio/
    mod.rs           # public API
    playback.rs      # cpal + rodio output stream and queue
    amplitude.rs     # amplitude ring buffer for visualizer
```

### Dependencies to Add (Cargo.toml)

- `ort` (ONNX Runtime bindings, with `cuda` feature if targeting CUDA)
- `cpal`
- `rodio`
- Phonemization crate (TBD from spike result)
- Possibly `ndarray` for ONNX tensor manipulation
- Possibly `tokio-process` if going the sidecar route

### TTS Engine Public API

```
pub struct TtsEngine {
    // owns the loaded model, voice embedding, phonemizer
}

pub struct TtsRequest {
    pub text: String,
    pub request_id: u64,  // for matching response/cancellation
}

pub struct TtsResponse {
    pub request_id: u64,
    pub samples: Vec<f32>,  // PCM, 24kHz mono
    pub sample_rate: u32,
}

impl TtsEngine {
    pub async fn new(model_path: PathBuf, voice_path: PathBuf) -> Result<Self, AppError>;
    pub async fn synthesize(&self, req: TtsRequest) -> Result<TtsResponse, AppError>;
    pub fn shutdown(self) -> Result<(), AppError>;
}
```

`synthesize()` is async and may be slow (tens to hundreds of milliseconds). Multiple concurrent calls are fine if the GPU isn't saturated; ONNX Runtime handles parallel inference internally.

### Audio Playback Public API

```
pub struct AudioOutput {
    // owns the cpal stream, the rodio sink, the amplitude buffer
}

pub struct AmplitudeTap {
    // shared, cloneable handle for reading recent amplitude data
}

impl AudioOutput {
    pub fn new() -> Result<Self, AppError>;
    pub fn enqueue(&self, samples: Vec<f32>, sample_rate: u32);
    pub fn set_volume(&self, volume: f32);
    pub fn set_muted(&self, muted: bool);
    pub fn stop_all(&self);  // for interrupt-on-input later
    pub fn amplitude_tap(&self) -> AmplitudeTap;
    pub fn is_playing(&self) -> bool;
}

impl AmplitudeTap {
    pub fn recent_samples(&self, count: usize) -> Vec<f32>;
    pub fn current_amplitude_rms(&self) -> f32;
}
```

The amplitude buffer is a ring buffer holding the last N samples actually sent to the audio device (post-volume, pre-mute). Size it to hold at least one second of audio (24000 samples) to give the visualizer flexibility.

Important: tapping amplitude must not require locking that blocks audio playback. Use `Arc<RwLock<RingBuffer>>` with read-locks held briefly, or a lock-free ring buffer if it becomes a bottleneck.

### Wiring

In `main.rs`:

```
// Existing from Milestone 2: PTY → ProcessingLayer → {terminal events, ProcessingEvent::TtsSegment channel}
// New for Milestone 3:
//   ProcessingEvent::TtsSegment → TTS task → audio queue → cpal output
//   AudioOutput::amplitude_tap → (held for Milestone 5)

let tts_engine = TtsEngine::new(model_path, voice_path).await?;
let audio_output = AudioOutput::new()?;

// TTS task:
//   while let Some(event) = proc_rx.recv().await {
//     match event {
//       ProcessingEvent::TtsSegment(text) => {
//         match tts_engine.synthesize(TtsRequest { text, request_id: next_id() }).await {
//           Ok(resp) => audio_output.enqueue(resp.samples, resp.sample_rate),
//           Err(e) => tracing::warn!("synthesis failed: {}", e),
//         }
//       },
//       other => /* forward as before */,
//     }
//   }
```

A single TTS task processes segments sequentially. This is simplest and gives natural ordering. If parallel synthesis becomes desirable (and it might, for lower latency on multi-sentence blocks), spawn N synthesis tasks pulling from a shared work queue and use sequence IDs to enqueue audio in order.

For Milestone 3, sequential synthesis is fine. The 5090 will synthesize fast enough that parallelism isn't critical.

### Model and Voice Files

Decide where the Kokoro ONNX model and voice embeddings live:

- **Option A**: bundled with the application (large binary)
- **Option B**: downloaded on first run to a known location
- **Option C**: user-provided path in settings (Milestone 6)

For development, hardcode a path (e.g., `~/.config/<app>/models/kokoro-v1.onnx` and a voices directory). Document the requirement in the README. Move toward Option A or B for distribution decisions later (out of scope for v1).

### Phonemizer Integration (Pure-Rust Path)

Pseudocode:

```
pub struct Phonemizer { /* G2P state */ }

impl Phonemizer {
    pub fn new() -> Result<Self, AppError>;
    pub fn phonemize(&self, text: &str) -> Result<Vec<i64>, AppError>; // tokens for Kokoro
}
```

The output format depends on Kokoro's tokenizer expectations. Reference existing Rust Kokoro implementations for the exact token mapping.

### Sidecar Path (If Needed)

If pure-Rust phonemization isn't viable:

1. Create a small Python script that:
   - Loads Kokoro (or just the phonemizer) once at startup
   - Reads requests from stdin (one JSON object per line: `{"id": 123, "text": "..."}`)
   - Writes responses to stdout (one JSON object per line containing PCM as base64 or raw bytes — measure which is faster)
2. Spawn this script from Rust via `tokio::process::Command`
3. Manage lifecycle: start at app launch, kill on app shutdown, handle unexpected exit (log and degrade gracefully)
4. Communicate via stdin/stdout streams managed by `tokio::io`

The sidecar is opaque to the rest of the app; the `TtsEngine` API stays the same.

If going this route, document it in DESIGN.md so the next session knows about it.

## Validation Steps

1. **Basic synthesis**: have Claude emit `[[TTS]]Hello world.[[/TTS]]`. Hear "Hello world."
2. **Multi-sentence**: have Claude emit `[[TTS]]First sentence. Second sentence. Third sentence.[[/TTS]]`. Hear all three with no gaps.
3. **Latency**: measure or estimate the time from `[[/TTS]]` to audible output. Should feel responsive (well under 1 second on 5090).
4. **Long-running session**: have a multi-turn conversation with several TTS-tagged responses. Verify no degradation, no memory growth, no audio glitches.
5. **Synthesis failure recovery**: artificially trigger a synthesis error (e.g., feed an empty string or extremely long input). Verify the error is logged and subsequent segments still work.
6. **Clean shutdown**: close the app while audio is playing. Verify no audio glitch on exit, no zombie threads or processes.
7. **Cross-platform**: validate on the second target platform (audio device behavior under PulseAudio, PipeWire, or WASAPI may differ).
8. **Amplitude tap basic check**: log amplitude data periodically while audio is playing; verify it's non-zero and varies. Visual verification will come in Milestone 5.

## Unit Tests

- Phonemizer: known input → expected token sequence (a few sanity cases)
- Audio queue: enqueueing samples produces output (use a mock/null audio backend if cpal can be replaced for testing)

Most of this milestone is integration-heavy; manual validation is the main quality gate.

## Known Risks and Mitigation

- **Phonemization complexity**: covered above; spike first
- **ONNX Runtime CUDA setup**: requires CUDA libraries on the system. Document the requirement clearly. CPU fallback should be automatic via `ort`.
- **Voice embedding format**: Kokoro voice files are typically `.npz` or similar. Parsing them requires either a Rust ndarray loader or a one-time conversion to a more convenient format
- **Audio crackling or glitches**: if the audio output stream's buffer is too small, you'll get glitches under load. cpal's defaults are usually fine, but tune buffer size if needed
- **Sample rate conversion**: Kokoro outputs 24kHz. The system audio device may want 44.1kHz or 48kHz. cpal handles resampling but verify it works correctly. Alternatively, use rodio's resampling
- **Long sentences exceed model context**: Kokoro may have a maximum input length (often around 500 tokens). For very long sentences, split further. The sentence segmenter from Milestone 2 should already produce short-enough segments, but log a warning if any segment exceeds the limit and either truncate or split further

## What "Done" Looks Like

The user can have an interactive Claude Code session, and when Claude wraps something in TTS tags, it gets spoken naturally. The voice is a single fixed voice, volume is 100%, and there's no UI control yet — but the pipeline works end-to-end and the experience feels good. The amplitude tap is in place, ready for the visualizer.

---

## Next Milestone

Milestone 4: Avatar Pane. Restructures the layout to two panes (terminal + avatar), introduces the avatar state machine, and renders configured images per state. No waveform yet — that's Milestone 5.
