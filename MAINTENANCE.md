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
