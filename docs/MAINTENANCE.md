# Maintenance & Update Notes

Living list of dependencies and runtime concerns to revisit periodically. Each item: what to check, why it matters, where to look.

---

# Dependency & component inventory

A complete, scannable inventory of everything cImp depends on, for periodic
"is there a newer version?" passes. Version columns reflect the pins in
`src-tauri/Cargo.toml` and `package.json` as of **v0.19.0**. The deeper
"Dependencies to track" sections below cover the *gotchas* for the hairy ones
(`ort`, `whisper-rs`, offload) — this inventory is the breadth; those are the
depth. Update both when you bump.

## How to check for updates

```bash
# Rust crates — shows newest compatible + newest available per dep
cargo install cargo-outdated        # one-time
cd src-tauri && cargo outdated -R   # -R = root deps only (skip transitive noise)
cargo update --dry-run              # what a `cargo update` would move (semver-compatible)

# Frontend (npm)
npm outdated                        # current vs wanted vs latest
npx npm-check-updates               # proposes package.json bumps (review, don't blind-apply)

# Tauri toolchain sanity
cargo tauri --version ; node --version ; rustc --version
```

Bump policy: move one ecosystem at a time, run `cargo test` + `npm run check` +
`npm test`, and for `ort` / `whisper-rs` / GPU features re-run the smoke tests
called out in their sections. Pinned-exact deps (`ort = "=…"`) need a manual
version edit — `cargo update` will not move them.

## Rust crates (`src-tauri/Cargo.toml`)

| Crate | Pin | Role | Watch / notes |
|---|---|---|---|
| `tauri` | `2` | App shell / IPC / windowing | Bump with `@tauri-apps/*` JS + `tauri-plugin-dialog` together (same major). |
| `tauri-build` (build-dep) | `2` | Tauri codegen | Keep in lockstep with `tauri`. |
| `tauri-plugin-dialog` | `2` | Native file dialogs | Pairs with the JS `@tauri-apps/plugin-dialog`. |
| `serde` / `serde_json` | `1` / `1` | (De)serialization everywhere | Stable; rarely needs attention. |
| `tokio` | `1` | Async runtime | Feature-rich pin (`rt-multi-thread,macros,sync,io-*,time,fs,process,net`). |
| `tokio-util` | `0.7` | `rt` helpers | — |
| `portable-pty` | `0.8` | PTY for the embedded terminals | Pre-1.0; check changelog on bump. |
| `thiserror` | `1` | Error derives | — |
| `tracing` / `tracing-subscriber` / `tracing-appender` | `0.1` / `0.3` / `0.2` | Logging + rolling file logs | `env-filter` feature; subscriber API shifts across 0.3.x. |
| `base64` | `0.22` | Asset/data encoding | — |
| `which` | `6` | Locate `claude` / shells on PATH | — |
| `vte` | `0.13` | ANSI/terminal parser (processing layer) | Pre-1.0; tag-scanner depends on its escape parsing. |
| `shlex` | `1` | Shell-word splitting | — |
| `uuid` | `1` | IDs (`v4`) | — |
| `async-trait` | `0.1` | Async `ToolRouter` trait (offload) | — |
| `reqwest` | `0.12` | Usage tracker + offload HTTP | `rustls-tls` only (no system OpenSSL — keeps single-binary). |
| `sysinfo` | `0.36` | System-monitor panel | Fast-moving pre-1.0; API churns — re-verify the monitor on bump. |
| `nvml-wrapper` | `0.12` | NVIDIA GPU stats | Loads `nvml.dll` at runtime; degrades to n/a without driver. |
| `misaki-rs` | `0.3` | TTS G2P phonemizer | **Pulls espeak-ng → binary is GPLv3** (see `NOTICE`); needs libclang to build. |
| `ort` | `=2.0.0-rc.11` | ONNX Runtime bindings (Kokoro TTS) | **Exact-pinned.** Wraps ORT 1.20.x. See the deep `ort` section. |
| `ndarray` | `0.16` | Tensor math for TTS pre/post | Keep aligned with what `ort` expects. |
| `bytemuck` | `1` | Zero-copy casts | — |
| `cpal` / `rodio` | `0.15` / `0.20` | Audio output | Pre-1.0; device-enumeration behavior changes across versions. |
| `whisper-rs` | `0.16` | STT (whisper.cpp bindings) | → `whisper-rs-sys 0.15`. See the deep `whisper-rs` section + build toolchain. |
| `rubato` | `0.16` | Mic resample → 16 kHz mono | — |
| `tree-sitter` | `0.26.9` | Code-graph parsing core | Grammar crates ride the `tree-sitter-language` shim, so they need not match this exactly — only the parser ABI must. |
| `tree-sitter-rust` | `0.24.2` | Rust grammar | — |
| `tree-sitter-typescript` | `0.23.2` | TS/TSX grammar | — |
| `tree-sitter-javascript` | `0.25.0` | JS grammar | — |
| `tree-sitter-python` | `0.25.0` | Python grammar | — |
| `cozo` | `0.7.6` | Embedded graph DB (code-knowledge graph) | `default-features = false` + `storage-sqlite,rayon`. **Deliberately omits** `graph-algo` (broken `graph_builder` vs rayon) and `storage-rocksdb` (heavy C++). |
| `ignore` | `0.4` | Gitignore-aware tree walk (indexer) | ripgrep's walker. |
| `notify` | `6` | FS watcher (incremental re-index) | `ReadDirectoryChangesW` on Windows. |
| `winreg` *(windows only)* | `0.52` | Registry probe for Git Bash detection | `cfg(windows)` target dep. |

