# Maintenance & Update Notes

Living list of dependencies and runtime concerns to revisit periodically. Each item: what to check, why it matters, where to look.

## Dependencies to track

### `ort` / ONNX Runtime — GPU EPs are unusable for Kokoro on this dev box

- **Current pin:** `ort = "=2.0.0-rc.11"` (`src-tauri/Cargo.toml`), which wraps **ORT 1.20.x** prebuilt against **CUDA 12.x + cuDNN 9**. Hard-linked to CUDA major 12: the bundled `onnxruntime_providers_cuda.dll` references `cudart64_12.dll`, `cublas64_12.dll`, `cublasLt64_12.dll`, `cufft64_11.dll`, and `cudnn64_9.dll` directly. CUDA 13.x will not load at all with this version.

- **Default runtime is CPU.** Kokoro is small enough to run near real-time on CPU. GPU EPs are opt-in via `CCTTS_GPU=cuda` env var. DirectML support was removed from the build (the `directml` ort feature is **not** enabled in `Cargo.toml`).

- **Failure matrix** (investigated 2026-05-02 on RTX 5090, driver 596.21, CUDA toolkits 12.2 & 12.9, cuDNN 9.21):

  | EP | Failure | Root cause |
  |---|---|---|
  | CUDA | `cudaErrorSymbolNotFound` on every kernel (Slice, Split, …) | RTX 5090 is Blackwell (sm_120), released **after** ORT 1.20. The prebuilt CUDA EP has no cubin for sm_120; JIT from PTX targeting older arches fails to resolve device symbols on Blackwell. **Toolkit version is irrelevant** — reproduced on both 12.2 and 12.9. |
  | DirectML | `ConvTranspose` E_INVALIDARG (0x80070057) on `/encoder/F0.1/pool/ConvTranspose` | ORT 1.20's DML EP rejects Kokoro's F0-decoder ConvTranspose parameters. No useful config knob; not GPU-specific (DML is broken for this model on any DX12 GPU). |
  | CPU | works | — |

- **CUDA opt-in is left available** because the same `cuda` ort feature is expected to work on Pascal/Volta/Turing/Ampere/Ada NVIDIA cards (anything with a cubin shipped in ORT 1.20). Users on those cards can `setx CCTTS_GPU cuda` and restart. On Blackwell, expect per-segment `cudaErrorSymbolNotFound` errors and silent output — see FEATURES.md for the auto-detection enhancement.

- **What to check for on `ort` updates:**
  - A newer `ort` release wrapping ORT 1.21+ — likely fixes both EPs at once: 1.21 adds Blackwell sm_120 cubins to the CUDA prebuilt, and updates DML's ConvTranspose validator. Watch <https://github.com/pykeio/ort/releases> and <https://crates.io/crates/ort>.
  - Upstream ORT release notes: <https://github.com/microsoft/onnxruntime/releases>.

- **When to act:** any time `ort` bumps. After bumping: re-test `CCTTS_GPU=cuda` on the 5090 (expect Blackwell to work in 1.21+); re-test DirectML by re-adding `directml` to ort features and registering it; if DML works for Kokoro again, consider making it the default GPU path on Windows since it's vendor-agnostic.

### `whisper-rs` / whisper.cpp — STT build toolchain (V6-01)

- **Current pin:** `whisper-rs = "0.16"` (→ `whisper-rs-sys 0.15.0`) + `rubato = "0.16"`.
- **Build needs a C/C++ toolchain + CMake.** `whisper-rs-sys` compiles
  whisper.cpp from source via the `cc` + `cmake` crates and generates FFI
  bindings with `bindgen` (libclang). On this Windows dev box that means:
  MSVC (`cl.exe`, auto-found by `cc`), **CMake on PATH** (VS bundles 4.2.3 +
  Ninja), and `libclang` at `C:\Program Files\LLVM\bin` — already pinned via
  `src-tauri/.cargo/config.toml`'s `LIBCLANG_PATH` (shared with misaki/espeak).
  No new CI tools: `windows-latest` already has VS + CMake + LLVM and the
  workflow exports `LIBCLANG_PATH`.
