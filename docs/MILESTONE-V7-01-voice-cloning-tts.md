# Milestone V7-01: Voice Cloning TTS (clone a voice from a short clip)

> **Release tag:** TBD by the user at ship time (V-series numbering is independent of the git tag, per the V6-01 convention). The "V7-01" label is a placeholder — adjust if you'd rather slot this as V6-02 under the audio pillar. This milestone is a **TTS *engine* expansion**: it adds a second, cloning-capable synthesis backend alongside the existing Kokoro engine, so the user can supply an audio file of a voice and have cctts speak in that voice.
>
> **Relation to V6-01.** The chosen engine (Chatterbox-Turbo) clones from audio **alone** and needs no reference transcript, so this milestone has **no hard dependency** on [MILESTONE-V6-01-speech-to-text.md](MILESTONE-V6-01-speech-to-text.md). V6-01 is only reused opportunistically: its `stt/capture.rs` mic-capture lets the user *record* a reference clip in-app (file upload works without it). V7-01 can ship before or after V6-01.

## Purpose

Let the user **clone a voice** and have cctts speak in it. The user provides:

1. **An audio file** containing a sample of the target voice (record via the existing mic capture or pick a file). Optimal length **≈ 10 seconds** (usable range ~5–30 s; longer gives no benefit and can hurt).
2. **An optional transcript** of what the sample says — **purely optional and currently unused**: Chatterbox-Turbo clones from the audio alone. The field is kept in the UI/profile for future engine-swaps, but the chosen engine ignores it.

cctts processes this once into a reusable **voice profile**, and from then on the avatar speaks AI prose in that voice. Everything stays **fully offline, no Python, no cloud** — same portability promise as Kokoro.

### Why this is a *replacement* engine, not a config tweak

The current TTS engine is **Kokoro v1.0** (`tts/engine.rs`), whose "voices" are static, pre-computed `256×N` f32 style-embedding `.bin` files (`tts/voice.rs`, `VoicePack::style_for(token_count)`). There is **no path from reference audio to a Kokoro style embedding** — Kokoro fundamentally cannot clone. So cloning requires a **different model** wired in behind the existing worker boundary. Kokoro stays as the default preset-voice engine; cloning is an additive, selectable backend.

### How it fits the existing architecture

- The **TTS worker** (`tts/worker.rs`: single-owner engine on an mpsc, `.manage`d handle, active-tab gating, suppression atomics) is the integration seam — we swap *what* synthesizes behind it, not the worker contract.
- The **audio output path** (`audio/playback.rs`: rodio sink on a dedicated thread, resamples to device rate) is engine-agnostic. Both Kokoro and the cloning model output **24 kHz mono f32** — no playback changes.
- The **mic capture** from V6-01 (`stt/capture.rs`, `cpal` input + `rubato` resample) is reused to *record* a reference clip in-app (optional; file upload works standalone).
- The **existing `ort` ONNX runtime** (already used for Kokoro, with the WebGPU/CUDA EP selection in `tts/engine.rs`) loads Chatterbox-Turbo's ONNX components — **no new inference runtime** is added.
- **Settings schema↔types↔store** and the **`models/` + Git LFS + two-zip release** plumbing are established extension points.

## Engine: Chatterbox-Turbo (decided)

**Chosen: Chatterbox-Turbo via the existing `ort` ONNX runtime.** The decision favors **cloning fidelity** — speaker similarity is what a cloning feature is judged on, and Chatterbox leads there. ZipVoice (sherpa-onnx) is retained below as the documented fallback if the Phase-0 spike shows Chatterbox's latency or short-text behavior is unacceptable.

