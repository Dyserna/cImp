# Maintenance & Update Notes

Living list of dependencies and runtime concerns to revisit periodically. Each item: what to check, why it matters, where to look.

## Dependencies to track

### `ort` / ONNX Runtime — GPU TTS via the WebGPU EP (shipped); CUDA broken on Blackwell

- **Current pin:** `ort = "=2.0.0-rc.11"` (`src-tauri/Cargo.toml`), `features = ["download-binaries"]` + a per-build GPU feature (below). Wraps **ORT 1.20.x**. The optional `cuda` prebuilt is hard-linked to CUDA major 12 (`onnxruntime_providers_cuda.dll` references `cudart64_12.dll`, `cublas64_12.dll`, `cublasLt64_12.dll`, `cufft64_11.dll`, `cudnn64_9.dll`); CUDA 13.x won't load with this version.

- **IMPLEMENTED — `tts-webgpu` is the shipped GPU TTS backend.** Kokoro runs on ONNX Runtime's native **WebGPU EP** (Dawn-backed → D3D12 on Windows, Vulkan on Linux, Metal on macOS). Validated on the dev box (RTX 5090 / Blackwell) 2026-06-15: correct output matching the CPU reference, genuinely on-GPU (ORT node-placement logs show WebGPU shader programs for every op, incl. the `ConvTranspose2D` that broke DirectML), **~5× faster than CPU** at steady state. Wired in `tts/engine.rs` as GPU-by-default with automatic CPU fallback, `CCIMP_GPU=cpu` forces CPU — mirrors `stt/engine.rs`. Runtime deps: three Dawn dylibs (`webgpu_dawn.dll`, `dxcompiler.dll`, `dxil.dll`) staged into the zip by `release.yml`; `download-binaries` static-links core ONNX Runtime into `ccimp.exe` (no `onnxruntime.dll`). Full write-up: `docs/features/FEATURE-tts-webgpu.md`.

- **GPU backend is a compile-time feature; default is CPU.** Kokoro is near-real-time on CPU, so the default feature set has **no** GPU EP (routine `cargo build`/test/rust-analyzer pull the CPU-only ORT prebuilt, no GPU SDK). GPU is opt-in at build time, exactly mirroring STT:
  - **`tts-webgpu` (shipped, portable, any vendor)** — `["ort/webgpu"]`. The release builds `--features stt-vulkan,tts-webgpu`.
  - **`tts-cuda` (optional, NVIDIA-only, not shipped)** — `["ort/cuda"]`. **Mutually exclusive with `tts-webgpu`**: `ort` has no `cuda`+`webgpu` prebuilt, so enabling both silently downloads a CPU-only ORT. Broken on Blackwell (below).
  - DirectML was evaluated and rejected (Windows-only D3D12, and ORT 1.20's DML EP rejects Kokoro's `ConvTranspose`); the `directml` feature is not enabled.

- **Failure matrix** (investigated 2026-05-02 on RTX 5090, driver 596.21, CUDA toolkits 12.2 & 12.9, cuDNN 9.21):

  | EP | Failure | Root cause |
  |---|---|---|
  | CUDA | `cudaErrorSymbolNotFound` on every kernel (Slice, Split, …) | RTX 5090 is Blackwell (sm_120), released **after** ORT 1.20. The prebuilt CUDA EP has no cubin for sm_120; JIT from PTX targeting older arches fails to resolve device symbols on Blackwell. **Toolkit version is irrelevant** — reproduced on both 12.2 and 12.9. |
  | DirectML | `ConvTranspose` E_INVALIDARG (0x80070057) on `/encoder/F0.1/pool/ConvTranspose` | ORT 1.20's DML EP rejects Kokoro's F0-decoder ConvTranspose parameters. No useful config knob; not GPU-specific (DML is broken for this model on any DX12 GPU). |
  | CPU | works | — |

- **Why the failure matrix no longer bites us:** `tts-webgpu` sidesteps both broken EPs — it runs on Blackwell where the CUDA prebuilt can't, and it runs the `ConvTranspose` that DirectML rejects. The matrix above is retained as the rationale for *why* WebGPU is the shipped path. The optional `tts-cuda` build still inherits the CUDA row's Blackwell breakage (per-segment `cudaErrorSymbolNotFound`, silent output) — it's expected to work only on Pascal..Ada, which is why it's neither default nor shipped. See `FEATURE-gpu-robustness.md` for the (still-relevant) CC pre-flight idea for `tts-cuda` users.

- **What to check for on `ort` updates:**
  - The WebGPU EP is flagged **experimental** upstream. On an `ort` bump, re-run the `tts-webgpu` smoke test (`cargo test --features tts-webgpu --bin ccImp -- --ignored --nocapture synthesizes`) to confirm Kokoro still produces correct audio and stays on-GPU. Watch <https://github.com/pykeio/ort/releases> and <https://crates.io/crates/ort>.
  - A newer `ort` wrapping ORT 1.21+ may fix the CUDA EP for Blackwell (1.21 adds sm_120 cubins) — relevant only for the optional `tts-cuda` build.
  - Watch whether the Dawn dylib set (`webgpu_dawn.dll`/`dxcompiler.dll`/`dxil.dll`) changes — if so, update the staging list in `release.yml` (both zip variants) and the layout in `PACKAGING.md`.
  - Upstream ORT release notes: <https://github.com/microsoft/onnxruntime/releases>.

