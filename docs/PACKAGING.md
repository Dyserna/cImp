# Packaging Notes

This file collects what was decided about distribution. The actual
release pipeline lives at `.github/workflows/release.yml` and is
documented in `RELEASE.md`. This document is the *why*.

## Current shape: portable Windows zip

A tag-driven GitHub Actions job builds `cimp-portable-win-x64-vX.Y.Z.zip`
on every `v*.*.*` push. Layout:

```
bin/
  cimp.exe
  onnxruntime*.dll      # usually none — ORT core is static-linked in the webgpu build
  webgpu_dawn.dll       # WebGPU EP (Dawn) runtime — GPU TTS
  dxcompiler.dll        #   "   (DirectX shader compiler Dawn uses for D3D12)
  dxil.dll              #   "
models/
  kokoro-v1.0.onnx
  voices/af_heart.bin
LICENSE
NOTICE
README.txt
```

Add `bin/` to PATH; run `cimp` from any terminal. No installer, no
registry entries, no admin rights. Everything cImp writes lives inside
the unzip folder: `settings.json` and `scrollback/` next to the exe in
`bin/`; logs under `<portable-root>/logs/` (sibling of `bin/`).
Per-launch-directory overlays (`.cimp.custom.config.json`) go in
whatever working directory the user starts cImp from.

The Rust runtime resolves the model dir as `<exe-dir>/../models/` (see
`model_dir()` in `src-tauri/src/tts/mod.rs`) — that's the only location
checked. Voicepack auto-discovery (`list_voices` IPC) reads the same
dir.

## Build

`npm run tauri build` produces platform-specific bundles under
`src-tauri/target/release/bundle/`. `tauri.conf.json` currently has
`bundle.active = false` so a plain `cargo tauri build` will skip bundling.

To re-enable bundles for a release build, flip:

```jsonc
// src-tauri/tauri.conf.json
"bundle": { "active": true, "targets": "all" }
```

Per-platform targets:

- **Windows:** `msi` (WiX) and `nsis`. Installer can include the WebView2
  runtime to cover older systems where it isn't preinstalled.
- **Linux:** `deb`, `rpm`, `appimage`. Each pulls in WebKitGTK as a runtime
  dep (or bundles it for AppImage).

## Code Signing

- **Windows:** required to avoid SmartScreen warnings. Out of scope for
  personal use; relevant if shipping to end users. Configure under
  `tauri.conf.json -> bundle.windows.signCommand` or via the Tauri signing
  CLI. Needs a code-signing certificate (cheap-ish OV, expensive EV).
- **macOS:** not a current target. Would need an Apple Developer ID +
  notarization if added later.
- **Linux:** signing isn't typically required; some distros expect signed
  repo metadata for auto-updates, not signed binaries.

## Kokoro Model Files

The portable zip ships `kokoro-v1.0.onnx` + `af_heart.bin` (Apache 2.0,
attributed in `NOTICE`). The release workflow downloads them from
HuggingFace at build time — they are not checked into the source repo.

Source builds (`npm run tauri build` locally without the workflow) need
the model files dropped into `<exe-dir>/../models/` — for a `cargo run`
that means `src-tauri/target/models/`, for a portable-staged build it's
the `models/` sibling of `bin/`. There is no APPDATA fallback.

Alternative distribution shapes considered and rejected:

1. **User-provided only (pre-portable-zip).** Worked for source builds
   but made the portable zip useless on first launch.
2. **Download on first run.** Adds first-run latency + a network-failure
   path; the Tauri runtime would need a Rust task that hits HuggingFace,
   verifies a SHA-256, drops the files. Build-time bundling beats it on
   reliability and offline use.
3. **Tauri bundle resources.** `tauri.conf.json -> bundle.resources` is
   designed for MSI/NSIS installers; the portable zip pipeline assembles
   the layout directly so this isn't needed.

## Whisper Model Files (V6-01 speech-to-text)

The **full** portable zip also ships a ggml Whisper model — default
`ggml-small.bin` (~466 MB, MIT, attributed in `NOTICE`) — into the same
`<exe-dir>/../models/` directory as the Kokoro assets. Unlike Kokoro it is
**committed via Git LFS** (alongside the Kokoro model + voicepacks) and
verified against `models/CHECKSUMS.txt` by the workflow's existing
"Verify committed assets" step; no build-time download.

The **slim / no-models** zip does **not** include it, so re-extracting a
no-models update never clobbers a user's local `ggml-*.bin`. The full zip
therefore grew by ~466 MB on top of Kokoro's ~310 MB — acceptable because
size-sensitive users take the slim zip and drop in whatever model they want.

Users can add other models by dropping `ggml-*.bin` files into `models/`;
they appear in Settings → Speech-to-text → Model. A missing configured
model degrades gracefully to an in-UI "model not found" state (the app
still launches; the record button surfaces an error on first use). The
release-staging copy is guarded by `Test-Path` so a release can ship
before the LFS blob is committed.

## TTS GPU — portable WebGPU (the released zip)

The release builds Kokoro TTS with ONNX Runtime's **WebGPU** execution provider
(`--features tts-webgpu`), the TTS analog of the STT Vulkan story below: one
portable binary that uses any vendor's GPU and falls back to CPU.

- **Portable AND GPU-capable.** The WebGPU EP is Dawn-backed (D3D12 on Windows,
  Vulkan on Linux, Metal on macOS). The only bundled runtime deps are three Dawn
  dylibs staged into `bin/`: `webgpu_dawn.dll`, `dxcompiler.dll`, `dxil.dll`. No
  CUDA DLLs, no redistributables, no SDK. `ort`'s `download-binaries` static-links
  core ONNX Runtime into `cimp.exe`, so there is usually no `onnxruntime.dll`.
