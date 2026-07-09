# Milestone V6-01: Speech-to-Text (offline dictation)

> **Release tag:** TBD by the user at ship time (the V-series numbering is independent of the git tag — V1.4-07 shipped as v1.3.3). V6-01 opens a new feature pillar on top of V5's design-token substrate. It is the first milestone to add an **audio *input*** path; until now the audio stack has been output-only (Kokoro playback + the played-sample amplitude tap).

## Purpose

Let the user dictate into cimp by voice instead of typing. Press-to-record (a bottom-bar button) and push-to-talk (a configurable `Ctrl+Shift` hold) capture microphone audio; a **fully offline, bundled** Whisper model transcribes it; the transcript lands in the **compose overlay** for review and send. No cloud, no API key, no native-OS speech recognizer — the model ships in the portable zip exactly like Kokoro does today.

Three things make this a clean fit with the existing architecture:

- The **TTS worker pattern** (`tts/worker.rs`: single-owner engine on an mpsc, `.manage`d handle, active-tab gating) is a ready-made template for an STT worker.
- The **compose overlay** (`composeState.ts` + `ComposeOverlay.svelte`) already auto-grows a textarea and submits to the active PTY via `ptyWrite`; the transcript appends into `composeContent`.
- The **bottom status bar** (`StatusBar.svelte`) already hosts pluggable buttons from `src/lib/status/`; the record button drops in alongside Mute / Announcements / Volume.
- The **shortcut dispatcher** (`shortcuts/dispatcher.ts`) and **settings schema↔types↔store** plumbing are established, well-trodden extension points.

## Decisions locked (from design discussion)

