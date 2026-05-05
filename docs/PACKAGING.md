# Packaging Notes

cctts is not packaged for distribution in v1. This file collects what would
need to be addressed when distribution becomes a goal, so future-you (or
future-Claude-Code) doesn't have to reconstruct it.

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

The Kokoro ONNX model + voicepacks total a few hundred MB. v1 leaves them
out of the binary because:

1. They have their own license (review before redistribution).
2. They're too large to be palatable inside an installer for a personal-use
   tool that expects you to bring your own.

Three options for distribution:

1. **User-provided (current).** Document the download in `README.md`. Done
   for v1.
2. **Bundle.** Embed the .onnx + at least the default voice in the
   installer. Adds ~300 MB to the download. Doable; touch
   `tauri.conf.json -> bundle.resources` and update `default_model_dir()`
   in `src-tauri/src/tts/mod.rs` to fall back to the bundled path when the
   `%APPDATA%` copy is absent.
3. **Download on first run.** Tauri has no built-in downloader; would need
   a Rust task that hits HuggingFace, verifies a SHA-256, drops the files
   in `default_model_dir()`. Adds first-run latency + a network-failure
   path.

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

Tauri ships a built-in updater that polls a JSON manifest, downloads, and
verifies signatures. Out of scope for v1. Would require:

- A signing key for updates (separate from code-signing cert).
- A static URL hosting the latest manifest + binaries.
- `tauri.conf.json -> plugins.updater` configured.

## Settings Migration

`src-tauri/src/settings/schema.rs` uses `#[serde(default)]` everywhere, so
adding fields is backward-compatible. Removing or renaming a field would
need an explicit migration; none exists today. If a v1.x ships with new
settings, plan a migration rather than a hard schema break.

## Asset Bundling

The avatar default assets (`/public/avatar/*.mp4`) are served by Vite in
dev and bundled into `dist/avatar/` for builds. They're embedded in the
Tauri output as part of `frontendDist`. No separate step required.

Custom user-picked image/video paths are absolute disk paths; they're
loaded via Tauri's asset protocol and need `assetProtocol.scope` to permit
the path. `tauri.conf.json` currently sets `scope: ["**"]` — broad, fine
for personal use. For distribution narrow this to specific user-data dirs.
