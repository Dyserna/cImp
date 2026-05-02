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