- **Engine:** `whisper-rs` (whisper.cpp bindings). Fully offline, one statically-linked C library, optional CUDA feature wired to the existing `CIMP_GPU=cuda` opt-in. Handles the entire Whisper pipeline (mel spectrogram → encoder → autoregressive decoder → tokenizer → timestamps) in one call, which is why we do **not** hand-roll Whisper on the already-present `ort` runtime. Justified C-FFI per project constraints (the toolchain already needs LLVM for misaki).
- **Model:** **selectable**, default **`ggml-small.bin`** (multilingual, ~466 MB). Users can drop other `ggml-*.bin` files into `models/` and pick them in Settings. Committed to `models/` via **Git LFS** (same as the Kokoro ONNX model + voicepacks) and verified against `models/CHECKSUMS.txt` at release time. The release workflow already produces **two zips** — a **full** variant (with models) and a **slim/no-models** variant — so the STT model rides in the full zip only and the no-models update zip is unaffected. The added zip size is therefore a non-issue (the user confirmed the two-build split covers it).
- **Capture:** `cpal` **input** stream, mono f32, resampled to Whisper's required **16 kHz mono** via **`rubato`**.
- **Destination:** the **compose overlay** — transcript appends to `composeContent` (opening the sheet if closed), user reviews and sends with `Ctrl+Enter`.
- **Default triggers:** bottom-bar **button = toggle** (click to start / click to stop); **push-to-talk = hold** (`Ctrl+Shift` down → record, release → stop). Both are configurable in Settings.
- **PTT stays bare `Ctrl+Shift`** (user decision — it's one-handed, simple, and used repeatedly). Rather than weaken it, we (a) implement **abort-on-other-key + min-hold debounce** semantics so a `Ctrl+Shift`-prefixed shortcut or terminal copy/paste doesn't produce a stray recording, and (b) **remap cimp's own three `Ctrl+Shift+…` default shortcuts off `Ctrl+Shift`** to remove the most common collisions (see Phase C / Phase D).

## What This Milestone Delivers

The phases are ordered to get one trigger working end-to-end as early as possible, then layer on the second trigger, settings surface, and packaging.

**Phase A — Backend capture + engine + worker behind IPC**

1. New **`stt/` module** mirroring `tts/`:
   - `mod.rs` — public API; STT model-dir resolution (reuse `tts::model_dir()` — same `<exe-dir>/../models/`), `list_models()` (enumerate `ggml-*.bin`), `default_model_path()`, and a `report_missing_model_files()` warning clone.
   - `engine.rs` — `SttEngine` wrapping `whisper_rs::WhisperContext` / `WhisperState`; `transcribe(samples_16k_mono: &[f32], opts) -> AppResult<String>`. EP/threads selection mirrors `tts/engine.rs` (`CIMP_GPU=cuda` → CUDA feature, else CPU). Loads the model named by settings.
   - `capture.rs` — `cpal` input-stream lifecycle: `start()` opens the chosen input device, pushes incoming samples into an accumulator (downmix to mono); `stop()` returns the accumulated buffer; an `AmplitudeTap`-style mirror feeds the **mic** waveform (reuse `audio::amplitude`'s ring). Resample native-rate → 16 kHz mono with `rubato` on stop (or stream-resample during capture — decide at impl time; batch-on-stop is simpler and fine for non-streaming).
   - `worker.rs` — single-owner worker on an mpsc (`spawn_stt_worker`). Receives a finished recording, runs `transcribe` off the UI thread, emits `stt-transcription { text }` and `stt-state { state }` (`idle` | `recording` | `transcribing` | `error`). Mirrors `spawn_tts_worker`.
2. **Shared state + handle:** an `SttHandle` (sender + recording flag + current state) created in `main.rs`, `.manage`d alongside the TTS/audio handles. The engine is constructed lazily on first record (or at startup if `stt.enabled`) so a missing model doesn't block launch — it logs the missing-model warning and the button shows a disabled/error state, mirroring how missing Kokoro files degrade TTS.
3. **IPC commands** (`ipc/commands.rs`, registered in `main.rs` `generate_handler!`):
   - `stt_start_recording()` — open capture, set state `recording`.
   - `stt_stop_recording()` — stop capture, hand buffer to the worker, set state `transcribing`; transcript arrives later via event.
   - `stt_cancel()` — stop capture, discard buffer, state `idle`.
   - `stt_list_models() -> Vec<String>` — for the settings dropdown.
   - `stt_list_input_devices() -> Vec<String>` — cpal input device names for the device picker (plus an implicit "System default").

**Phase B — One trigger end-to-end: the bottom-bar button (toggle)**

4. **`src/lib/stt.ts`** — stores (`sttState`, `recording`), thin IPC wrappers for the four commands, and an event listener that on `stt-transcription` **appends** the text into `composeContent` (opening the compose sheet via `openCompose()` if closed) and on `stt-state` updates `sttState`. Registered once at app startup (next to `initSettings` / `installDispatcher` in `App.svelte`).
5. **`src/lib/status/RecordButton.svelte`** — mirrors `MuteButton.svelte` house style (`.status-button`, `aria-pressed`, glyphs). Honors `stt.button_mode`:
   - **toggle** (default): `onclick` flips between `stt_start_recording` / `stt_stop_recording`.
   - **hold**: `onpointerdown` starts, `onpointerup` / `onpointerleave` stops (added in Phase C alongside PTT-hold semantics so both holds share one code path).
   Visual states: idle (mic glyph), recording (pulsing/red, `aria-pressed=true`), transcribing (spinner/disabled). Gated on `$settings.stt.enabled`.
6. Add `<RecordButton />` to `StatusBar.svelte`'s right cluster, behind `{#if $settings.stt.enabled}`.

**Phase C — Push-to-talk (hold) + button hold mode**

7. **Shortcut action `push_to_talk` (bare `Ctrl+Shift`, hold).** The current dispatcher is keydown-only and fire-once; PTT is a **hold** gesture on a **modifiers-only** chord. The user has chosen to keep bare `Ctrl+Shift` (one-handed, repeatable), so the dispatcher must make that chord robust rather than swap it out. Extend `shortcuts/dispatcher.ts`:
   - Add `push_to_talk` to `ShortcutAction` and parse `s.push_to_talk` in `configureShortcuts`.
   - Add a capture-phase **keyup** listener (paired with the existing keydown) plus a small PTT state machine: `idle → armed → recording`. When the required modifiers become satisfied with **no** non-modifier key pressed, move to `armed` and start a debounce timer (~150 ms). If the timer elapses while still held → `recording` (call `start`). On modifier release → if `recording`, `stop` (transcribe); if still `armed` (released before the debounce), do nothing (it was a quick chord, not a dictation).
   - **Abort-on-other-key:** if any non-modifier key is pressed while `armed`/`recording`, treat the `Ctrl+Shift` as a shortcut prefix, not PTT — **cancel** the recording (discard, no transcript) and let the keypress flow to its normal handler. This is what makes bare `Ctrl+Shift` coexist with un-remappable chords like the terminal's `Ctrl+Shift+C`/`Ctrl+Shift+V`.
   - Honor the existing `setSuppressed` guard (shortcut-capture UI) and `stt.enabled`. Wire to `{ start, stop, cancel }` from `stt.ts`.
   - **Vacate `Ctrl+Shift` for cimp's own shortcuts.** Per the user's preference (reconfigure the others rather than weaken PTT), remap the three default bindings that currently sit on `Ctrl+Shift` so they no longer collide with the PTT chord. Proposed new defaults (adjustable — confirm at impl), shipped via the schema/migration in Phase D:
     - `open_compose`: `Ctrl+Shift+E` → **`Alt+Enter`** (rarely used in shells; distinct from `submit_compose` = `Ctrl+Enter`).
     - `split_pane_vertical`: `Ctrl+Shift+\` → **`Alt+\`** (pairs with `split_pane_horizontal` = `Ctrl+\`).
     - `close_pane`: `Ctrl+Shift+W` → **`Ctrl+Alt+W`** (distinct from `close_tab` = `Ctrl+W`).
     New installs get these defaults; existing settings files keep the user's current bindings unless they reset. The abort-on-other-key logic above remains the real safety net — the remap just minimizes the visible arm/abort flicker for the app's own shortcuts, and does nothing for un-remappable terminal/OS chords (which abort-on-other-key handles).
8. **Button hold mode** — implement the `onpointerdown`/`onpointerup` path in `RecordButton.svelte` (shared start/stop with PTT). `button_mode` setting selects toggle vs hold at render time.

**Phase D — Settings surface**

9. **Rust schema** (`settings/schema.rs`): new `SttSettings`, added to `Settings` + `Settings::default()`, `#[serde(default)]` so old files round-trip (no migration needed per PACKAGING.md's additive-field rule):
   ```rust
   #[derive(Clone, Serialize, Deserialize, Debug)]
   #[serde(default)]
   pub struct SttSettings {
       /// Master enable for the whole STT feature (button + PTT).
       pub enabled: bool,
       /// GGML model filename under models/ (e.g. "ggml-small.bin").
       pub model_file: String,
       /// Whisper language hint. "auto" = detect; "en", "he", … force.
       pub language: String,
       /// cpal input device name; empty = system default.
       pub input_device: String,
       /// Bottom-bar record button behavior.
       pub button_mode: SttButtonMode,
       /// Translate non-English speech to English instead of transcribing
       /// verbatim (Whisper's translate task).
       pub translate_to_english: bool,
   }

   #[derive(Clone, Copy, Serialize, Deserialize, Debug, PartialEq, Eq)]
   #[serde(rename_all = "snake_case")]
   pub enum SttButtonMode { Toggle, Hold }

   impl Default for SttSettings {
       fn default() -> Self {
           Self {
               enabled: false,                       // opt-in; needs a model present
               model_file: "ggml-small.bin".into(),
               language: "auto".into(),
               input_device: String::new(),          // system default
               button_mode: SttButtonMode::Toggle,
               translate_to_english: false,
           }
       }
   }
   ```
   Add `pub stt: SttSettings` to `Settings` (+ `Default`). Add `pub push_to_talk: Option<String>` to `ShortcutSettings`, defaulting to `Some("Ctrl+Shift".to_string())`. Change three existing `ShortcutSettings::default()` values off `Ctrl+Shift` (per Phase C): `open_compose` → `Alt+Enter`, `split_pane_vertical` → `Alt+\`, `close_pane` → `Ctrl+Alt+W`. Because these are *default* changes only, an old `settings.json` with the previous strings round-trips untouched — so a one-line **migration** (or an explicit note in CHANGELOG) is warranted for existing users who want the new defaults: either leave their bindings as-is (safest, no migration) or, if we want the conflict gone for everyone, add a migration step that rewrites *only* values still equal to the old `Ctrl+Shift+…` defaults. Recommend the no-migration path (respect user bindings) and document the recommended remap in README for upgraders.
10. **TS mirror** (`settings/types.ts`): `SttSettings` interface + `SttButtonMode` union (`'toggle' | 'hold'`); add `stt` to `Settings` and `push_to_talk` to `ShortcutSettings`. `defaultSettings()` in `store.ts` adds the `stt` block and the `push_to_talk` default; add a derived `stt` store (`derived(settings, s => s.stt)`).
11. **Settings UI** — an **STT section** in `SettingsApp.svelte`:
    - Enable checkbox.
    - Model dropdown populated from `stt_list_models()` (help text: "Drop additional `ggml-*.bin` files into the `models/` folder to add models"). If the configured model is missing, show an inline warning with the download source.
    - Input-device dropdown from `stt_list_input_devices()` (first entry "System default").
    - Language input/select (`auto` + common codes) and a "Translate to English" checkbox.
    - Button-mode toggle (Toggle / Hold).
    - PTT binding via the existing `ShortcutCapture.svelte`, bound to `shortcuts.push_to_talk`.
12. Wire `push_to_talk` through `configureShortcuts` wherever the shortcut handlers are assembled (the `App.svelte` startup wiring that calls `configureShortcuts(s, handlers)`).

**Phase E — Mic waveform + polish**

13. **Mic waveform while recording** — reuse `WaveformOverlay.svelte` / the `audio-amplitude` event plumbing, but source it from the capture tap (a new `mic-amplitude` event from a `spawn_amplitude_streamer`-style task that runs while `recording`). Gives live visual feedback that the mic is hearing the user. Keep it minimal — reuse the existing component, don't build a second visualizer.
14. **Empty/failed transcription UX** — a transcript of "" (silence / too-short) shows a transient toast ("Didn't catch that") via the existing `toast.ts`; a transcription error sets `stt-state: error` and surfaces through the existing error banner/toast path. No crash, mirrors TTS's single-segment-failure tolerance.

**Phase F — Packaging, deps, docs**

15. **`Cargo.toml`** — add `whisper-rs` (with a `cuda` feature wired to the same opt-in story as `ort`) and `rubato`. Note the build now needs a C/C++ toolchain + CMake (whisper.cpp); document in MAINTENANCE.md.
16. **Model + release workflow.** Commit `ggml-small.bin` to `models/` via **Git LFS** (the Kokoro model + voicepacks already live there as LFS blobs) and add its SHA-256 to `models/CHECKSUMS.txt` — the workflow's existing "Verify committed assets against CHECKSUMS.txt" step then covers it automatically. In `.github/workflows/release.yml`, copy the model into the **full** staged layout only (next to `Copy-Item models/kokoro-v1.0.onnx`), leaving the **slim/no-models** zip untouched so re-extracting an update never clobbers a user's local models. No build-time download needed.
17. **Docs** — `PACKAGING.md` (new "Whisper Model Files" section mirroring the Kokoro one; note the larger zip), `DESIGN.md` (add STT to the feature list — it's currently TTS-only), `README.md` (usage: button + PTT + how to add models), `NOTICE` (whisper.cpp is MIT — clean; attribute the Whisper model per its license), and `CHANGELOG.md` (Added: offline speech-to-text).

## What This Milestone Does NOT Do

- **Streaming / live partial transcripts.** Recording is captured to a buffer and transcribed on stop. No real-time word-by-word display. Latency for `small` on CPU is ~1–3 s for a short utterance — acceptable for "stop → transcribe → drop into compose." Streaming is a FUTURE-FEATURES candidate.
- **Voice activity detection / auto-stop on silence.** The user controls start/stop explicitly (button or PTT). No automatic endpointing.
- **Cloud / API STT.** Explicitly out — offline only. No API-key storage concern (a side benefit).
- **Per-tab STT config.** STT is global, consistent with the global-only avatar/TTS decision. One model, one device, one set of triggers app-wide.
- **Sending transcript straight to the PTY.** Transcript always lands in the compose overlay for review this milestone. Direct-to-PTY could be a later setting if friction surfaces.
- **Speaker diarization, punctuation models, custom vocab, or post-edit LLM cleanup.** Raw Whisper output only.
- **macOS audio-input entitlements.** Windows is the primary target (Linux validation deferred per project constraints). cpal input on Windows (WASAPI) is the supported path; document Linux as best-effort.

## Implementation Steps

A → B → C → D → E → F. A+B give a working button; the rest layer on.

### Phase A — Backend capture + engine + worker

#### A.1 Module skeleton

`src-tauri/src/stt/mod.rs`:
```rust
mod capture;
mod engine;
mod worker;

pub use engine::SttEngine;
pub use worker::spawn_stt_worker;

use std::path::PathBuf;
use crate::error::AppResult;

/// STT models live in the same portable dir as Kokoro: <exe-dir>/../models/.
pub fn list_models() -> AppResult<Vec<String>> {
    let dir = crate::tts::model_dir()?;            // reuse the resolver
    let mut out = Vec::new();
    if let Ok(rd) = std::fs::read_dir(&dir) {
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            if name.starts_with("ggml-") && name.ends_with(".bin") {
                out.push(name);
            }
        }
    }
    out.sort();
    Ok(out)
}

pub fn default_model_path(model_file: &str) -> AppResult<PathBuf> {
    Ok(crate::tts::model_dir()?.join(model_file))
}
```
Add `mod stt;` to `main.rs`.

#### A.2 Engine

`src-tauri/src/stt/engine.rs` — mirror `tts/engine.rs`'s structure and EP-selection block:
```rust
use std::path::Path;
use whisper_rs::{FullParams, SamplingStrategy, WhisperContext, WhisperContextParameters};
use crate::error::{AppError, AppResult};

pub struct SttEngine {
    ctx: WhisperContext,
    language: String,           // "auto" | "en" | …
    translate: bool,
}

impl SttEngine {
    pub fn new(model_path: &Path, language: String, translate: bool) -> AppResult<Self> {
        if !model_path.exists() {
            return Err(AppError::ModelNotFound(model_path.display().to_string()));
        }
        // CUDA feature is compile-time in whisper-rs; gate GPU use on
        // CIMP_GPU=cuda to match the TTS engine's runtime convention.
        let mut params = WhisperContextParameters::default();
        params.use_gpu(std::env::var("CIMP_GPU").as_deref() == Ok("cuda"));
        let ctx = WhisperContext::new_with_params(
            &model_path.to_string_lossy(),
            params,
        ).map_err(|e| AppError::Stt(format!("load {}: {e}", model_path.display())))?;
        Ok(Self { ctx, language, translate })
    }

    /// `samples` MUST be 16 kHz mono f32. Caller (capture.rs) resamples.
    pub fn transcribe(&self, samples: &[f32]) -> AppResult<String> {
        let mut state = self.ctx.create_state()
            .map_err(|e| AppError::Stt(format!("create_state: {e}")))?;
        let mut p = FullParams::new(SamplingStrategy::Greedy { best_of: 1 });
        if self.language != "auto" { p.set_language(Some(&self.language)); }
        p.set_translate(self.translate);
        p.set_print_progress(false);
        p.set_print_special(false);
        state.full(p, samples)
            .map_err(|e| AppError::Stt(format!("inference: {e}")))?;
        let n = state.full_n_segments().map_err(|e| AppError::Stt(e.to_string()))?;
        let mut text = String::new();
        for i in 0..n {
            if let Ok(seg) = state.full_get_segment_text(i) { text.push_str(&seg); }
        }
        Ok(text.trim().to_string())
    }
}
```
Add an `Stt(String)` and (if not present) reuse `ModelNotFound` in `error.rs`.

> **API note:** pin `whisper-rs` and confirm the exact `FullParams` / `WhisperContextParameters` surface against the chosen version at impl time — the crate's API has shifted across releases. The shape above is representative, not version-locked.

#### A.3 Capture

`src-tauri/src/stt/capture.rs` — `cpal` input stream. Build the default (or named) input device, `build_input_stream` for f32, push into an `Arc<Mutex<Vec<f32>>>` accumulator (downmix interleaved channels to mono), and tee a copy into an `AmplitudeTap`-style ring for the mic waveform. On `stop()`, drop the stream, take the buffer, and `rubato`-resample `device_rate → 16_000`. Keep the resampler config simple (sinc fixed-ratio). Hold the live `cpal::Stream` in the capture struct (it's `!Send` on some hosts — keep it owned by a dedicated thread or the capture handle, never sent across `.await`).

#### A.4 Worker + handle + IPC

`worker.rs`: `spawn_stt_worker(app, handle)` owns the `SttEngine` (lazy-init on first job), loops on an mpsc of `Vec<f32>` (16 kHz mono), runs `transcribe`, and `app.emit("stt-transcription", json!({ "text": text }))` + `stt-state` transitions. Construct the `SttHandle` in `main.rs`, `.manage` it, and add the commands to `generate_handler!`. The commands flip recording state and post the captured buffer to the worker's sender.

### Phase B — Button end-to-end

#### B.1 `src/lib/stt.ts`
```ts
import { writable } from 'svelte/store';
import { invoke } from '@tauri-apps/api/core';
import { listen } from '@tauri-apps/api/event';
import { composeContent, composeOpen, openCompose } from './composeState';
import { get } from 'svelte/store';

export type SttState = 'idle' | 'recording' | 'transcribing' | 'error';
export const sttState = writable<SttState>('idle');

export const startRecording = () => invoke('stt_start_recording');
export const stopRecording  = () => invoke('stt_stop_recording');
export const cancelRecording = () => invoke('stt_cancel');
export const listSttModels = () => invoke<string[]>('stt_list_models');
export const listInputDevices = () => invoke<string[]>('stt_list_input_devices');

let inited = false;
export function initStt(): void {
  if (inited) return;
  inited = true;
  void listen<{ state: SttState }>('stt-state', (e) => sttState.set(e.payload.state));
  void listen<{ text: string }>('stt-transcription', (e) => {
    const t = e.payload.text?.trim();
    if (!t) return;                       // silence → handled by toast elsewhere
    if (!get(composeOpen)) openCompose();
    const cur = get(composeContent);
    composeContent.set(cur ? `${cur} ${t}` : t);
  });
}
```
Call `initStt()` in `App.svelte` startup.

#### B.2 `RecordButton.svelte` + StatusBar
Clone `MuteButton.svelte`'s markup/CSS. Toggle-mode click handler:
```ts
function onClick() {
  const s = get(sttState);
  if (s === 'recording') void stopRecording();
  else if (s === 'idle') void startRecording();
  // 'transcribing' → disabled
}
```
Add `{#if $settings.stt.enabled}<RecordButton />{/if}` to `StatusBar.svelte`'s `.status-bar-right`.

### Phase C — PTT + button hold
Per Deliverables §7–8. Key detail: the keyup listener stops recording when **any required modifier** of the parsed `push_to_talk` chord is released, and a latch prevents key-repeat from re-triggering start. Share `startRecording`/`stopRecording` with the button's hold path.

### Phase D — Settings
Per Deliverables §9–12. Schema → TS mirror → `defaultSettings()` → derived store → `SettingsApp.svelte` section → `configureShortcuts` wiring.

### Phase E — Mic waveform + polish
Per Deliverables §13–14.

### Phase F — Deps, CI, docs
Per Deliverables §15–17.

## Test Plan

### Phase A (backend)
- **Unit (Rust)** — `list_models()` filters to `ggml-*.bin`; `SttSettings` round-trips through serde with defaults matching spec. Resampler: feed a 48 kHz sine, assert output length ≈ `len * 16000/48000` and dominant frequency preserved (FFT sanity or RMS check).
- **Manual** — with a model present, call `stt_start_recording`, speak, `stt_stop_recording`; confirm a `stt-transcription` event with plausible text in the log. With **no** model present, confirm launch still succeeds, a missing-model warning logs, and `stt_start_recording` returns a clean error (no panic).

### Phase B (button)
- **Manual** — enable STT in settings (after dropping a model in `models/`). Click the record button → it shows recording; speak; click again → transcribing → the text appears in the compose overlay (sheet opens if it was closed). Edit and `Ctrl+Enter` sends to the active tab.
- **Manual** — append behavior: dictate twice without sending; the second transcript appends after the first with a single space.

### Phase C (PTT + hold)
- **Manual** — hold `Ctrl+Shift` → recording starts once (no key-repeat retrigger); release → stops and transcribes. A <150 ms tap produces no recording.
- **Manual** — set button mode to Hold: press-and-hold the button records; release stops. Pointer-leave-while-held stops (no stuck recording).
- **Manual — accidental trigger** — bind another shortcut that uses Ctrl+Shift (e.g. `Ctrl+Shift+\`); confirm using it does not start a dictation (the debounce / latch + chord-exact match prevents it). Re-bind PTT to `Ctrl+Shift+Space` via ShortcutCapture and confirm it works.

### Phase D (settings)
- **Manual** — model dropdown lists every `ggml-*.bin` in `models/`; selecting a different one takes effect on next recording (worker reloads engine on model-setting change — verify the reload path). Input-device dropdown lists devices; picking a non-default device routes capture there. Language `en` vs `auto`; "Translate to English" produces English from non-English speech. Toggling `enabled` shows/hides the bottom-bar button live.
- **Round-trip** — change every STT setting, restart the app, confirm persistence. Confirm an old `settings.json` without an `stt` block loads with defaults (additive `#[serde(default)]`, no migration).

### Phase E
- **Manual** — the mic waveform animates while recording and reflects speech amplitude; it's quiet during silence.

### Phase F
- **Build** — `cargo build` succeeds with the new C-FFI deps (CMake present). `npm run build` succeeds.
- **Packaging** — a fresh portable unzip with the bundled `ggml-small.bin` works STT out of the box; deleting the model degrades gracefully to a clear in-UI "model not found" state.

## Files Most Likely Touched

**Backend (`src-tauri/src`)**
- `stt/mod.rs`, `stt/engine.rs`, `stt/capture.rs`, `stt/worker.rs` — new module
- `main.rs` — `mod stt;`, build `SttHandle`, `.manage`, register commands in `generate_handler!`, call `spawn_stt_worker`, optional `spawn_amplitude_streamer` for mic
- `ipc/commands.rs` — `stt_start_recording` / `stt_stop_recording` / `stt_cancel` / `stt_list_models` / `stt_list_input_devices`
- `error.rs` — `Stt(String)` variant (reuse `ModelNotFound`)
- `settings/schema.rs` — `SttSettings`, `SttButtonMode`, `Settings.stt`, `ShortcutSettings.push_to_talk`
- `audio/` — possibly expose the amplitude ring for reuse by capture (or duplicate the tiny ring in `stt/capture.rs`)
- `Cargo.toml` — `whisper-rs`, `rubato`

**Frontend (`src`)**
- `lib/stt.ts` — new: stores, IPC wrappers, event listeners
- `lib/status/RecordButton.svelte` — new
- `lib/StatusBar.svelte` — mount the button
- `lib/shortcuts/dispatcher.ts` — `push_to_talk` action, keyup listener, hold latch
- `lib/settings/types.ts`, `lib/settings/store.ts` — `SttSettings` mirror, defaults, derived `stt` store
- `SettingsApp.svelte` — STT settings section + PTT ShortcutCapture binding
- `App.svelte` — `initStt()`, wire `push_to_talk` into `configureShortcuts`
- `lib/WaveformOverlay.svelte` — reuse for mic (Phase E)

**Packaging / docs**
- `.github/workflows/release.yml`, `models/CHECKSUMS.txt`
- `docs/PACKAGING.md`, `docs/DESIGN.md`, `docs/MAINTENANCE.md`, `README.md`, `NOTICE`, `CHANGELOG.md`

## Risks and Open Questions

- **`Ctrl+Shift` modifiers-only PTT (kept by user decision).** Those two modifiers are held during many normal shortcuts, so the binding only works because of the **arm/debounce + abort-on-other-key** state machine (Phase C §7): start fires once per chord press (guarded against `event.repeat`), a ~150 ms debounce ignores quick chords, and any non-modifier key while held cancels the recording and lets the keypress through. This is what makes bare `Ctrl+Shift` coexist with un-remappable chords (`Ctrl+Shift+C`/`V` terminal copy-paste, OS shortcuts). The three cimp-own `Ctrl+Shift+…` defaults are remapped off `Ctrl+Shift` to cut visible arm/abort flicker. The residual risk is occasional micro-flicker of the recording indicator when the user presses `Ctrl+Shift+<key>`; acceptable, and tunable via the debounce window.
- **`whisper-rs` build deps + the MSVC bindgen bug.** `whisper-rs` **0.16.0** needs a C++ toolchain (MSVC, auto-found by the `cc` crate), **CMake on PATH** (the `cmake` crate invokes it by name; VS 2026 bundles `cmake` 4.2.3 + Ninja), and **libclang** (already installed at `C:\Program Files\LLVM\bin` and wired via `src-tauri/.cargo/config.toml` → `LIBCLANG_PATH`). The CUDA feature needs the CUDA toolkit (present: 12.2/12.9/13.2). **Known risk:** [CodexMonitor #599](https://github.com/Dimillian/CodexMonitor/issues/599) — `whisper-rs-sys` bindgen can emit glibc types (`_IO_FILE`, …) under MSVC and fail with a `usize` overflow, triggered when bindgen sees MinGW/MSYS headers. **Mitigation:** build from PowerShell or the VS x64 Native Tools prompt (clean of MinGW), **never Git Bash** (its PATH carries `/mingw64/bin`). **Do a build-validation spike first** (compile `whisper-rs` "hello world" locally and on CI) before writing feature code; if #599 bites, pin a known-good version, set `BINDGEN_EXTRA_CLANG_ARGS` to force MSVC targets, or commit Windows-target pre-generated bindings. CI (`windows-latest`) already has VS + CMake + LLVM and the workflow exports `LIBCLANG_PATH`, so **no new CI tools** are expected — only validation.
- **`cpal` input stream `Send`/threading.** The live `Stream` is `!Send` on some hosts and must not cross an `.await`. Keep it owned by a dedicated capture thread or a non-async handle; the worker only ever receives the finished `Vec<f32>`. This mirrors how the audio *output* stream is already managed (a plain std thread), so the pattern exists in-repo.
- **Default model size vs portable zip.** `ggml-small` adds ~466 MB to the **full** zip on top of Kokoro's ~310 MB. **Resolved:** the release already ships a full (with-models) zip and a slim (no-models) update zip, so size-sensitive users take the slim zip and drop in whatever `ggml-*.bin` they want. No download-default tradeoff needed. Still worth a one-line PACKAGING.md note that the full zip grew.
- **Engine reload on settings change.** Changing `model_file` / `language` / `translate` must reload/reconfigure the worker's `SttEngine`. Decide: rebuild the whole `WhisperContext` on model change (necessary — model is baked into the context) vs. just re-deriving per-call `FullParams` for language/translate (cheap, no reload). Implement accordingly; only `model_file` and `input_device` need heavyweight handling.
- **Resample quality vs latency.** `rubato` sinc resampling adds a few ms — negligible vs. the seconds of Whisper inference. Fine. If a future streaming mode lands, revisit (streaming wants a cheaper online resampler).
- **Transcript-append vs replace semantics.** This milestone appends with a single space and opens the sheet. If a user expects each dictation to replace the buffer, that's a future setting; appending is the safer default (never destroys typed text).
- **Mic permissions / no input device.** On a machine with no microphone (or a denied OS permission), `stt_start_recording` must fail cleanly into the `error` state with a toast, not panic. Add a device-presence check in `stt_list_input_devices` / `start`.

## Followups Tracked Elsewhere (FUTURE-FEATURES candidates)

- **Streaming / live partial transcripts** with VAD auto-stop — the natural next iteration once batch dictation proves useful.
- **Direct-to-PTY destination** as a setting (skip the compose review step).
- **LLM post-edit cleanup** of raw transcripts (punctuation, capitalization, removing filler) — note this *would* reintroduce a model/provider choice; keep offline if pursued.
- **Per-language model auto-selection** or a tiny+large two-tier model (fast draft, accurate finalize).