## Frontend / npm (`package.json`)

| Package | Pin | Role |
|---|---|---|
| `@tauri-apps/api` | `^2.1.1` | JS ↔ Rust IPC bridge |
| `@tauri-apps/plugin-dialog` | `^2.0.0` | Dialog plugin (JS half) |
| `@xterm/xterm` | `^5.5.0` | Terminal emulator widget |
| `@xterm/addon-canvas` | `^0.7.0` | xterm canvas renderer |
| `@xterm/addon-fit` | `^0.10.0` | xterm fit-to-container |
| `@xterm/addon-serialize` | `^0.13.0` | xterm scrollback serialization |
| `@sveltejs/vite-plugin-svelte` *(dev)* | `^4.0.0` | Svelte + Vite glue |
| `@tauri-apps/cli` *(dev)* | `^2.1.0` | `tauri` build/dev CLI |
| `@tsconfig/svelte` *(dev)* | `^5.0.4` | TS base config |
| `svelte` *(dev)* | `^5.1.9` | UI framework (Svelte 5 / runes) |
| `svelte-check` *(dev)* | `^4.0.5` | Svelte type-check |
| `tslib` *(dev)* | `^2.8.0` | TS runtime helpers |
| `typescript` *(dev)* | `^5.6.3` | Type system |
| `vite` *(dev)* | `^5.4.10` | Bundler / dev server |
| `vitest` *(dev)* | `^4.1.5` | Test runner |

Keep `@tauri-apps/api` + `@tauri-apps/plugin-dialog` + `@tauri-apps/cli` aligned
with the Rust `tauri` major. Svelte 5, Vite 5, and Vitest 4 are majors — read
migration notes before bumping any of them.

## Native libraries linked/vendored through crates

Not separately installable — they ride in via a crate, but each has its own
upstream cadence worth watching.

| Component | Comes via | Effective version | Watch |
|---|---|---|---|
| ONNX Runtime | `ort = =2.0.0-rc.11` | **1.20.x** (static-linked) | <https://github.com/microsoft/onnxruntime/releases> — 1.21+ may fix CUDA-on-Blackwell. |
| Dawn / WebGPU EP dylibs | `ort/webgpu` prebuilt | tracks the `ort` rc | `webgpu_dawn.dll` + `dxcompiler.dll` + `dxil.dll`; update `release.yml` staging if the set changes. |
| whisper.cpp | `whisper-rs-sys 0.15` (from `whisper-rs 0.16`) | tracks the sys crate | Compiled from source via `cc`/`cmake`; bindgen #599 pitfall (build from PowerShell). |
| espeak-ng | `espeak-rs-sys` (via `misaki-rs 0.3`) | tracks misaki | **GPLv3 source** → propagates to the binary license. Needs libclang. |
| SQLite | `cozo` `storage-sqlite` | bundled by cozo | The code-graph on-disk backend. |

## Build toolchain (host machine + CI)

| Tool | Version / location | Needed for |
|---|---|---|
| Rust | edition 2021, **MSRV 1.77** (`Cargo.toml rust-version`) | everything |
| Node + npm | LTS (CI: `windows-latest`) | frontend build |
| MSVC | VS 2026, `_MSC_VER` 1950 (`cl.exe`, auto-found by `cc`) | native crates, GPU builds |
| CMake | VS-bundled 4.2.3, on PATH | whisper.cpp + espeak builds |
| Ninja | VS-bundled | `stt-vulkan` shader-gen sub-build (`CMAKE_GENERATOR=Ninja`) |
| LLVM / libclang | `C:\Program Files\LLVM\bin` (pinned in `.cargo/config.toml`) | bindgen for whisper-rs / misaki / espeak |
| Vulkan SDK (LunarG) | `C:\VulkanSDK\1.4.350.0` (`VULKAN_SDK`, pinned in `.cargo/config.toml`) | `--features stt-vulkan` only |
| CUDA toolkit *(optional)* | 13.2 for `stt-cuda`; 12.x + cuDNN 9 for `tts-cuda` | the non-shipped NVIDIA-only GPU features |
| cuDNN *(optional)* | 9.21 (`…\v9.21\bin\12.9\x64`, not on PATH by default) | `tts-cuda` / `ort` CUDA EP only |

The default `cargo build` (CPU-only feature set) needs **none** of the GPU/SDK
rows — only Rust + a C toolchain + CMake + libclang. The Vulkan/CUDA rows apply
only when building those opt-in features.

### Linux build (Ubuntu 24.04) — GPU parity

The Linux release (`release.yml` `build-linux`) builds the same
`stt-vulkan,tts-webgpu` feature set as Windows. Validated on Ubuntu 24.04 (WSL2);
CI runs on `ubuntu-24.04`.