- **Open follow-ups (not blocking):** validate `tts-webgpu` on a non-NVIDIA GPU (AMD/Intel) when one is available; the cold-start one-time Dawn shader-compile cost (~1.3 s on first synth, paid once by the long-lived engine); and surfacing the active TTS backend in the UI (currently log-only, matching STT — see `FEATURE-tts-webgpu.md` Phase 4). Cross-platform/Linux rationale and the "STT stays on whisper.cpp — do NOT unify runtimes yet" decision live in `FUTURE-FEATURES.md`.

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
- **GPU backends are compile-time features; the DEFAULT feature set is empty
  (CPU).** So routine `cargo build`/`cargo test` and rust-analyzer work from a
  plain shell with no GPU SDK / dev-env / generator requirements. GPU is opt-in:
  - **`stt-vulkan` (the release backend, recommended).** whisper.cpp's Vulkan
    backend. Produces a **portable** binary — the only GPU runtime dep is the
    system `vulkan-1.dll` (on every Win10+) — runs on any vendor's GPU and
    falls back to CPU when none is present. `release.yml` builds the zip with
    this, so end users get auto GPU/CPU with nothing bundled.
  - **`stt-cuda` (optional, NVIDIA-only).** ~20-40% faster than Vulkan but not
    portable (imports `cublas64_*.dll`) and build-heavy — see the CUDA note
    below. For local NVIDIA max-perf only; not shipped.
  - Runtime (`stt/engine.rs`): when a GPU backend is compiled, STT uses the GPU
    by default and **falls back to CPU automatically** if GPU init fails or no
    GPU is present (this is what makes the Vulkan binary universal).
    `CCIMP_GPU=cpu` forces CPU.

- **Building `--features stt-vulkan` (the saga — three Windows gotchas):**
  1. **Vulkan SDK** (LunarG) provides `glslc` + headers + `vulkan-1.lib`.
     `VULKAN_SDK` is pinned in `.cargo/config.toml` (the installer also sets it
     machine-wide). Pinned version: `C:\VulkanSDK\1.4.350.0` — bump on upgrade.
  2. **MSVC dev environment + Ninja generator.** ggml-vulkan builds its shader
     generator as a nested CMake *ExternalProject*. The VS CMake generator does
     NOT propagate the compiler into that sub-build (`No CMAKE_C_COMPILER`), so
     force `CMAKE_GENERATOR=Ninja` and build with `cl.exe` on PATH (a VS x64
     Native Tools prompt, or `vcvars64.bat` sourced). `CL=/FS` serializes PDB
     writes. NOTE these are env-only and intentionally NOT in `.cargo/config.toml`
     (that would force every CPU build through Ninja+dev-env too).
  3. **MAX_PATH on a deep repo.** The nested shader-gen path is ~264 chars from
     this repo's deep location and `cl` fails (`C1041`) even with
     `LongPathsEnabled=1`. Local fix: build with a short `CARGO_TARGET_DIR`
     (e.g. `C:\ct`). CI is unaffected — the runner path (`D:\a\ccImp\ccImp`) is
     short enough. Validated 2026-06-14: with all three, a local Vulkan build
     produces a clean binary importing **only** `vulkan-1.dll` (no CUDA DLLs).
- **CI (`release.yml`):** a `Setup MSVC dev environment` step (`ilammy/msvc-dev-cmd`)
  + an `Install Vulkan SDK` step (LunarG silent installer, sets `VULKAN_SDK` /
  PATH), then the build sets `CMAKE_GENERATOR=Ninja` + `CL=/FS` and runs
  `--features stt-vulkan`. If CI ever hits the MAX_PATH wall, add a short
  `CARGO_TARGET_DIR` and update the staging-copy paths.

- **Optional CUDA path (`--features stt-cuda`) — kept for local NVIDIA only:**
  `nvcc` gates the MSVC host version in `crt/host_config.h`. This box has only
  MSVC 14.50 (VS 2026, `_MSC_VER` 1950); CUDA 12.x rejects `>=1950`, **CUDA 13.2
  accepts** (`<1960`). So a CUDA build must use 13.2, and **CUDA 13.2's `bin`
  must be the first CUDA dir on PATH** (the VS-generator MSBuild CUDA
  integration injects an include path from the first CUDA bin; a 12.x there
  pulls its rejecting header even when nvcc is 13.2). That PATH entry also
  supplies the load-time `cublas64_13.dll`. Auto-detects `sm_120a` (the 5090's
  Blackwell arch — works where `ort`/Kokoro's prebuilt CUDA can't). This is why
  `stt-cuda` is NOT the default or shipped: too much setup, not portable.
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
