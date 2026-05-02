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
