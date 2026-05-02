# Future Features

Ideas and enhancements deferred past the current milestone. Each item: what, why, and the trigger that should make us pick it up.

## Auto-detect Blackwell (or any unsupported GPU) and gracefully skip CUDA opt-in

- **What:** When `CCTTS_GPU=cuda` is set, probe the GPU compute capability before registering the CUDA EP. If the CC is unsupported by the bundled ORT prebuilt (currently sm_120 / Blackwell), log a clear warning and fall back to CPU instead of letting the user see per-segment `cudaErrorSymbolNotFound` errors and silent output.
- **Why:** Today the `CCTTS_GPU=cuda` opt-in is honest but unfriendly on Blackwell — registration succeeds, the session commits, and inference fails per-segment with a cryptic CUDA error. A pre-flight probe gives a single clear message at startup.
- **Costs to weigh:**
  - Querying CC requires loading the CUDA runtime, which we already do indirectly via ort. Either add a tiny `cudarc` (or similar) dependency to call `cudaDeviceGetAttribute`, or shell out to `nvidia-smi --query-gpu=compute_cap --format=csv` (works but ugly and adds a subprocess).
  - The "supported CC list" needs to be maintained alongside ort bumps — a magic list is fine but easy to forget to update. Could instead do a real probe inference (build session, run a 1-token forward pass, catch failure) which is self-validating but slower at startup.
- **Trigger to act:** if anyone besides the dev box reports the "registered but no audio" symptom on Blackwell, OR when `ort` upgrades to a version that adds new GPU support and we want the probe to handle the next-gen-GPU regression class generally.
- **Related:** `MAINTENANCE.md` "ort / ONNX Runtime" entry tracks the underlying ORT 1.20 + Blackwell mismatch.

## Espeak fallback for out-of-vocabulary words

- **What:** Add espeak-ng as a secondary G2P backend behind `misaki-rs`. When misaki returns its letter-by-letter fallback (signaling an OOV word), route the word through espeak instead and substitute the resulting phonemes back into the sequence.
- **Why:** Current pipeline uses `misaki-rs` with `default-features = false` (pure Rust, no espeak). Unknown words — proper nouns, acronyms, code identifiers — degrade to letter-by-letter spelling (e.g. "eBook" → "i bi o o keɪ"). Espeak has a real G2P for unknown words and would dramatically improve pronunciation quality on technical content.
- **Costs we already weighed (M3 decision, see `memory/project_phonemizer_choice.md`):**
  - License: espeak-ng is GPLv3 → distributed binary becomes GPLv3 (project source can stay Apache 2.0). User accepts this since the project will always be open source.
  - Not single-binary: bundled espeak adds `libespeak-ng.dll` plus a phoneme data dir alongside `cctts.exe`, breaking the v1 "one bundled binary" goal.
  - Windows MSVC build of `espeak-rs-sys` (the bundled Rust wrapper from piper-rs) is untested in public CI; budget a day or two to get it building cleanly.
- **Trigger to act:** real Claude output (not synthetic test cases) demonstrates that OOV pronunciation is a recurring annoyance. Until then, defer.
- **Implementation sketch when picked up:**
  - Add `espeak-rs-sys` (bundled variant) as an optional dependency behind a `cargo` feature flag.
  - In `src-tauri/src/tts/phonemize.rs`, detect misaki's letter-by-letter fallback (check whether each character was emitted as its own token) and re-phonemize that word via espeak.
  - Update bundling to ship `libespeak-ng.dll` + phoneme data dir; relax the single-binary constraint in `memory/project_constraints.md`.
  - Update `LICENSE` / `NOTICE` to reflect GPLv3 propagation in the binary distribution.

## Video render support for avatar states

- **What:** Render `<video autoplay loop muted playsinline>` for state assets whose extension indicates video (`.mp4`, `.webm`, `.mov`), keeping `<img>` for raster formats. The avatar component picks the element type per-asset based on file extension, so the same config slot can hold either kind.
- **Why:** The `Art/` folder already ships MP4 versions of every state (`Idle.mp4`, `Speaking.mp4`, etc.) plus a `Transistion.mp4`. M4 wired only the static PNGs because the design doc explicitly enumerates `<img>`-friendly formats (PNG/JPG/GIF/animated WebP). Animated WebP works, but MP4 is what the user actually has and is much more efficient than animated PNG/WebP for the same length/quality. Adding a small video branch unlocks the existing assets without a re-export.
- **Costs to weigh:**
  - Two element types means two code paths for transition cache-busting (URL `?t=` query works for both, but `<video>` needs `currentTime = 0` + `play()` on swap to actually restart, since the `?t=` trick alone doesn't restart playback once the element is mounted).
  - Settings UI (M6) needs a file picker that accepts both image and video extensions, and the schema validator should reject other formats early.
  - Cross-platform codec coverage: WebView2 plays H.264 MP4 fine; WebKitGTK depends on GStreamer plugins on the host. Document a minimum-codec requirement or recommend WebM/VP9 for Linux.
- **Trigger to act:** any of (a) the user wants to actually use the MP4 art they generated, (b) M6 settings UI lands and the user picks a video file, (c) animated WebP rendering produces visible artifacts on either platform.
- **Implementation sketch when picked up:**
  - In `src/lib/AvatarOverlay.svelte`, branch on `displayedSrc` extension: render `<video>` for video extensions, `<img>` otherwise. Both elements share `width: 100%; height: 100%; object-fit: contain`.
  - On state change, after assigning `displayedSrc`, if the new element is a `<video>`, set `currentTime = 0` and call `play()` in a `tick()` callback so the element exists.
  - Update `avatarConfig.ts` defaults to point at the MP4s the user has (`/avatar/Idle.mp4`, `/avatar/Speaking.mp4`, …, `/avatar/Transistion.mp4` — note the existing typo on disk; either match it or rename the source file).
  - Copy the MP4s into `public/avatar/` alongside the PNGs (or replace).
  - Confirm the spec carve-out: DESIGN.md currently lists only PNG/JPG/GIF/animated WebP for state assets. Update DESIGN.md to add MP4/WebM as a supported format, since this is a real architectural extension, not a hidden detail.