**Distro floor is 24.04, not 22.04.** ort's WebGPU Linux prebuilt is a *static*
`libonnxruntime` compiled against **glibc ≥ 2.38 + libstdc++ from GCC 13/14**
(its objects reference `__isoc23_strtoll@GLIBC_2.38`,
`std::ios_base_library_init()@GLIBCXX_3.4.32`). Ubuntu 22.04 (glibc 2.35, GCC 11)
cannot link or run it, and glibc can't be upgraded in place. ort is the only TTS
runtime, so the whole build+runtime floor is 24.04 (glibc 2.39). The shipped
binary's floor is therefore Ubuntu 24.04+.

Build inputs beyond the obvious Tauri/ALSA `-dev` packages:

| Input | Why | How |
|---|---|---|
| `libwebkit2gtk-4.1-dev libsoup-3.0-dev libgtk-3-dev librsvg2-dev libayatana-appindicator3-dev` | Tauri v2 webview | apt |
| `libasound2-dev` | cpal/rodio (ALSA) — **new on Linux** | apt |
| `cmake clang libclang-dev llvm` | whisper.cpp/espeak-ng cmake + bindgen | apt |
| `LIBCLANG_PATH=/usr/lib/llvm-<N>/lib` | bindgen can't find libclang otherwise | `dirname $(find /usr/lib/llvm-* -name libclang.so ...)` |
| `libssl-dev` | **build-time only** — `ort-sys` build-dep `ureq`→`native-tls`→openssl on Linux; not linked into the binary | apt |
| `glslc` + recent Vulkan headers | whisper's ggml-vulkan shader-gen; Ubuntu ships neither new enough (needs `VK_EXT_layer_settings` etc.) | LunarG apt repo (`lunarg-vulkan-noble.list`) → `shaderc vulkan-headers libvulkan-dev` |
| `espeak-ng-data` | espeak-rs-sys builds only the lib on Linux, not the compiled phoneme tables; `build.rs` copies the system package's data next to the binary | apt |

Two Linux-specific bits in `build.rs`: `find_system_espeak_data()` sources
espeak-ng-data from the system pkg (and warns instead of panicking off Windows),
and `set_linux_origin_rpath()` adds an `$ORIGIN` rpath so the bundled
`libwebgpu_dawn.so` (ort's WebGPU/Dawn runtime dylib — the Linux analog of
`webgpu_dawn.dll`; there is no `dxcompiler`/`dxil` off Windows) resolves next to
the binary. The portable tarball ships `bin/cimp` + `bin/libwebgpu_dawn.so` +
`bin/espeak-ng-data/`.

## External runtime components & models (not in the repo)

Shipped in the portable zip or run as separate services. Not version-managed by
cargo/npm — check their sources manually.

| Component | What / where | Update check |
|---|---|---|
| Kokoro TTS model | `kokoro-v1.0.onnx` + `voices/af_heart.bin` (Apache 2.0) — downloaded at release time from HuggingFace | HF model card; bump the download URL in `release.yml`. |
| Whisper STT model | `ggml-small.bin` (~466 MB, MIT) — committed via Git LFS, verified vs `models/CHECKSUMS.txt` | whisper.cpp ggml model releases. |
| `llama-server` (llama.cpp) | offload backend **and** embedding server; user-run, not bundled | <https://github.com/ggml-org/llama.cpp/releases> — rebuild/redownload periodically. |
| Offload model | Qwen3.6-35B-A3B (GGUF, quantized) on the local llama-server | newer Qwen / quant releases. |
| Embedding model | Qwen3-Embedding-4B Q8_0, 2560-dim, on `mcp1:8085` (RTX 3070) | re-embed the graph if you change model/dims (auto-probed). |
| Offload MCP servers | `ddg` + `context7` as Streamable-HTTP endpoints (`172.21.1.11:17201/17202`); plus stdio `git`/`fetch`/`fs`/`context7` | each MCP server's own repo; live-reloadable in Settings → Tools. |
| WebView2 runtime | Windows system component (or installer-bundled) | OS-managed; relevant only if shipping an installer. |

---

## Dependencies to track

### `ort` / ONNX Runtime — GPU TTS via the WebGPU EP (shipped); CUDA broken on Blackwell

- **Current pin:** `ort = "=2.0.0-rc.11"` (`src-tauri/Cargo.toml`), `features = ["download-binaries"]` + a per-build GPU feature (below). Wraps **ORT 1.20.x**. The optional `cuda` prebuilt is hard-linked to CUDA major 12 (`onnxruntime_providers_cuda.dll` references `cudart64_12.dll`, `cublas64_12.dll`, `cublasLt64_12.dll`, `cufft64_11.dll`, `cudnn64_9.dll`); CUDA 13.x won't load with this version.