| | **Chatterbox-Turbo / `ort`** *(chosen)* | **ZipVoice / sherpa-onnx** *(fallback)* |
|---|---|---|
| Rust path | Wire the 4-component ONNX pipeline on the existing `ort` runtime | First-class `sherpa-rs` / `sherpa-onnx` C API bindings |
| New native dep | **None — reuses `ort`** | sherpa-onnx (a 2nd ONNX C++ runtime alongside `ort`) |
| Model size / arch | ~0.5B, **autoregressive LLM** | ~123M, non-autoregressive flow-matching |
| License | MIT (commercial-OK) | Apache/MIT (commercial-OK) |
| Reference clip | ~5–10 s | ~5–10 s |
| Transcript of reference | **Not needed** | Used (would auto-fill via Whisper) |
| Output | 24 kHz | 24 kHz |
| **Speaker similarity** | **Higher (≈0.63–0.67 tts-bench)** | Lower (≈0.49 tts-bench) |
| Naturalness (UTMOS) | ~4.04 (Turbo) | ~4.15 |
| Short-utterance stability | **Risk: AR line garbles "Hi!"/"Yes" (#97) — verify on Turbo** | Stable (NAR) |
| Watermark | Mandatory inaudible PerTh watermark | None |

**Why Chatterbox for *this* product:** the whole point of the feature is sounding like the uploaded voice, and Chatterbox's speaker similarity is markedly higher than ZipVoice's on the one neutral benchmark. It also reuses the `ort` runtime already in the binary (no second native dependency), and needs no reference transcript. The accepted costs: it's a ~0.5B autoregressive model (latency must be proven on real hardware — see spike), it carries a mandatory inaudible watermark, the Turbo variant **silently ignores the emotion/exaggeration controls** of base Chatterbox, and the autoregressive family has a documented short-text failure mode that needs a mitigation plan.

> **Variant note:** "Chatterbox-Turbo" specifically = the official Resemble AI **ONNX export** (components: `speech_encoder`, `embed_tokens`, `language_model`, `conditional_decoder`; fp32/fp16/q8/q4 variants, CUDA-capable). Base Chatterbox is **Python-only** and cannot meet the no-Python constraint — so it is out, despite its emotion controls. The C++ reference port `DDATT/Chatterbox-turbo-cpp` consumes the same ONNX files and is a useful pipeline reference.

The spec is written behind a `CloneEngine` trait so the fallback slots in without re-architecting if the spike forces a switch.

## Decisions locked (from research + discussion)

- **Cloning is an additive backend.** Kokoro stays as the default engine for preset voices; cloning is selected via a new `tts.engine` setting. No rip-out. A user with no reference clip keeps working exactly as today.
- **Engine abstraction.** Introduce a `TtsBackend` enum / `CloneEngine` trait so `tts/worker.rs` dispatches to the active backend. Synthesis signature stays "text in → 24 kHz mono f32 out."
- **Voice profiles.** A cloned voice is a **profile** stored under `models/voices/cloned/<name>/` containing: the resampled reference wav, the (provided or auto-generated) transcript, and any cached conditioning. Profiles are managed in Settings. **Global, single active profile** — consistent with the locked "avatar/TTS stay global-only" decision (no per-tab voices).
- **Optimal reference duration ≈ 10 s** (UI guidance: 5–30 s; warn below ~4 s and above ~30 s). Mono, resampled to the model's required rate via `rubato`.
- **Transcript is optional and unused by the engine.** Chatterbox-Turbo clones from audio alone. Keep an optional transcript field in the profile/UI for future engine-swaps, but no Whisper auto-transcribe is wired (no V6-01 dependency).
- **Phonemizer.** The cloning path uses Chatterbox's own text tokenizer (`embed_tokens`), so **misaki/espeak is not on the cloned path** — the GPLv3 espeak dependency is irrelevant when cloning. misaki stays for the Kokoro path.
- **Runtime.** Reuse the existing `ort` ONNX Runtime and its WebGPU/CUDA EP selection (`tts/engine.rs`) — **no new inference dependency**.
- **Consent gate.** A one-time acknowledgement in the clone-creation UI: *only clone voices you have permission to use.* Non-negotiable for an ethics/abuse posture; cheap to add.

## What This Milestone Delivers

Phases are ordered to get **one cloned voice speaking end-to-end** as early as possible, then layer on enrollment UX, settings, and packaging.

**Phase A — Backend: clone engine behind the TTS worker**

1. **`CloneEngine` + backend dispatch.**
   - New `tts/clone.rs` — `CloneEngine` wrapping Chatterbox-Turbo's ONNX components loaded via the existing `ort` runtime. `synthesize(text, &VoiceProfile) -> AppResult<Vec<f32>>` returning 24 kHz mono.
   - Refactor `tts/engine.rs` / `tts/mod.rs` so the worker holds a `TtsBackend` (`Kokoro(TtsEngine)` | `Clone(CloneEngine)`), selected from `settings.tts.engine`. The Kokoro struct is unchanged; it's just one arm now.
2. **Voice-profile model + store.** `tts/profile.rs` — `VoiceProfile { name, ref_wav_path, transcript, sample_rate, conditioning_cache }`, `list_profiles()`, `load(name)`, `save(...)`, `delete(name)`, all under `models/voices/cloned/<name>/` (`ref.wav`, `transcript.txt` (optional/unused), `cond.bin` (cached speaker encoding), `profile.json`).
3. **Enrollment.** `create_profile(name, source_audio_path, transcript: Option<String>)`: decode source audio (any common format via `symphonia`, or `hound` for wav), downmix to mono, `rubato`-resample to Chatterbox's expected input rate, persist `ref.wav`; run the **`speech_encoder`** ONNX component once on the clip and cache its output as the speaker conditioning (`cond.bin`) — this is the expensive step that we do at enroll time, not per utterance; store the optional `transcript` verbatim if provided (unused by the engine); write `profile.json`.
4. **IPC commands** (`ipc/commands.rs`, registered in `generate_handler!`):
   - `tts_list_clone_profiles() -> Vec<String>`
   - `tts_create_clone_profile(name, audio_path, transcript: Option<String>) -> Result<()>` (runs enrollment off the UI thread; emits progress/`tts-clone-state`)
   - `tts_delete_clone_profile(name)`
   - `tts_preview_clone(name, text)` — synthesize a short sample for the user to audition

**Phase B — One cloned voice end-to-end (engine selection + speak)**

5. Extend `TtsSettings` with `engine: TtsEngineKind` (`kokoro` | `clone`) and `clone_profile: String`. The worker reads these on the settings broadcast (same path that already applies voice/speed live) and switches backend / reloads the active profile.
6. Minimal frontend wiring: a way to pick engine = clone + active profile (even a temporary debug control), so an existing profile can be exercised. Full UX is Phase D.
7. Confirm the full path: AI output → `[[TTS]]` extraction (`processing/tags.rs`) → worker → `CloneEngine` → rodio sink, in the cloned voice, with suppression/selection-tracking intact (those live above the engine, so they should "just work").

**Phase C — Enrollment UX (record or upload + optional transcript)**

8. **`src/lib/clone.ts`** — IPC wrappers + stores (`cloneProfiles`, `cloneState`), event listeners for enrollment progress and errors.
9. **Reference capture/upload** — a "Create voice" dialog:
   - **Record** (reuse V6-01 mic capture; show the existing waveform; ~10 s guidance with a soft timer) **or** **Choose file** (Tauri file dialog).
   - **Optional transcript** textarea with helper text: "Optional — not required for cloning." (Stored but unused by Chatterbox; kept for future engines.)
   - **Consent checkbox** (locked decision).
   - **Create** → calls `tts_create_clone_profile`, shows progress (decoding → encoding speaker → ready), then a **Preview** button (`tts_preview_clone`).
10. **Manage profiles** — list, rename/delete, set active, re-preview.

**Phase D — Settings surface**

11. Settings UI: engine selector (Kokoro presets vs Cloned voice), active-profile dropdown, link into the Create/Manage dialog. When `engine = clone` but no profile exists, guide the user to create one. Speed/volume/mute apply across both engines.

**Phase E — Polish**

12. Quality/UX polish: loudness-normalize the reference on enrollment; trim leading/trailing silence; warn on too-short/too-noisy clips; graceful fallback to Kokoro if a profile fails to load.

**Phase F — Packaging, deps, docs**

13. `Cargo.toml`: add an audio-decode crate (`symphonia`, or `hound` for wav-only) for reading uploaded reference files. **No new inference runtime** — Chatterbox-Turbo loads through the `ort` crate already present. Pick the quantization variant (fp16 vs q8/q4) at impl time based on the spike's quality/latency results.
14. Ship the Chatterbox-Turbo ONNX assets under `models/` via **Git LFS** (`speech_encoder`, `embed_tokens`, `language_model`, `conditional_decoder` + the text tokenizer), add SHA-256s to `models/CHECKSUMS.txt`, and copy them into the **full** release zip only (slim/no-models zip untouched), mirroring the Kokoro/Whisper packaging.
15. Docs: `DESIGN.md` (voice cloning in the feature list), `PACKAGING.md` (new "Voice-Cloning Model Files" section), `README.md` (how to clone a voice, clip-length guidance, consent note), `NOTICE` (Chatterbox/Chatterbox-Turbo MIT license + a note that output carries Resemble's inaudible PerTh watermark), `CHANGELOG.md` (Added: voice cloning).

## What This Milestone Does NOT Do

- **Real-time streaming synthesis of partials.** Synthesis is per-utterance, same as today's Kokoro path (the existing pipeline already chunks AI output by sentence, which helps perceived latency). Live token-streaming TTS is a future candidate — and may become more relevant given Chatterbox's autoregressive latency profile.
- **On-the-fly cloning per message.** You enroll a profile once; you don't pass a fresh clip per utterance. (The runtime *could* re-condition each call, but caching a profile is faster and matches the global-voice model.)
- **Per-tab cloned voices.** Global-only, per the locked avatar/TTS decision. One active voice app-wide.
- **Cross-lingual / multi-speaker mixing.** English first (broader language coverage is a followup; the Multilingual Chatterbox variant is a later option). No speaker blending.
- **Removing Kokoro.** Kokoro remains the default and the no-clip path.
- **Cloud or API cloning.** Offline only, by constraint.
- **Voice-likeness guarantees or watermarking of *our* output** (beyond whatever the chosen model embeds). We add a consent gate, not a detection system.

## Implementation Steps

A → B → C → D → E → F. A+B give a cloned voice speaking from a pre-made profile; C makes it user-creatable; D/E/F productionize.

### Phase A — Backend

#### A.1 Backend abstraction (`tts/mod.rs`, `tts/engine.rs`)

Introduce the dispatch enum and keep the synthesis contract identical:
```rust
pub enum TtsBackend {
    Kokoro(engine::TtsEngine),   // existing, unchanged
    Clone(clone::CloneEngine),   // new
}

impl TtsBackend {
    /// 24 kHz mono f32, same as Kokoro today.
    pub fn synthesize(&mut self, text: &str) -> AppResult<Vec<f32>> {
        match self {
            TtsBackend::Kokoro(e) => e.synthesize_text(text),   // wraps the current path
            TtsBackend::Clone(e)  => e.synthesize(text),
        }
    }
}
```
The worker constructs the active backend from `settings.tts.engine` and rebuilds it when that setting (or `clone_profile`) changes — the same broadcast path that already applies `voice`/`speed` live.

#### A.2 Clone engine (`tts/clone.rs`)

Load the four Chatterbox-Turbo ONNX components as `ort` sessions (reuse `tts/engine.rs`'s EP-selection block for WebGPU/CUDA + CPU fallback). Hold them plus the active `VoiceProfile`'s cached speaker conditioning. The synthesis pipeline:

1. **Text → tokens** via Chatterbox's text tokenizer → `embed_tokens` session → token embeddings.
2. **Autoregressive decode:** loop the `language_model` session, feeding the cached speaker conditioning + the running sequence, sampling speech tokens until EOS (with a max-length guard). This is the latency-dominant, sequential step.
3. **`conditional_decoder`** turns the speech tokens into a 24 kHz waveform. Return the samples.

The `speech_encoder` is **not** in the per-utterance path — it runs once at enroll time and its output is cached in the profile (`cond.bin`). Watermarking is applied by the model/runtime as shipped; do not strip it.

> **API note:** this 4-component pipeline (tokenizer details, the AR sampling loop, tensor shapes, KV-cache handling, EOS token) must be reverse-engineered from the official `chatterbox-turbo-ONNX` repo and the `DDATT/Chatterbox-turbo-cpp` reference port at impl time. The Phase-0 spike exists precisely to nail this down before feature code. Pin the model revision and quantization variant.

#### A.3 Voice profiles (`tts/profile.rs`) + enrollment

`VoiceProfile` load/save under `models/voices/cloned/<name>/`. Enrollment decodes arbitrary input audio → mono → `rubato` → model rate, persists `ref.wav`, runs `speech_encoder` once and caches the conditioning to `cond.bin`, stores the optional transcript verbatim (unused), writes `profile.json`. Run enrollment on a blocking task; emit `tts-clone-state` (`decoding` | `encoding` | `ready` | `error`).

#### A.4 IPC + handle

Add the four commands to `ipc/commands.rs` and `generate_handler!`. The worker handle gains a way to switch backend/profile on demand (or it simply re-reads settings on the broadcast). Reuse the existing `TtsHandle`/worker; no new long-lived thread is required beyond the worker.

### Phase B — Engine selection end-to-end
Per Deliverables §5–7. Add `engine` + `clone_profile` to `TtsSettings`, switch the backend in the worker, and verify `[[TTS]]` → cloned audio with suppression/selection intact.

### Phase C — Enrollment UX
Per Deliverables §8–10. `clone.ts` + Create-voice dialog (record via V6-01 capture or file pick) + optional transcript + consent + preview + manage list.

### Phase D — Settings
Per Deliverables §11. Engine selector + active profile + entry to Create/Manage; speed/volume/mute shared across engines.

### Phase E — Polish
Per Deliverables §12. Loudness normalize, silence trim, clip-quality warnings, profile-load fallback to Kokoro.

### Phase F — Deps, CI, docs
Per Deliverables §13–15.

## Test Plan

### Phase A (backend)
- **Unit (Rust)** — `VoiceProfile` round-trips through serde; `list_profiles()` enumerates `models/voices/cloned/*`; enrollment resamples a 48 kHz test clip to the model rate (length ≈ `len * rate/48000`, dominant frequency preserved) and writes a non-empty `cond.bin`.
- **Manual** — create a profile from a fixture wav, then synthesize fixed text via the clone backend; confirm 24 kHz mono samples and audibly the reference timbre. **Short-text check:** synthesize "Yes.", "Done.", "Hi!" and confirm intelligible output (this is the #97 risk area).

### Phase B (engine select)
- **Manual** — set `tts.engine = clone` + a profile; AI output is spoken in the cloned voice. Switch back to `kokoro`; preset voice returns. `tts_stop`, selection read-along, and suppression all behave as on Kokoro.

### Phase C (enrollment UX)
- **Manual** — record ~10 s in-app → create → preview plays in the new voice. Upload a file path → same. Transcript left blank works (engine ignores it). Consent unchecked → Create is blocked. Delete a profile → it's gone and, if it was active, the engine falls back gracefully.

### Phase D (settings)
- **Manual** — engine + profile selection persists across restart; old `settings.json` without the new fields loads with defaults (additive `#[serde(default)]`). Speed/volume/mute affect the cloned voice.

### Phase E
- **Manual** — a too-short (<4 s) or silent clip surfaces a clear warning; a noisy clip still enrolls but with a soft caution; a corrupt profile falls back to Kokoro with a toast, no panic.

### Phase F
- **Build** — `cargo build` succeeds with the audio-decode dep (no new inference runtime; `ort` already present). `npm run build` succeeds.
- **Packaging** — fresh full-zip unzip clones a voice out of the box; slim zip works once the user drops the model files in `models/`; missing model files degrade to a clear in-UI error.

## Files Most Likely Touched

**Backend (`src-tauri/src`)**
- `tts/clone.rs`, `tts/profile.rs` — new
- `tts/mod.rs`, `tts/engine.rs`, `tts/worker.rs` — `TtsBackend` dispatch, backend switch on settings
- `ipc/commands.rs` — `tts_list_clone_profiles` / `tts_create_clone_profile` / `tts_delete_clone_profile` / `tts_preview_clone`
- `error.rs` — `Clone`/`VoiceProfile` error variants (reuse `ModelNotFound`)
- `settings/schema.rs` — `TtsSettings.engine`, `TtsSettings.clone_profile`, `TtsEngineKind`
- `Cargo.toml` — audio decode (`symphonia`/`hound`); reuses existing `ort`

**Frontend (`src`)**
- `lib/clone.ts` — new: stores, IPC wrappers, enrollment listeners
- Create/Manage-voice dialog component(s) — new (reuse the V6-01 waveform + capture)
- `lib/settings/types.ts`, `lib/settings/store.ts` — `TtsSettings` mirror (engine, clone_profile), derived store
- `SettingsApp.svelte` — engine selector + profile management entry
- `App.svelte` — `initClone()` startup wiring

**Packaging / docs**
- `.github/workflows/release.yml`, `models/CHECKSUMS.txt`
- `docs/DESIGN.md`, `docs/PACKAGING.md`, `docs/MAINTENANCE.md`, `README.md`, `NOTICE`, `CHANGELOG.md`

## Risks and Open Questions

- **Phase-0 spike is mandatory and is the gate on this whole milestone.** Before any feature code, stand up the four Chatterbox-Turbo ONNX components on `ort` in a throwaway Rust binary, reverse-engineering the pipeline from the official `chatterbox-turbo-ONNX` repo + the `DDATT/Chatterbox-turbo-cpp` reference port, and clone a real ~10 s clip on the dev GPU. It must answer: (1) the exact 4-component pipeline (tokenizer, AR sampling loop, KV-cache, EOS); (2) **latency on our hardware** — see next risk; (3) **short-text behavior** — see below; (4) which quantization variant (fp16/q8/q4) holds quality. If any of these fails, fall back to the documented ZipVoice/sherpa-onnx path.
- **Latency is the primary risk (re-examined).** Chatterbox is a ~0.5B **autoregressive** model — it emits speech tokens sequentially, so cost scales with output length and is fundamentally heavier than Kokoro or a flow-matching model. The impressive published RTFs came from **TensorRT-LLM / torch.compile**, which do **not** transfer to plain ONNX-Runtime-in-Rust. On consumer GPUs via `ort`, real-time is plausible but **not guaranteed**. The spike must measure first-audio latency and RTF for both short and long utterances; if it's sluggish, options are a smaller quantization, keeping Kokoro for low-latency cases, or switching to ZipVoice. (Note: the earlier assumption that the size difference makes performance "minimal" is incorrect — ~123M NAR vs ~0.5B AR is a real gap.)
- **Short-utterance gibberish (#97).** The autoregressive Chatterbox line has a documented, unfixed tendency to garble ultra-short inputs ("Hi!", "Yes", "Done.") — exactly what an assistant avatar emits constantly. The spike must test this on **Turbo** specifically. Mitigations if present: pad short segments with trailing punctuation/silence, route ≤1–2-word confirmations through Kokoro, or batch short fragments. Treat a clean short-text result as a release gate.
- **Mandatory watermark.** All Chatterbox output carries Resemble's inaudible PerTh watermark; it cannot be disabled. Independent measurement suggests it's effectively inaudible at 24 kHz but not provably "zero impact." Acceptable; disclose in `NOTICE`/README.
- **Turbo drops emotion controls.** The exaggeration/CFG knobs that distinguish base Chatterbox are silently ignored by the Turbo ONNX variant. We get fidelity, not expressivity. Fine for this milestone; note it so nobody wires up controls that do nothing.
- **Engine reload cost.** Switching `engine`/`clone_profile` rebuilds backend state (sessions + cached conditioning). Rebuild on change; don't hot-swap mid-utterance. Cache loaded profiles/sessions to keep switching snappy.
- **Reference clip quality.** Clones inherit noise/room/codec artifacts from the reference. Mitigate with loudness normalization + silence trim on enrollment and UI guidance (clean, ~10 s, single speaker); optional denoise pass is a followup.
- **Licensing/attribution.** Chatterbox/Chatterbox-Turbo are MIT (commercial-OK). Confirm the ONNX export's license metadata and add `NOTICE` attribution + the watermark disclosure. (F5-TTS CC-BY-NC weights and any Python-only model remain out — they violate the constraints.)
- **Ethics/consent.** The consent gate is the floor. Consider a visible indicator that the active voice is cloned, and document acceptable use in README.

## Followups Tracked Elsewhere (FUTURE-FEATURES candidates)

- **Streaming/live partial synthesis** for the cloned voice once batch playback proves out.
- **Multilingual / cross-lingual cloning** (CosyVoice2-class quality) if a no-Python ONNX path matures — currently Python-only, so out of scope.
- **Reference denoise / enhancement** pass before enrollment for noisy clips.
- **Multiple saved profiles with quick-switch** UX (the storage model already supports many; this is just surfacing it) — still global-active, just faster switching.
- **Emotion/expressivity controls** if a future Chatterbox variant exposes them through ONNX (Turbo currently ignores them), or by adopting base Chatterbox should a no-Python path appear.
- **Fallback to ZipVoice/sherpa-onnx** behind the same `CloneEngine` trait if latency or short-text stability proves unworkable — the trait is designed so this swap doesn't re-architect anything.
