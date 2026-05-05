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

## ~~Espeak fallback for out-of-vocabulary words~~ — shipped (default)

Always on — `misaki-rs` is pulled in with default features, which includes its
`espeak` fallback. espeak-ng is statically linked, so no `libespeak-ng.dll` is
shipped, but `espeak-ng-data/` (~7.5 MB) sits next to `cctts.exe` (auto-copied
by `build.rs`). The compiled binary is GPLv3 (see `NOTICE`); cctts source stays
Apache-2.0. Builds need `libclang.dll` for bindgen — pinned via
`src-tauri/.cargo/config.toml`. Verified end-to-end: `"eBook" → "ˈi bˈʊk."`.