- **IMPLEMENTED — `tts-webgpu` is the shipped GPU TTS backend.** Kokoro runs on ONNX Runtime's native **WebGPU EP** (Dawn-backed → D3D12 on Windows, Vulkan on Linux, Metal on macOS). Validated on the dev box (RTX 5090 / Blackwell) 2026-06-15: correct output matching the CPU reference, genuinely on-GPU (ORT node-placement logs show WebGPU shader programs for every op, incl. the `ConvTranspose2D` that broke DirectML), **~5× faster than CPU** at steady state. Wired in `tts/engine.rs` as GPU-by-default with automatic CPU fallback, selectable GPU/CPU at runtime via the `tts.device` setting (*Settings → Audio → TTS → Process on*) — mirrors `stt/engine.rs`. Runtime deps: three Dawn dylibs (`webgpu_dawn.dll`, `dxcompiler.dll`, `dxil.dll`) staged into the zip by `release.yml`; `download-binaries` static-links core ONNX Runtime into `cimp.exe` (no `onnxruntime.dll`). Full write-up: `docs/features/FEATURE-tts-webgpu.md`.

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
  - The WebGPU EP is flagged **experimental** upstream. On an `ort` bump, re-run the `tts-webgpu` smoke test (`cargo test --features tts-webgpu --bin cImp -- --ignored --nocapture synthesizes`) to confirm Kokoro still produces correct audio and stays on-GPU. Watch <https://github.com/pykeio/ort/releases> and <https://crates.io/crates/ort>.
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
    when `stt.device` is `Gpu` (the default) and **falls back to CPU
    automatically** if GPU init fails or no GPU is present (this is what makes
    the Vulkan binary universal). The `stt.device` setting (*Settings →
    Speech-to-text → Process on*) selects GPU vs CPU at runtime and supersedes
    the old `CIMP_GPU` env var, which is no longer read.

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
     (e.g. `C:\ct`). CI is unaffected — the runner path (`D:\a\cImp\cImp`) is
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

## Offload backends (V8-01 / V8-02)

The offload pool lives under `offload.backends` in `settings.json`. A V8-01
single-server config (`offload.server_command` + `offload.autostart`) migrates
to one Local backend automatically (v1.16 → v1.17); the legacy scalar fields
still work as a fallback when `backends` is empty.

Example pool — a big local model, a small LAN box, and an optional cloud API:

```jsonc
"offload": {
  "enabled": true,
  "backends": [
    {
      "name": "main",
      "enabled": true,
      "tier": "quality",
      "tool_scope": { "mode": "all" },
      "kind": {
        "type": "local",
        "server_command": "llama-server --model C:\\models\\Qwen3.6-35B-A3B-Q4.gguf --port 8080 --jinja -ngl 99 --ctx-size 150000 --flash-attn",
        "autostart": true
      }
    },
    {
      "name": "lan-3070",
      "enabled": true,
      "tier": "fast",
      "tool_scope": { "mode": "all" },          // trusted LAN → all tools
      "kind": {
        "type": "remote",
        "base_url": "http://192.168.1.50:8080",  // a llama-server on the LAN box
        "auth_token": "",
        "is_cloud": false,
        "cloud_consent": false
      }
    },
    {
      "name": "cloud",
      "enabled": false,                          // off until you opt in
      "tier": "quality",
      "declared_context": 128000,                // cloud APIs rarely expose /props
      "declared_model": "some-cloud-model",
      "tool_scope": { "mode": "allexcept", "tools": ["read_file","code_search","run_command","filesystem","git"] },
      "kind": {
        "type": "remote",
        "base_url": "https://api.example.com/v1",
        "auth_token": "sk-...",                  // redacted in Debug logs
        "is_cloud": true,
        "cloud_consent": true                    // REQUIRED for a cloud backend to be usable
      }
    }
  ]
}
```

Notes when maintaining this:

- **Routing** is `offload/router.rs::select` — a pure function over `BackendView`
  snapshots (readiness → tool-need → context budget → tier/availability). The
  unit tests there encode the expected behavior; update them when changing the
  selection order.
- **Remote capabilities**: a remote `llama-server` exposes `n_ctx` via `/props`;
  cloud APIs usually don't, so they rely on `declared_context`. The probe treats
  any HTTP response from a cloud `/health` as "reachable" (cloud endpoints often
  lack `/health`); a LAN llama-server must answer `/health` 2xx.
- **Cloud privacy** rests on two independent checks: the router never routes a
  local-data task to a cloud backend (`required_tools ⊄ allowed`), and the agent
  loop's `NativeRouter` filters the `tools` array by scope *and* refuses a
  disallowed call. Keep both — they're tested in `router.rs` and `agent.rs`.
- **Warm pool vs. fallback child (V8-03)**: when the app is running it owns the
  loop + pool + router + global gate + MCP host (`offload/service.rs`), and the
  `cimp --offload-mcp` child is a thin proxy to it. Only the app sees all
  in-flight offloads, so cross-backend spill/fail-over works there. The child
  still carries the **self-contained fallback** (the V8-02 path) for when the app
  is down — keep it first-class (headless `claude -p` / cron paths depend on it),
  and keep the shared `router`/`agent` code shared so the two paths can't drift.

## Offload warm pool, loopback endpoint & MCP host (V8-03)

When the app is up, the offload service (`offload/service.rs::OffloadService`,
held in `AppState`) is the single owner of the warm pool, the global concurrency
gate, and the MCP host. The per-session `cimp --offload-mcp` child forwards to it
over a small authenticated loopback HTTP endpoint.

**Loopback endpoint + discovery file (`offload/loopback.rs`).**

- Binds `127.0.0.1:0` (ephemeral port) and requires a **per-launch bearer token**.
  Routes: `POST /run`, `GET /describe`, `GET /events` (SSE). Purpose-built for
  offload — not a general local API.
