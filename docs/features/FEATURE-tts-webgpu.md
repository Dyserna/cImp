# Feature: Portable GPU TTS via the ONNX Runtime WebGPU EP

> **Status: IMPLEMENTED (2026-06-15, branch `spike/tts-webgpu`).** Phase 0 spike
> passed (results below); Phases 1–3 + 5 are done — `tts-webgpu` / `tts-cuda`
> Cargo features, the GPU-default-with-CPU-fallback engine wiring, the
> `release.yml` build + Dawn-dylib staging, and the doc updates. Phase 4 was
> deliberately scoped to **log-only** (the active backend is logged at startup,
> matching STT) — a UI indicator is deferred as a joint STT+TTS enhancement.
> The phase sections below are kept as the design record.

## Purpose

Replace the NVIDIA-only, Blackwell-broken `CCTTS_GPU=cuda` TTS path with a
**portable, any-vendor GPU backend** for Kokoro — the **ONNX Runtime WebGPU
execution provider** (native, non-browser; Dawn-backed → D3D12 on Windows,
**Vulkan on Linux**, Metal on macOS). End state mirrors what `stt-vulkan`
already gives STT: one binary that uses the GPU on any vendor (NVIDIA / AMD /
Intel) and falls back to CPU automatically when there's no usable GPU, with
nothing CUDA-specific bundled.

This is the TTS half of the larger "unify on Vulkan under Linux" direction.
The STT half is explicitly **not** changing — see `FUTURE-FEATURES.md`
§ "Unify TTS and STT on one inference runtime — DECIDED: not now". STT stays on
whisper.cpp/ggml-Vulkan; only TTS moves.

## Why this and not DirectML / CUDA