- **Known pitfall (bindgen on MSVC):** `whisper-rs-sys` bindgen can emit glibc
  types and fail with a `usize` overflow when it sees MinGW/MSYS headers.
  **Build from PowerShell or the VS x64 Native Tools prompt, never Git Bash**
  (Git Bash's PATH carries `/mingw64/bin`). Validated 2026-06-14: clean build
  from PowerShell, no #599 recurrence. If it bites on a bump: pin a
  known-good version, set `BINDGEN_EXTRA_CLANG_ARGS` to force the MSVC target,
  or commit Windows-target pre-generated bindings.
- **GPU is a compile-time feature, ON BY DEFAULT (`stt-cuda`).** whisper.cpp's
  CUDA backend is compiled by `nvcc`. Local dev builds get GPU; releases/CI
  pass `--no-default-features` for a portable CPU binary (see below). Runtime
  uses GPU by default with automatic CPU fallback (`stt/engine.rs`);
  `CCTTS_GPU=cpu` forces CPU. Unlike `ort` there is no prebuilt-download path —
  STT GPU and TTS GPU are independent.
- **CUDA version vs MSVC host-compiler gate (the key constraint here).** `nvcc`
  rejects host compilers newer than it supports (`crt/host_config.h`). This box
  has **only** MSVC 14.50 (VS 2026, `_MSC_VER` 1950) and no VS 2022 toolset.
  CUDA 12.x rejects 1950 (`_MSC_VER >= 1950`); **CUDA 13.2 accepts it**
  (`_MSC_VER < 1960`). So the GPU build **must** use CUDA 13.2, not 12.x.
- **`.cargo/config.toml` pins CUDA_PATH + CUDACXX to v13.2** (`force = true`,
  build-time only) — the machine's default `CUDA_PATH` is 12.9. But that alone
  is **not sufficient**: with the VS CMake generator, MSBuild's CUDA
  integration injects an include path from the **first CUDA `bin` on PATH**, so
  a 12.x there pulls in its rejecting `host_config.h` even when nvcc is 13.2.
- **CUDA 13.2's `bin` MUST be the first CUDA directory on PATH** (ahead of any
  12.x). This one PATH entry fixes the build's include injection AND supplies
  `cublas64_13.dll` — a **load-time** dependency of the GPU binary (cudart is
  static-linked). Without it the process won't even launch, and that loader
  failure happens before any Rust runs, so the runtime CPU-fallback can't catch
  it. Validated 2026-06-14: with 13.2 ahead of 12.2 on PATH, a bare
  `cargo build` produces a clean GPU binary, auto-detecting `sm_120a` (the RTX
  5090's Blackwell arch — the same arch `ort`/Kokoro's prebuilt CUDA can't
  target, so STT GPU works on Blackwell where TTS GPU doesn't). With 12.x first,
  the build fails with ~250 `host_config.h` "unsupported Microsoft Visual
  Studio version" errors.
- **Releases stay CPU-only.** `release.yml` builds with `--no-default-features`:
  GitHub `windows-latest` has no CUDA toolkit, and a CUDA binary isn't portable
  (the `cublas64_13.dll` dependency). The full/slim zips therefore ship a
  CPU-only `cctts.exe`; GPU STT is a local-build feature.
- **What to check on CUDA bumps:** when a CUDA release adds VS 2026 (`_MSC_VER`
  1950+) support, 12.x could work again; if VS bumps past 1959, re-point
  `CUDA_PATH` at a newer CUDA. To verify the gate: grep the chosen CUDA's
  `include/crt/host_config.h` for the `_MSC_VER` bounds.
- **What to check on bumps:** the `whisper-rs` API has shifted across releases
  (e.g. segment text moved to `WhisperState::get_segment(i).to_str_lossy()` in
  0.16). Re-verify `FullParams` / `WhisperContextParameters` / `WhisperState`
  against `src/stt/engine.rs` when bumping. Watch
  <https://github.com/tazz4843/whisper-rs/releases>.

## Known runtime issues to revisit

### Spurious `[[TTS]] tag exceeded max-hold without close` warnings

- **Symptom:** `WARN tts_stub: [[TTS]] tag exceeded max-hold without close; treating as literal` fires at runtime, sometimes in clusters around tab switches. The opener was held for ≥500ms (`processing.max_hold_ms`) without seeing a close, so it gets flushed as literal terminal bytes — the user sees `[[TTS]]` in the terminal and that segment is never spoken.
- **Suspected causes (not yet narrowed down):**
  - TUI redraws inside Claude Code that produce partial content matching the tag-opener prefix (`[`, `[[`, `[[T`…) which the scanner holds while waiting for the rest. If the TUI rewrites that region before the close arrives, the held content is stale.
  - Slow streaming bursts where the genuine tag content takes longer than 500ms to arrive (model latency + network jitter).
  - Pre-existing in v1; tab switches make it more visible because users notice the warnings while context-switching, not because the switch itself causes them.
- **Where to look:** `src-tauri/src/processing/{mod.rs,screen.rs,tags.rs}` — specifically `ProcessingLayer::collect_events` and `Screen::drain_flushable`. The 500ms threshold is `DEFAULT_MAX_HOLD` in `processing/mod.rs`, runtime-configurable via `processing.max_hold_ms`.
- **Possible fixes when investigated:**
  - Bump `max_hold_ms` default to 1000–2000ms. Trade-off: slower display of any prose that contains `[` characters in non-TTS context.
  - Distinguish "opener seen but no further bytes for N ms" (scanner-side timeout) from "opener seen, more bytes arriving but no close yet" (held content is still growing). Only force-flush in the first case; let the second continue holding.
  - Capture a real reproducer (e.g. a tcpdump-style log of raw PTY bytes when this fires) to confirm which trigger is actually responsible before tuning.
- **When to act:** if users start reporting visible `[[TTS]]` text in the terminal, OR if the warning rate becomes high enough to clutter logs in normal operation.
