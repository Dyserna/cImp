# Packaging Notes

This file collects what was decided about distribution. The actual
release pipeline lives at `.github/workflows/release.yml` and is
documented in `RELEASE.md`. This document is the *why*.

## Current shape: portable Windows zip

A tag-driven GitHub Actions job builds `cctts-portable-win-x64-vX.Y.Z.zip`
on every `v*.*.*` push. Layout:

```
bin/
  cctts.exe
  onnxruntime*.dll
models/
  kokoro-v1.0.onnx
  voices/af_heart.bin
LICENSE
NOTICE
README.txt
```

Add `bin/` to PATH; run `cctts` from any terminal. No installer, no
registry entries, no admin rights. Everything cctts writes lives inside
the unzip folder: `settings.json` and `scrollback/` next to the exe in
`bin/`; logs under `<portable-root>/logs/` (sibling of `bin/`).
Per-launch-directory overlays (`.cctts.custom.config.json`) go in
whatever working directory the user starts cctts from.

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

## CUDA Runtime

`ort = 2.0.0-rc.11` (which wraps ORT 1.20.x) dynamically links the CUDA
runtime when `CCTTS_GPU=cuda` is set. The bundled providers DLL references
`cudart64_12.dll`, `cublas64_12.dll`, `cublasLt64_12.dll`, `cufft64_11.dll`,
and `cudnn64_9.dll` directly — distribution would need to either:

- Document a CUDA 12.x + cuDNN 9 install requirement (current README
  approach), or
- Bundle the CUDA redistributables. Bigger download but works out of the
  box. NVIDIA's CUDA redist license allows redistribution of the runtime;
  check the current EULA before shipping.

CPU-only is the default and needs no extra runtime.

## Updates

Currently manual: download the next release zip, unzip over the existing
folder. The `settings.json` next to the exe and any per-folder
`.cctts.custom.config.json` overlays are not in the zip and stay where
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
The `cctts-avatars` Vite plugin in `vite.config.ts` serves them at
`/avatar/<theme>/...` in dev and copies them to `dist/avatar/<theme>/...`
at build time, where they're embedded in the Tauri output as part of
`frontendDist`. The release workflow also stages the same `avatars/`
tree into the portable zip's `avatars/` folder for on-disk discoverability.
No separate step required.

Custom user-picked image/video paths are absolute disk paths; they're
loaded via Tauri's asset protocol and need `assetProtocol.scope` to permit
the path. `tauri.conf.json` currently sets `scope: ["**"]` — broad, fine
for personal use. For distribution narrow this to specific user-data dirs.