- Advertises `{port, token, pid}` in a discovery file at
  **`<exe-dir>/.cimp-offload.json`** (the portable root, next to `settings.json`
  — *never* `~/.claude`), written when `offload.enabled` and **removed on graceful
  exit**. The token rotates every launch; on Unix the file is `chmod 600`
  (best-effort; Windows ACL tightening is a TODO).
- **Security model / residual risk:** loopback-only bind + token auth keep another
  local process from driving offloads or reading task text *in flight*. A
  malicious local process that can read the discovery file could still do both —
  the same trust assumption as any localhost dev server. Mitigations: ephemeral
  token, loopback bind, file perms. Don't log the token; don't widen the bind off
  loopback.
- The child probes the endpoint per request and **falls back** to the
  self-contained path on any transport failure (stale discovery file from a
  hard-killed app, app mid-restart, app not running).

**MCP host (`offload/mcp_host.rs`).** Warm client pool over `offload.mcp_servers`
(same shape as Claude's `mcpServers`). Per server: `initialize`+`tools/list`,
namespacing `<server>__<tool>`, a **read-class filter** (leading-verb heuristic on
the first two name segments — see `is_read_class`/`WRITE_VERBS`, unit-tested), and
`filesystem` confinement (the configured `allowed_roots` are appended as the
server's allowed dirs). stdio is fully warm (a reader task demuxes JSON-RPC by id);
HTTP `url` servers are best-effort single-POST. A crashed/hung server is isolated
(its tools drop from the capability set) and surfaces in *Settings → Offload → MCP
tool servers*. Example config is in `README.md`.

**Live capabilities / `tools/list_changed`.** `OffloadService` exposes a change
channel fed by (a) MCP-host connect/drop pulses and (b) a periodic health watch
that compares the ready-backend set. The loopback `/events` stream relays each as
a `change` event; the child (which holds the stdio pipe to Claude) emits
`notifications/tools/list_changed`. `describe()` always renders from live health.

**Global concurrency.** `offload.global_concurrency` (optional) caps total
offloads in flight; `null` auto-sizes from the summed per-backend slot counts,
clamped to 32. The gate is created at app launch — changing the cap needs a
relaunch.

## Code graph grammars & tags queries (V9-02)

The code graph extracts symbols/calls via a generic tree-sitter `tags.scm`
engine (`src-tauri/src/graph/tags.rs`). **To add a language:**

1. Add its `tree-sitter-<lang>` grammar crate to `src-tauri/Cargo.toml` (must be
   ABI-compatible with `tree-sitter = 0.26` — exposes a `LANGUAGE` `LanguageFn`;
   check with `cargo add <crate> --dry-run` + a build).
2. Add a `Lang` variant + file extensions in `graph/model.rs` (`from_path`,
   `tag`, `from_tag`) and a `language_for` arm in `graph/builder.rs`.
3. For a **code** language: vendor a query at `src-tauri/queries/<lang>/tags.scm`
   (prefer the grammar's upstream `queries/tags.scm`; trim over-broad
   `@reference.call` patterns that rely on `#is-not?`/`#not-match?` predicates —
   the base `Query` engine doesn't enforce them). Add an `include_str!` arm to
   `tag_spec` and route the variant through the engine in `parse_file`. For a
   **markup/data** language, skip the query — registering it in `language_for`
   already enables `graph_struct_search`.
4. Add the tag to the default `languages` list (`settings/schema.rs` +
   `lib/settings/types.ts`) if it should index by default, and to the Settings
   "Supported" hint in `SettingsApp.svelte`.
5. Add a fixture test in `graph/tags.rs`; the `every_vendored_query_compiles`
   test already guards that every vendored query compiles against its grammar
   (catches a node/field name that drifted in a grammar update).

The engine derives containment and caller attribution purely from byte spans, so
a `tags.scm` whose `@definition.<kind>` capture sits on the actual construct node
(not an enclosing scope) works with no engine changes. Capture suffixes map to
`SymbolKind` in `kind_from_suffix`.

## Code Intelligence — Context Engine (V10)

The "Code Graph" tab is renamed **Code Intelligence** (internal tab id
`graph-monitor` and the `graph` settings key are unchanged) and its view
(`src/lib/CodeIntelligenceView.svelte`) routes five sections: Index / Activity /
Memory / Context / Analyses.

**Schema versioning & migration.** `graph/schema.rs::GRAPH_SCHEMA_VERSION` stamps
the derived-relation shape. On open, `GraphIndex::migrate_schema` compares it
against a `schema_meta` singleton (which is **not** in `RELATIONS`, so it survives
`reset()`); a mismatch drops+recreates the derived relations, and the service's
normal rebuild repopulates them from source. This runs once, transparently, on
the first launch after an upgrade — bump `GRAPH_SCHEMA_VERSION` whenever a
`RELATIONS` column changes.

**Memory relations are rebuild-safe.** `session` / `mem_event` / `mem_note` are
ensured by `ensure_memory_relations` **outside** `RELATIONS`, because a full
index rebuild calls `reset()` (drops every `RELATIONS` relation) and memory is
runtime event data, not derived from source — it must survive a rebuild.

**Memory event sources are per-agent.** Claude records in-process via the
transcript tap (`oob/claude.rs::record_tool_events`, beside `update_agents`;
session id = the `<id>.jsonl` stem), wired through `OobContext.mem` from
`pty/manager.rs`. OpenCode's OOB SSE stream has no tool events, so its memory
comes from the injection plugin's `tool.execute.after` hook POSTing to
`/memory/event`. `graph::classify_tool` maps tool names → `(kind, arg)` for both.

**Memory-tool session scoping — per-agent (partial), pending a Claude Code
feature.** The `context_recall` / `context_note` / `context_notes` MCP tools have
no session argument (Claude Code does not pass session identity into an MCP
server's tool-call context — see below), so they resolve a session from
`graph.db` by recency. To keep a **Claude** tab and an **OpenCode** tab on the
same project from reading/writing each other's session, the resolution is scoped
to the *calling agent*: the MCP child's `--consumer` (claude/opencode) flows
`offload/mcp.rs::proxy_graph` → `/graph_run` (`GraphRunBody.consumer`) →
`run_graph_tool` → `dispatch_recorded` `source` → `mem_agent(source)` →
`GraphIndex::mem_current_session_for(Some(agent))` (and the app-down fallback
`handle_call(params, consumer)` does the same). `source` is also the activity
ring's badge, so OpenCode's graph/context calls now read as `opencode`, not
`claude` (frontend `GraphCall.source` union + `.hsrc.opencode`).

*Residual limitation (periodically re-check):* two tabs of the **same** agent
(e.g. two Claude tabs) on one project still share the same agent scope, so a
`context_note` from one can attach to the other's session, and `context_recall`
can return the other's working set. Full per-tab isolation needs a session
identifier available *inside the MCP tool call* so the tool knows which of
several same-agent sessions is calling.

- **What's missing:** Claude Code exposes a session id to **hooks** (the
  `UserPromptSubmit` payload carries `session_id` — that's how the transcript tap
  and `cimp --context-hook` get it) but **not** to the MCP servers it launches:
  the `cimp --offload-mcp` child is spawned per Claude session yet receives no
  session UUID (no arg, no env var, and no field on the JSON-RPC `tools/call`
  params). So the child literally cannot tell which session is invoking a tool.
- **What to watch for** (any of these closes the gap): a session id / session
  metadata field on the MCP `tools/call` request; a per-session env var set on
  the MCP server process at spawn (like the hook `session_id`); or an MCP
  "elicitation"/context mechanism that carries session identity. Check the MCP
  server docs + the Claude Code hooks/MCP release notes.
- **When it lands:** thread that id into `dispatch_recorded` alongside `source`,
  add `mem_event`/`session` writes keyed by it, and switch the tools from
  `mem_current_session_for(agent)` to an exact session lookup. The recording side
  already stores a real per-session id (`session.session_id`), so only the read
  path's "which session am I" resolution needs to change.

**Context injection** (opt-in, `graph.context_injection`). `graph/context.rs`
ranks files (symbol/reference/doc hits + session working set) and budget-packs
outline digests — synchronous, no per-prompt embedding. Claude injects via a
`UserPromptSubmit` hook (`cimp --context-hook`, `context_hook.rs`) added to the
`--settings` overlay; OpenCode via a generated dependency-free plugin
(`tabs/config.rs::write_opencode_plugin` → `<project>/.opencode/plugin/cimp-inject.js`,
baking in the loopback port+token per launch; `.opencode/` is added to
`.git/info/exclude`). **Never launch OpenCode with `--pure`** — it disables all
external plugins.

**New local loopback routes** (`offload/loopback.rs`), same authenticated-
localhost trust model as `/graph_run`: `POST /context/retrieve` (gated on
`context_injection`) and `POST /memory/event` (OpenCode's memory ingress).

## Code Intelligence — Token Efficiency (V11)

**Schema bump to v3 — one rebuild for the whole V11–V14 roadmap.**
`graph/schema.rs::GRAPH_SCHEMA_VERSION` moved 2 → 3 for a single column change:
`symbol.is_test` (provisioned for a later milestone, unused by anything in
V11). That's the *only* `RELATIONS` shape change, so it's the only thing that
forces the migrate-on-open rebuild described in the V10 section above. Every
other new store this milestone adds is **additive, create-if-missing, and
needs no version bump**: `code_chunk` (added to `RELATIONS` directly — the
code-embedding source text) plus `digest` and `code_vec`, both ensured lazily
the first time they're needed (`GraphIndex::put_digest` /
`ensure_code_vector_store`), the same pattern V10 used for `session` /
`mem_event` / `mem_note`. **`injected` (the Phase C dedup state) is *not* a
relation** — it's an in-memory `HashMap<session_id, InjectState>` on
`GraphService` (`graph/service.rs`), so it never survives a restart and needs
no schema entry; a restart just re-injects fresh on the next turn, which is
the intended fail-safe.

**Three Claude hook shims, one shared POST helper.** `context_hook.rs`'s
`post_loopback(path, body)` (Bearer auth, `Content-Length`, `Connection:
close`, 2xx-only, ~600 ms timeout) is now used by all three CLI subcommands
wired in `main.rs`:

| Subcommand | Hook event | Route | Module |
|---|---|---|---|
| `cimp --context-hook` | `UserPromptSubmit` | `POST /context/retrieve` | `context_hook.rs` (V10) |
| `cimp --precompact-hook` | `PreCompact` | `POST /context/compaction` | `compact_hook.rs` (V11 Phase D) |
| `cimp --read-hook` | `PreToolUse` (matcher `Read`) | `POST /context/should_read` | `read_hook.rs` (V11 Phase E) |

All three are dependency-light, synchronous, and fail open (print nothing,
exit 0) on any error — a hook must never block or perturb the agent's turn.
`tabs/config.rs` adds the `PreCompact` hook to the Claude settings overlay
whenever `context_injection && compaction_context`, and the `PreToolUse` hook
whenever `context_injection && read_advisor` (independent toggles — a project
can run compaction survival without the read advisor).

**Compaction route's side effects are unconditional.** `GraphService::
compaction_context` (`graph/service.rs`) always clears the session's
`injected` dedup map and marks it `post_compaction` — even when
`compaction_context` is off or the rendered block is empty — because those
two effects are what keep Phase C (dedup) and Phase E (read advisor) correct
across a compaction regardless of whether the block itself is gated on. Only
the returned working-set/notes text is gated.

**`TODO(spike)` — two hook output contracts are unverified against the pinned
Claude Code build:**
- **D0 (`compact_hook.rs`):** which JSON field of a `PreCompact` hook's stdout
  actually reaches the *compaction prompt* (we emit the documented
  `hookSpecificOutput.additionalContext` shape, mirroring the
  `UserPromptSubmit` hook, but this hasn't been confirmed hands-on the way the
  V10 OpenCode injection spike was). The server-side effects (dedup clear,
  post-compaction flag) are correct **regardless** of whether Claude reads
  this field, so the feature degrades safely either way — worst case the
  block just doesn't reach the model.
- **E1 (`read_hook.rs`):** whether a `PreToolUse` deny's
  `permissionDecisionReason` is surfaced **to the model** (not just the user)
  on the pinned Claude Code version. If it isn't, the read advisor can't
  substitute usable content on a deny and the milestone spec says to cancel
  the feature rather than ship a bare refusal — `read_advisor` defaults off,
  so nothing is affected until this is confirmed and the setting is turned on
  per project.

**Read advisor staleness check uses content hash, not mtime.** `should_read`
(`graph/service.rs`) compares the current file's FNV hash against the indexed
`file.hash` — the same check `graph_snippet`'s `stale` flag uses — rather than
comparing a stored mtime against the memory event's timestamp. A code-review
fix (see the `fix(V11)` commit): mtime comparison is vulnerable to filesystem
clock skew on network shares / WSL2 bind-mounts, which could wrongly suppress
a real edit and hand the agent stale content.

**Digest jobs are demand-driven, slot-gated, and local-only.**
`context_llm_digests` only digests files that actually ranked into an
injection and have no outline (docs/configs/long scripts) — not the whole
repo. `GraphService::enqueue_digest` single-flights by `(root, file,
content_hash)` (an `InflightGuard` removes the key on `Drop`, so a panicked
digest task can't permanently leak a slot) and caps concurrent jobs at 32.
The compute itself goes through `OffloadSupervisor::run_internal` — a
non-streaming, tools-off, thinking-suppressed completion that **only
considers backends already running locally** (`self.running`, not the full
pool/router), so a digest can never route to a remote or cloud backend
regardless of `allow_remote_worker_access`. Injection never blocks on this: a
cache miss falls back to the V10 outline/empty digest and the result lands in
`graph.db`'s `digest` relation for the next retrieve.

**Code-embedding backfill rides the doc-embedding pass, strictly after it.**
`embed_backfill` (`graph/service.rs`) embeds `doc_chunk`s first (cheaper, and
doc search stays useful even with code embedding off), then — only when
`embed_code_bodies` is on — embeds pending `code_chunk`s into `code_vec` under
the same epoch/dim/model. `graph_semantic_code` is advertised (`graph/mcp.rs
tools()`) only when **both** `semantic_search` and `embed_code_bodies` are on
(a code-review fix — the backfill that actually populates `code_vec` only
runs when `semantic_search` is on, so gating the tool on `embed_code_bodies`
alone would advertise a tool that could never return results). No full-text
fallback exists for code chunks the way `graph_search_docs` backs
`graph_semantic_docs` — a miss degrades to a clear "unavailable, try
`graph_find_symbol`/`graph_struct_search`" message instead of silently
re-running as a keyword search.

**`file_centrality` counts distinct inbound edges, not join rows** (a
code-review fix). `graph_repo_map`'s ranking signal is inbound call-edge
count per file; the initial implementation joined `edge` against `symbol`
without deduping, so a callee name defined N times in one file inflated that
file's centrality by N×. Fixed in `graph/index.rs::file_centrality`, with a
regression test alongside it.

## Code Intelligence — Agentic Inner Loop (V12)

**No schema bump — every V12 store is additive, create-if-missing.**
`symbol.is_test` (Phase C) is the one column that would normally force a
version bump, but it rode V11's v2 → v3 bump for free (the column already
existed, unused, in that migration — see the V11 section above), so
`GRAPH_SCHEMA_VERSION` stays at 3 for all of V12. Every other new store is a
plain relation created on first use, the same pattern V10/V11 used for
`session`/`digest`/`code_chunk`: `commit_touch` (Phase D, file churn),
`project_fact` (Phase E, durable facts), `session_distilled` (Phase E, an
idempotency marker per session id), and `meta` (Phase F, a small generic
key/value store backing the analyses-auto trigger's last-seen counts). An
older `graph.db` opens against these with zero migration step — they simply
don't exist until the first write.

**A fourth Claude hook shim joins the V11 three, sharing the same POST
helper.** `postedit_hook.rs`'s `cimp --postedit-hook` (`PostToolUse`, matcher
`Edit|Write|MultiEdit` → `POST /context/post_edit`) reuses `context_hook.rs`'s
`post_loopback(path, body)` exactly like `compact_hook.rs` and `read_hook.rs`
do — same Bearer auth, `Content-Length`, ~600 ms timeout, fail-open-on-any-error
posture. `tabs/config.rs` adds the hook to the Claude settings overlay
whenever `context_injection && auto_check`, independent of the other three
context-hook toggles.

**`TODO(spike F0)` — a third unverified hook output contract, same posture as
V11's D0/E1.** Which JSON field of a `PostToolUse` hook's stdout actually
reaches the model as additional context is unconfirmed against the pinned
Claude Code build (`postedit_hook.rs`'s module doc). We emit the documented
`hookSpecificOutput.additionalContext` shape, mirroring `UserPromptSubmit`/
`PreCompact`. Degrades safely either way: the server-side effects (debounce
clock, baseline update, parked-block bookkeeping) run regardless of whether
Claude reads the field, and a parked block still drains via the next
`/context/retrieve` call (`GraphService::drain_auto_check`) — worst case the
block just arrives a turn later instead of inline. `auto_check` defaults off,
so nothing is affected until this is confirmed and a project opts in.

**The `checks/` module is a new dependency surface: parser fixtures need
upkeep alongside the tools they parse.** `checks::parsers` has one parser per
shipped `ParserKind` (`cargo-json`, `tsc`, `eslint-json`, `pytest`,
`generic-gcc`); each is regex/JSON-shape coupled to that tool's *current*
output format. A `cargo`/`tsc`/`eslint`/`pytest` release that changes its
diagnostic JSON shape or line format silently degrades `run_check` to
zero/garbage groups rather than erroring loudly — there's no schema
validation against the real tool, only the fixtures in `checks/parsers.rs`'s
test module. Re-run those fixtures (and add a new one from a real tool
invocation) whenever bumping a toolchain this repo's own `checks:` config
points at, and periodically spot-check the parser against that tool's latest
`--help`/changelog for output-format notes.

**`graph_impact` / `is_test` / `graph_tests_for` are all approximate by the
same name-keyed-call-graph limitation `graph_references` already documents.**
None of these resolve dynamic dispatch, trait objects, higher-order callbacks,
or reflection-based test discovery — they walk the same reverse/forward
`calls` edges the rest of the graph does, which are name-keyed, not
type-checked. `graph_impact`'s dependent tree and `graph_tests_for`'s test
list are both labeled candidates in their tool descriptions (`graph/mcp.rs`),
same honesty convention as dead exports: an empty result reads as "found
none", not "verified none exist." Test detection itself (`graph/builder.rs`'s
`is_test` walkers) has no bit at all for languages without a bespoke walker or
a path-convention fallback — again accurate-but-incomplete rather than wrong,
matching V10's `visibility` precedent.

**The 4-agent code-review pass (`fix(V12)`, commit `aa120c3`) is worth reading
directly** — it caught several correctness bugs that would otherwise degrade
silently: `git status --porcelain` collapsing a brand-new untracked directory
into one `?? dir/` line (both `graph_impact` and `changed_only` now use
`-z --untracked-files=all` and NUL-split, shared between `graph::impact` and
`checks::gitls`); a `changed_only` site filter that could drop a just-edited
file's occurrence when a diagnostic already had ≥5 sites elsewhere (fixed by
filtering the *uncapped* site list before `cap_sites` truncates — see
`checks::mod::run`'s doc comment); a check that fails to spawn previously
vanished from the report indistinguishably from "ran clean" (now surfaced as
`"⚠ check `<name>` did not run: <err>"`, `checks::auto::spawn_failure_line`);
`is_cfg_test` missing `cfg(any(test, …))`/`cfg(all(test, …))`; a
`DistillGuard` in-flight-session-id guard preventing two concurrent
distillation sweeps (a full rebuild and a watcher-batch reindex can both pick
up the same idle session) from double-distilling it into duplicate facts; the
project-fact ranking boost requiring a whole-word, ≥4-char, non-generic-stem
match (the initial version was a raw substring match, so `mod`/`index`/
`context` spuriously boosted unrelated files); and `parse_unified_diff` only
treating a `+++ ` line as a new file header when it immediately follows a
`--- ` line (otherwise an added line whose *content* starts with `++` can be
misread as a header). The same pass also de-duplicated the two modules' git
spawn helper into `graph::gitcmd::run_git` (shared by `graph::impact` and
`graph::gitmeta`; `checks::gitls` keeps its own async twin on purpose — see
that module's doc comment) and bounded `graph_recent_changes` at the Datalog
level (`:order -last_ts :limit`) instead of scanning the whole `commit_touch`
relation per retrieve.

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