- **CUDA** (today's opt-in): NVIDIA-only, and the bundled ORT 1.20 prebuilt has
  no sm_120 cubin so it's silently broken on Blackwell (RTX 5090). See
  `MAINTENANCE.md` "ort / ONNX Runtime".
- **DirectML**: vendor-agnostic but **Windows-only** (D3D12) — a dead end for
  the planned Linux port. Deprecated as a direction.
- **WebGPU EP**: vendor-agnostic *and* cross-platform with one backend. Kokoro
  is already an ONNX graph, so this is an EP swap, not a model port.

## Current state (what we're changing)

- `src-tauri/src/tts/engine.rs` — `TtsEngine::new` builds an `ort::Session` and
  registers an EP from a `match` on `CCTTS_GPU`: `"cuda"` → `CUDAExecutionProvider`,
  everything else → `CPUExecutionProvider`. Each arm already falls back to CPU on
  registration failure and sets a `bound_ep` label string for logging. The new
  WebGPU arm slots into this exact pattern.
- `src-tauri/Cargo.toml` — `ort = { version = "=2.0.0-rc.11", features =
  ["download-binaries", "cuda"] }`. No TTS GPU Cargo feature today (CUDA is
  always compiled in; GPU is purely a runtime env opt-in).
- STT, by contrast, gates its GPU backend at **compile time** (`stt-vulkan` /
  `stt-cuda` features) and defaults to GPU-on-with-CPU-fallback at runtime. The
  end state here should match STT's model for consistency.

## Key upstream facts (verified June 2026)

- The native WebGPU EP **is exposed in `ort` 2.0.0-rc** already (we're at rc.11;
  rc.12 is latest). Not blocked on a future release — but `ort` flags it
  **experimental** ("may produce incorrect results/crashes").
- **`download-binaries` ships WebGPU prebuilts for Windows / macOS / Linux**,
  including **Dawn helper dylibs** that must sit beside the binary at runtime.
  `ort`'s `copy-dylibs` feature (on by default) stages them to the target dir;
  the release zip must then include them (same shape as the espeak-ng-data /
  build.rs copy story).
- **`cuda` + `webgpu` cannot share one prebuilt** — `features = ["cuda",
  "webgpu"]` silently downloads a **CPU-only** build. A single binary with both
  needs ORT compiled from source. So shipping WebGPU means **dropping the CUDA
  opt-in** from the same binary (keep it only as a separate optional build, like
  `stt-cuda`).
- Registration API is the same `.register(&mut builder)` used by the existing
  CUDA/CPU arms.

## Phase 0 RESULT — PASSED (2026-06-15, branch `spike/tts-webgpu`)

Spike ran on the dev box (RTX 5090 / Blackwell — the exact GPU CUDA can't drive).
Harness: an `#[ignore]`d unit test in `tts/engine.rs` (`spike::webgpu_synthesizes`)
that swaps the ort feature `cuda`→`webgpu`, registers the WebGPU EP by default,
and synthesizes a real phrase. Verdict: **green on all three gate questions.**

- **Correctness** — output matches the CPU reference: WebGPU peak 0.6387 / rms
  0.0702 vs CPU peak 0.6433 / rms 0.0702 for the same 4.35 s utterance. Not just
  "some audio" — the *right* audio.
- **Genuinely on GPU (not silent CPU fallback)** — ORT debug logs show Dawn
  WebGPU compute shader programs for every op: `Conv2dMM`, `ConvTranspose2D`,
  `LeakyRelu`, `ReduceMean`, `Transpose`, `Gather`, `ScatterND`, …
- **The DirectML-killer op works** — `ConvTranspose2D` (Kokoro's F0 decoder, the
  op that threw `E_INVALIDARG` on the DML EP) executes on WebGPU with no error.
- **Latency** — steady-state (warm) synth **~125 ms** on WebGPU vs **~680 ms** on
  CPU = **~5.4× faster** (~35× real-time). First synth is ~1.3 s due to one-time
  Dawn shader compilation; the long-lived engine pays that once at startup.
- **Build/packaging** — `features = ["download-binaries", "webgpu"]` resolved and
  pulled the prebuilt + Dawn dylibs cleanly; no Vulkan SDK / Ninja / source build
  needed (unlike `stt-vulkan`). Registration is the same `.register(&mut builder)`
  shape as the existing CUDA arm. Default Windows Dawn backend (D3D12) was used;
  `with_dawn_backend_type(Vulkan)` is available for the Linux path.

**Conclusion: proceed to Phases 1–5 (productionize).** The experimental-EP risk
did not materialize for Kokoro on this hardware. (Caveats still to settle in
productionization: confirm on a non-NVIDIA GPU when available; the cold-start
shader-compile cost; shipping the Dawn dylibs in the release zip.)

---

## Phase 0 — Validation spike (THE GATE, do this first)

Everything else is mechanical; this phase decides whether the feature is viable
at all. The risk is identical to what killed DirectML for Kokoro: the
`ConvTranspose` (F0 decoder) and any `STFT` ops may be unsupported on the
WebGPU EP and either error or silently fall back to CPU.

1. On a throwaway branch, swap the ort feature set: `features =
   ["download-binaries", "webgpu"]` (drop `cuda` for the spike — they can't
   coexist in the prebuilt).
2. Add a WebGPU EP registration arm (see Phase 2) wired so the spike build
   always tries WebGPU.
3. Run Kokoro end-to-end on representative text (include a phrase that exercises
   the F0/ConvTranspose path — any normal sentence does). Confirm **all three**:
   - **Correctness** — audio matches the CPU output (not garbage / not silence).
   - **Actually on GPU** — verify it did *not* silently fall to CPU. Enable ORT
     verbose EP logging and/or watch GPU utilization (`nvml`/Task Manager) during
     synth. Silent CPU fallback is the main trap.
   - **Latency** — at least not worse than CPU; ideally a win on longer text.
4. **Decision:**
   - Correct + GPU-bound → proceed to Phase 1.
   - `ConvTranspose`/`STFT` errors, garbage audio, or silent CPU fallback → STOP.
     Record the failing op + ORT version in `MAINTENANCE.md`, revert, and treat
     this as gated on EP maturity (re-spike on the next `ort` bump). This is a
     real possible outcome given the EP is experimental.

## Phase 1 — Cargo feature & dependency (after the gate passes)

Mirror the STT pattern for consistency:

- Add `tts-webgpu = ["ort/webgpu"]` and make the base `ort` dep stop hard-wiring
  `cuda`. Keep CUDA available as an optional, non-default `tts-cuda =
  ["ort/cuda"]` (NVIDIA-only fast path, not shipped — exact analog of `stt-cuda`).
- `download-binaries` stays always-on.
- **Document loudly** (Cargo.toml comment + MAINTENANCE.md): never enable
  `tts-cuda` and `tts-webgpu` together — Cargo feature unification would pull the
  CPU-only prebuilt and silently disable the GPU. They are mutually exclusive
  build configs.
- Release feature set becomes `stt-vulkan,tts-webgpu` → a fully portable
  any-vendor GPU zip for **both** subsystems.

## Phase 2 — Engine wiring (`tts/engine.rs`)

- Switch TTS from "runtime env opt-in only" to STT's **compile-time backend +
  GPU-by-default** model:
  - When `tts-webgpu` is compiled: default to registering the WebGPU EP; on
    registration failure (or `CCTTS_GPU=cpu`) fall back to CPU. This makes the
    shipped binary "GPU when present, CPU otherwise" with zero config — matching
    STT.
  - When `tts-cuda` is compiled (optional builds only): keep the explicit
    `CCTTS_GPU=cuda` opt-in (CUDA stays opt-in because it's non-portable and
    Blackwell-broken).
  - Plain build (no TTS GPU feature): CPU only, as today.
- Add the WebGPU arm using the existing fallback-to-CPU pattern:
  ```rust
  // sketch — adapt to the rc.11/rc.12 API surface
  match WebGPUExecutionProvider::default().register(&mut builder) {
      Ok(()) => "GPU (WebGPU)",
      Err(e) => { tracing::warn!(error = %e, "WebGPU EP unavailable; CPU"); /* register CPU */ "CPU" }
  }
  ```
- Update the `bound_ep` label set to include `"GPU (WebGPU)"`; keep it flowing to
  the existing startup log line.
- **Guard against silent CPU fallback.** The WebGPU EP can register "successfully"
  yet run ops on CPU. If `ort` exposes a way to confirm the bound EP / per-node
  placement, log it; otherwise document that the Phase 0 GPU-utilization check is
  the verification of record and add a one-line warning if init is suspiciously
  fast.

## Phase 3 — Packaging & build (`build.rs`, `release.yml`)

- **Dawn dylibs in the zip.** Ensure the WebGPU helper dylibs `copy-dylibs`
  stages next to the binary are included in the release staging copy (parallel to
  the existing `espeak-ng-data/` copy in `build.rs`). Enumerate exactly which
  files they are and add them to the packaging manifest / `PACKAGING.md`.
- **`release.yml`** builds with `--features stt-vulkan,tts-webgpu`. Unlike the
  `stt-vulkan` build, WebGPU is a **prebuilt** — no Vulkan SDK / Ninja / MAX_PATH
  gymnastics for the TTS side (one of the wins). Confirm the combined-feature
  build still pulls correct prebuilts and doesn't trip the cuda+webgpu CPU-only
  fallback (it won't, since `tts-cuda` isn't in the release set).
- Re-check binary size and `NOTICE`/licensing for the bundled Dawn artifacts.

## Phase 4 — Runtime & UX

- **DONE — `CCTTS_GPU` semantics reconciled.** `CCTTS_GPU=cpu` now forces CPU for
  **both** TTS and STT; the old TTS-only `CCTTS_GPU=cuda` runtime opt-in is gone
  (CUDA is now the compile-time `tts-cuda` feature). The GPU backend is otherwise
  on-by-default-with-CPU-fallback, identical to `stt/engine.rs`.
- **DEFERRED (scoped to log-only) — UI backend indicator.** The active TTS backend
  is logged at startup ("TTS engine ready bound=GPU (WebGPU)"), matching STT,
  which is also log-only. A bottom-bar "GPU/CPU" indicator was intentionally NOT
  added here: doing it for TTS alone would be asymmetric with STT. The right shape
  is a single combined STT+TTS GPU-status indicator, tracked as a separate
  enhancement (would reuse the `FEATURE-gpu-robustness.md` messaging).

## Phase 5 — Docs

- `MAINTENANCE.md` — mark WebGPU as the shipped TTS GPU path; record the working
  `ort`/ORT version, the Phase 0 op-coverage result, and the exact Dawn dylib
  list. Retire the CUDA-as-default framing.
- `FUTURE-FEATURES.md` — move the "Portable GPU TTS via the ONNX Runtime WebGPU
  EP" entry to § 3 (Done / historical). Leave the "do not unify STT yet" decision
  entry in place.
- `PACKAGING.md` — add the Dawn dylibs to the shipped-files list.

## Risks & open questions

- **Experimental EP (primary risk).** Correctness/crash/silent-fallback on
  Kokoro's ops. Phase 0 is the gate; if it fails, the whole feature waits for EP
  maturity. Re-spike on each `ort` bump.
- **Silent CPU fallback** masking a non-functional GPU path — mitigated by the
  Phase 0 utilization check; ongoing verification story is an open question if
  `ort` doesn't expose bound-EP introspection.
- **cuda+webgpu prebuilt conflict** — resolved by dropping CUDA from the shipped
  binary; CUDA survives only as a separate optional build. Anyone wanting both in
  one binary needs a source build (out of scope).
- **Linux** — not validated now (Linux is deferred per project decision), but
  this feature is precisely what makes Linux GPU TTS possible later. No Linux
  testing required for the Windows ship; the Vulkan-via-Dawn path is the payoff
  when Linux is picked up.
- **Windows GPU API choice** — Dawn may pick D3D12 over Vulkan on Windows; fine
  (still vendor-agnostic). Only worth forcing Vulkan if a concrete reason appears.

## Files most likely touched

- `src-tauri/src/tts/engine.rs` — WebGPU EP arm, compile-time-feature + GPU-default model, `bound_ep` label.
- `src-tauri/Cargo.toml` — `tts-webgpu` / `tts-cuda` features; drop hard-wired `ort/cuda`.
- `src-tauri/build.rs` — stage Dawn dylibs next to the exe.
- `.github/workflows/release.yml` — build `--features stt-vulkan,tts-webgpu`; stage dylibs.
- Status-indicator frontend + `CCTTS_GPU` handling — backend label + unified env semantics.
- `docs/MAINTENANCE.md`, `docs/FUTURE-FEATURES.md`, `docs/PACKAGING.md` — as above.

## Sequencing

Phase 0 first and alone — it's a few hours and it decides everything. Only if it
passes do Phases 1–5 follow (roughly one focused PR, since the wiring mirrors the
existing CUDA arm and the STT feature-gating pattern). Do **not** touch STT as
part of this work.