- **GPU by default with CPU fallback** (`tts/engine.rs` registers WebGPU then
  falls back; `CIMP_GPU=cpu` forces CPU) — same model as STT. Validated ~5×
  faster than CPU and correct on Kokoro, including the `ConvTranspose` that broke
  the DirectML EP. See `docs/features/FEATURE-tts-webgpu.md`.
- **No extra build toolchain.** Unlike `stt-vulkan`, the WebGPU EP is a prebuilt
  — no Vulkan SDK / Ninja / MSVC-generator needed for the TTS half.
- The release staging step **fails the build** if any of the three Dawn dylibs is
  missing from the build output, so a GPU-less zip never ships silently.

## CUDA Runtime (optional `tts-cuda` only — not shipped)

GPU TTS via CUDA is now an **optional, non-default compile-time feature**
(`--features tts-cuda`), mutually exclusive with `tts-webgpu` (`ort` has no
prebuilt combining both). It is **not shipped** — the release uses `tts-webgpu`,
which already covers NVIDIA. Kept only for local NVIDIA experiments. A `tts-cuda`
binary dynamically links the CUDA runtime: `ort = 2.0.0-rc.11` (ORT 1.20.x)'s
providers DLL references `cudart64_12.dll`, `cublas64_12.dll`, `cublasLt64_12.dll`,
`cufft64_11.dll`, and `cudnn64_9.dll`, so such a build would need a CUDA 12.x +
cuDNN 9 install (and is broken on Blackwell — see MAINTENANCE.md). This is exactly
why WebGPU is the shipped path instead.

CPU-only is the default feature set and needs no extra runtime.

## STT GPU — portable Vulkan (the released zip)

STT GPU works **unlike** the TTS/CUDA story above, and is the reason the
portable zip can offer GPU without sacrificing portability. The release is
built with whisper.cpp's **Vulkan** backend (`--features stt-vulkan`):

- **The released `cimp.exe` is portable AND GPU-capable.** Its only
  GPU-related runtime dependency is `vulkan-1.dll` — a Windows system component
  present on every Win10+ machine. So one binary: uses any vendor's GPU
  (NVIDIA/AMD/Intel) when present, falls back to CPU automatically when not
  (`stt/engine.rs` tries GPU then CPU; `CIMP_GPU=cpu` forces CPU). **Nothing
  GPU-specific is bundled** — no CUDA DLLs, no redistributables.
- **The default feature set is CPU-only**, so routine local builds stay light;
  the GPU build is explicit (`--features stt-vulkan`). CI builds the release
  that way — see `release.yml`'s MSVC-dev-env + Vulkan-SDK steps and
  MAINTENANCE.md for the build requirements (Vulkan SDK, Ninja generator, MSVC
  dev env; deep local repos also need a short `CARGO_TARGET_DIR` for MAX_PATH).
- **Optional `stt-cuda`** exists for local NVIDIA max-perf but is never shipped:
  a CUDA binary imports `cublas64_*.dll`, isn't portable, and needs the CUDA
  13.2 toolchain to build (see MAINTENANCE.md). Vulkan is the portable default.

## Updates

Currently manual: download the next release zip, unzip over the existing
folder. The `settings.json` next to the exe and any per-folder
`.cimp.custom.config.json` overlays are not in the zip and stay where
they are across updates.

Tauri ships a built-in updater that polls a JSON manifest, downloads, and
verifies signatures. Adopting it would require:

- A signing key for updates (separate from code-signing cert).
- A static URL hosting the latest manifest + binaries.
- `tauri.conf.json -> plugins.updater` configured.

The GitHub release page is sufficient for now since the zip is small
relative to the model download.

## Settings Migration

`src-tauri/src/settings/schema.rs` uses `#[serde(default)]` everywhere, so
adding fields is backward-compatible. Removing or renaming a field would
need an explicit migration; none exists today. If a v1.x ships with new
settings, plan a migration rather than a hard schema break.

## Asset Bundling

The avatar default assets live at the top-level `avatars/<theme>/` folder.
The `cImp-avatars` Vite plugin in `vite.config.ts` serves them at
`/avatar/<theme>/...` in dev and copies them to `dist/avatar/<theme>/...`
at build time, where they're embedded in the Tauri output as part of
`frontendDist`. The release workflow also stages the same `avatars/`
tree into the **full** portable zip's `avatars/` folder for on-disk
discoverability. The slim / no-models update zip omits it — the embedded
copies render fine, and the on-disk folder is customization-only. No
separate step required.

The **sprite** avatar variant works the same way: sets live at the
top-level `sprites/<set>/` folder (each a `manifest.json` + frame
subfolders), served at `/sprites/<set>/...` in dev by the `cImp-sprites`
Vite plugin and copied to `dist/sprites/<set>/...` at build. Because the
frontend loads them from the embedded `/sprites/...` URLs, sprite avatars
function without any on-disk portable folder or Rust path-stamping — the
release workflow still stages `sprites/` into the **full** zip for
discoverability and for dropping in new sets (the slim / no-models update
zip omits it, same as `avatars/`). Adding a new bundled set is two steps: drop
the folder under `sprites/`, and add its name to `KNOWN_SPRITE_SETS` in
`src/lib/avatarConfig.ts`.

The bundled `claudeSprites` set is pixel-art Clawd mascot animation,
sourced from the Clawdmeter project (see the credits in `README.md` and
`NOTICE`).

Custom user-picked image/video paths are absolute disk paths; they're
loaded via Tauri's asset protocol and need `assetProtocol.scope` to permit
the path. `tauri.conf.json` currently sets `scope: ["**"]` — broad, fine
for personal use. For distribution narrow this to specific user-data dirs.
