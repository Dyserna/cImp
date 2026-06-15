# Feature: GPU Robustness (Auto-Detect Unsupported CUDA Compute Capabilities)

## Purpose

When `CCIMP_GPU=cuda` is set, probe the GPU's compute capability *before* registering the CUDA execution provider with `ort`. If the CC isn't supported by the bundled ONNX Runtime prebuilt — currently `sm_120` (Blackwell, RTX 5090 era) — log a clear warning at startup and fall back to CPU automatically. Replace today's behavior of "registration succeeds, session commits, every per-segment inference fails with `cudaErrorSymbolNotFound`, audio is silent."

A pre-flight check converts a cryptic per-segment failure mode into a single readable startup message.

See `FUTURE-FEATURES.md` § "Auto-detect Blackwell..." for the full rationale; this doc captures the implementation strategy.

## Background

This is a single-item feature; no group. It's listed standalone in `FUTURE-FEATURES.md` and lives in this group of feature docs because it's discrete enough to handle separately from the larger UX features.

The underlying ORT 1.20 + Blackwell mismatch is also tracked in `docs/MAINTENANCE.md` under "ort / ONNX Runtime." This feature complements that maintenance entry — the maintenance entry tracks the upstream upgrade; this feature improves ccImp's behavior *until* the upstream upgrade lands and *after* it lands for any next-gen-GPU regression class.

## Two viable approaches

### Option A: Static CC-list probe

Maintain a list of supported compute capabilities for the bundled `ort` prebuilt (e.g., `[sm_70, sm_75, sm_80, sm_86, sm_89, sm_90]`). At startup, query the GPU's CC and check membership.

- **Pros**: fast (tens of milliseconds), well-defined, easy to test.
- **Cons**: the list is a magic number that must be updated alongside every `ort` bump. Forgetting to update it → false negatives (CPU fallback for a supported GPU) or false positives (no fallback for an unsupported GPU). Add to the `MAINTENANCE.md` checklist for ort upgrades.

CC query implementations:
- **`cudarc` crate** (or similar CUDA Rust binding): `cudaDeviceGetAttribute(cudaDevAttrComputeCapabilityMajor/Minor)`. Adds a small dep but stays in-process.
- **Shell out to `nvidia-smi --query-gpu=compute_cap --format=csv,noheader`**: works without a new dep, but adds a subprocess at startup and depends on `nvidia-smi` being on PATH (typical on Windows after driver install; less reliable on Linux). Ugly.

Recommend `cudarc` (or whichever CUDA-binding crate the existing ort dependency tree already pulls in transitively — check at implementation time; we may already have what we need without a new direct dep).

### Option B: Probe inference

Build a tiny ORT session with the CUDA EP, run a 1-token forward pass, catch failure. If failure, tear down the session and fall back to CPU.

- **Pros**: self-validating. Works regardless of which GPUs ort supports today or tomorrow. No magic list to maintain.
- **Cons**: slower at startup (probe inference is on the order of 100ms-1s). Couples startup time to model loading on a code path that's *intended* to throw away its work. More fragile — distinguishing "this CC is unsupported" from "this model has a bug" or "GPU memory is full" needs careful error inspection.

### Recommendation: Option A first, Option B if maintenance burden bites

Ship Option A. Add the CC-list update to the `MAINTENANCE.md` ORT-upgrade checklist. If the list-update step gets forgotten in practice and users hit silent regressions, switch to Option B.

If `ort` ships a runtime API for "is this device supported?" in a future version, prefer that over either approach.

## Implementation outline

The work is small — one PR's worth — and isolated to the TTS init path.

### 1. Locate the CUDA EP registration site

Find where `ort` is initialized today and where the CUDA EP is registered. Likely in `src-tauri/src/tts/...` (a `tts/init.rs` or similar). Confirm the `CCIMP_GPU=cuda` env var is read there.

### 2. Add the CC probe

Before EP registration, when `CCIMP_GPU=cuda`:

```rust
// Pseudocode — adapt to actual cudarc API
fn probe_cuda_supported() -> Result<bool, String> {
    let device = cudarc::driver::CudaDevice::new(0).map_err(|e| e.to_string())?;
    let major: i32 = device.attribute(ComputeCapabilityMajor)?;
    let minor: i32 = device.attribute(ComputeCapabilityMinor)?;
    let cc = (major, minor);
    let supported_ccs = [(7,0), (7,5), (8,0), (8,6), (8,9), (9,0)];  // Update on ort bumps
    Ok(supported_ccs.contains(&cc))
}
```

The supported list is the source of truth for "what does our bundled ort prebuilt understand." Maintain it in a single named const so the `MAINTENANCE.md` instruction can point at the exact symbol.

### 3. Branch on the probe result

```rust
if env::var("CCIMP_GPU").as_deref() == Ok("cuda") {
    match probe_cuda_supported() {
        Ok(true) => {
            tracing::info!("CUDA EP enabled (compute capability {major}.{minor})");
            register_cuda_ep(&mut session_builder)?;
        }
        Ok(false) => {
            tracing::warn!(
                "GPU compute capability {major}.{minor} is not supported by the bundled \
                 ONNX Runtime build. Falling back to CPU inference. \
                 See docs/MAINTENANCE.md for details."
            );
            // Skip CUDA EP registration; CPU EP is the default fallback.
        }
        Err(e) => {
            tracing::warn!("CUDA probe failed: {e}. Falling back to CPU inference.");
        }
    }
}
```

The warning message is the user-visible payoff. Include the CC numbers and a doc reference so users know what to do (upgrade ccImp when an ort bump lands; until then, accept CPU).

### 4. Surface the fallback in the UI

Optional but worth doing: the bottom status bar (or a one-time toast on startup) shows "TTS running on CPU (GPU unsupported)" instead of silently falling back. Use the existing `Toast.svelte` infrastructure. Keep it dismissable; don't repeat on every launch — write a "we've already informed the user about this hardware" flag to settings.

### 5. Document maintenance

Add to `docs/MAINTENANCE.md` under the existing "ort / ONNX Runtime" entry:

> **Supported CC list.** When upgrading `ort` to a new prebuilt, update `SUPPORTED_CCS` in `<file path>` to match the prebuilt's documented compute-capability coverage. Failure to update will cause supported GPUs to fall back to CPU silently. Symptom: users on GPUs known to be supported by the new prebuilt see "GPU compute capability X.Y is not supported" warnings.

## Open questions

- **Multiple GPUs**: `cudarc::CudaDevice::new(0)` probes device 0. If a user has multiple GPUs and ort would have selected a different one, we may probe the wrong device. Decide at implementation time: probe all visible CUDA devices and pass if any is supported, or probe the device ort would actually select. The latter requires inspecting ort's device-selection logic. Likely not worth the complexity for v1; document the limitation.
- **AMD ROCm / Intel oneAPI / Apple MPS**: out of scope. ccImp targets Windows and Linux with NVIDIA, per `DESIGN.md`.
- **What if `cudarc` itself fails to load** (e.g., no NVIDIA driver, no CUDA runtime)? Treat as "probe failed → fall back to CPU." User probably set `CCIMP_GPU=cuda` accidentally on a machine without CUDA. Don't crash; warn and continue.

## Milestone recommendation

**No milestone doc needed.** Single PR, ~100 lines of Rust + a doc update. Implement when the trigger fires (per `FUTURE-FEATURES.md`: "anyone besides the dev box reports the 'registered but no audio' symptom on Blackwell, OR when `ort` upgrades to a version that adds new GPU support and we want the probe to handle the next-gen-GPU regression class generally").

If `ort` upgrades to support Blackwell before this is picked up, this feature is still worth shipping — Blackwell isn't the last new GPU. The probe defends against the next regression class generically.

## Files most likely touched

- `src-tauri/src/tts/...` — init/registration site for CUDA EP (exact path depends on current code structure)
- `src-tauri/Cargo.toml` — add `cudarc` (or equivalent) dep, if not transitively available
- `src/lib/Toast.svelte` (or existing toast invocation site) — surface the fallback message
- `src-tauri/src/settings/schema.rs` — optional "informed about GPU fallback" flag to suppress repeat toasts
- `docs/MAINTENANCE.md` — supported-CC list maintenance entry
