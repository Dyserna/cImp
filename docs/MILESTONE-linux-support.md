# Milestone: Linux Support

> **Release tag:** TBD by the user at ship time (the V-series numbering is independent of the git tag). This milestone is **cross-cutting** rather than a new feature pillar: it adds a second target OS to an app that has shipped Windows-only since V1. It supersedes the standing "Linux validation deferred" decision — Windows remains the primary target, but Linux becomes a supported, CI-built second target.

## Purpose

Make cImp build, run, and ship on Linux (x86-64, targeting Ubuntu 22.04+/Debian-based first) as a portable tarball, with the same core capabilities as the Windows build: multi-tab PTY terminals, the fullscreen AI TUI tabs, TTS (Kokoro), STT (Whisper), the code graph, and the local offload stack.

The groundwork is already in place. cImp was written with `#[cfg(windows)]` / `#[cfg(unix)]` branches from the start — nearly every platform-specific site in the Rust backend **already has a Unix arm**, and the frontend has **no** platform branching. This milestone is therefore mostly **build toolchain, native-dep validation, and release packaging**, not a rewrite of app logic. The two genuine unknowns — the WebGPU TTS prebuilt on Linux, and webkit2gtk input fidelity for the fullscreen TUI tabs — are gated behind an early build-and-run spike (Phase A) so we learn the truth before investing in packaging.

## What already works (audited 2026-07-03, no change needed)

Every platform-gated site in `src-tauri/src/` already carries a Linux branch:

| Concern | File | Linux behavior today |
|---|---|---|
| Shell detection | `shell/detect.rs` | `resolve_unix()` uses `$SHELL`; Git Bash registry probe is Windows-only |
| Command name resolution | `pty/resolve.rs` | `cfg(not(windows))` arm returns the bare name (no `.exe`/`.cmd` suffixing) |
| Child-process reaping | `process_guard.rs` | Windows Job Objects; a documented **no-op stub** on Linux |
| Console-window suppression | `offload/mcp_host.rs`, `offload/supervisor.rs`, `offload/tools/run_command.rs` | `CREATE_NO_WINDOW` creation-flag gated `cfg(windows)` |
| Snap Layouts / square-corner window shaping | `ipc/commands.rs`, `tauri-plugin-snap-layout` | Snap-layout plugin compiles as a no-op stub off Win11; `set_window_square_corners` is a no-op on non-Windows |
| Statusline exe path | `statusline/mod.rs` | 8.3 short-path is Windows-only; the long path stands on Linux |
| Secret / discovery file perms | `settings/mod.rs`, `offload/loopback.rs` | `cfg(unix)` tightens perms to `0600` |
| PTY | `portable-pty` (0.8) | Uses `openpty` on Linux |
| Model / theme / avatar / ebin resolution | `pty/resolve.rs`, loaders | All relative to the exe dir; layout-agnostic |
| Frontend | `src/**` | No `win32` / `navigator.platform` branching — renders identically under webkit2gtk |

So the Rust logic changes in this milestone are expected to be **small**: build-staging gates and a per-OS TTS-feature/runtime-lib selection, not new subsystems.

## Decisions locked (from the 2026-07-03 investigation)

- **Distro baseline:** Ubuntu 22.04 LTS / Debian-family first (matches `ubuntu-latest` on GitHub Actions and the webkit2gtk-4.1 availability there). Other distros are best-effort, not validated.
- **Package format:** portable **tarball** (`.tar.gz`) mirroring the Windows portable-zip layout (`bin/`, sibling `ebin/`, `models/`, `themes/`, `palettes/`, `avatars/`, `sprites/`). AppImage / `.deb` are explicitly out of scope for this milestone (candidates for a follow-up) — the tarball reuses the existing staging logic most directly.
- **GPU story:** keep it portable and vendor-agnostic where the Windows build already is. `stt-vulkan` (whisper.cpp Vulkan backend) is the target STT feature on Linux — only runtime dep is system `libvulkan.so.1`. TTS GPU is the **open question** (see Phase A / Risks): resolve `tts-webgpu` on Linux, else fall back to CPU TTS (portable) or `tts-cuda` (NVIDIA-only) for the Linux release.
- **No new Python / sidecars** — unchanged project constraint. All native deps stay C-FFI or pure-Rust.
- **Windows stays primary.** Nothing in this milestone regresses the Windows build; all new build inputs are additive and OS-gated.

## What This Milestone Delivers

The phases front-load the two real unknowns (does it build, does the TUI feel right under webkit2gtk) before any packaging work.

**Phase A — Build + run spike (resolve the unknowns first)**

1. **System-dep bring-up** on an Ubuntu 22.04 box/VM. Install the Tauri v2 Linux prerequisites and cImp's native-dep build inputs:
   - Tauri/webview: `libwebkit2gtk-4.1-dev`, `libsoup-3.0-dev`, `libgtk-3-dev`, `librsvg2-dev`, `libayatana-appindicator3-dev`, `build-essential`, `pkg-config`.
   - Audio (**new on Linux** — cpal/rodio use ALSA): `libasound2-dev` at build, `libasound2` at runtime.
   - TTS phonemizer (misaki-rs → espeak-ng-sys): `cmake`, `clang`/`libclang-dev`, `llvm` (same requirement as the Windows build — needed for bindgen).
   - STT Vulkan (`stt-vulkan`): `libvulkan-dev`, plus `glslc` (`glslang-tools` / Vulkan SDK) for whisper.cpp's shader generator.
2. **CPU-first build** to prove the baseline compiles and links end-to-end with no GPU features:
   `cargo build --release` (default features) then `npm run tauri build -- --no-bundle`.
3. **GPU build spike** — attempt `--features stt-vulkan,tts-webgpu` and **record the truth** about each:
   - `stt-vulkan`: expected to build given the Vulkan SDK; confirm the produced binary runs and falls back to CPU with no GPU.
   - `tts-webgpu`: **the pivotal unknown.** Determine whether `ort` `=2.0.0-rc.11`'s `download-binaries` ships a **Linux** WebGPU (Dawn/Vulkan) prebuilt. If yes, capture the exact runtime `.so` names it drops (Linux Dawn is Vulkan-backed — `dxcompiler`/`dxil`/D3D do **not** apply). If no, pick the Linux TTS fallback (CPU default, or `tts-cuda` for the NVIDIA release) and record that decision here.
4. **Run + smoke the app** on Linux under webkit2gtk. Explicitly exercise the memory-flagged touchy paths:
   - **Fullscreen AI TUI tabs** — mouse/scroll forwarding and the hold-Alt local-mouse bypass (per `project_fullscreen_tui_milestone` / `project_webview_clipboard_wheel`). Confirm wheel→synthesized-event and pointer behavior are usable under webkit2gtk.
   - **Clipboard** — the tauri clipboard plugin path (WebView2 denied `navigator.clipboard.readText`; verify webkit2gtk behavior through the plugin abstraction).
   - **Audio out** (Kokoro playback) and **audio in** (STT capture) through ALSA.
   - **nvml** GPU stats: `nvml-wrapper` loads `libnvidia-ml.so.1` at runtime (ships with the NVIDIA driver); confirm it degrades to `gpu: None` cleanly when absent.
   - **Offload stack** — spawn a `llama-server`, MCP host, and a native `run_command`; confirm no orphaned children on hard-kill given `process_guard` is a no-op on Linux (see Risks).

   **Exit gate for Phase A:** a Linux binary that launches, runs a Claude tab, speaks TTS, and dictates STT — with a written verdict on the `tts-webgpu` question. Everything downstream depends on this.

**Phase B — Code / build gating (small, surgical)**

5. **Per-OS release-staging gates.** The release logic in `release.yml` hard-requires the Windows Dawn DLLs (`webgpu_dawn.dll`, `dxcompiler.dll`, `dxil.dll` — the latter two are D3D12-only and do not exist on Linux). Whatever Phase A determines the Linux TTS runtime is (a set of `.so`s, or nothing for CPU), the staging step must select it per-OS rather than failing on the missing Windows DLLs.
6. **Any residual `cfg` gaps** the spike surfaces (e.g. a Windows-only import that slipped without a Unix arm, an unconditionally-`.exe` path). Expected to be near-zero based on the audit, but this is where they land.
7. **`build.rs` review on Linux** — confirm the espeak-ng-data copy (`find_espeak_data` / `copy_espeak_data`) and the themes/palettes/ebin copy resolve correctly under the Linux `target/{profile}` layout (path logic is OS-agnostic, but the espeak-rs-sys build-dir hash walk should be verified on a real Linux build).

**Phase C — Linux release pipeline (`release.yml`)**

8. **New `build-linux` job** (`runs-on: ubuntu-latest`) alongside `build-windows`:
   - `apt-get` the Phase A system deps; set up Rust + Node identically.
   - Build with the Phase-A-confirmed feature set (`stt-vulkan` + the chosen TTS path).
   - Stage the **tarball** layout: reuse the Windows staging structure but with the Linux binary (`cimp`, no `.exe`), the Linux TTS runtime `.so`s (if any), and Linux builds of the `ebin` tools (`broot`, `rustnet` — swap the download URLs to their `x86_64-unknown-linux-*` assets).
   - Produce **full** and **slim/no-models** tarballs mirroring the two Windows zips; compute SHA-256s; attach to the same GitHub release.
9. **Portable README** variant for Linux (`scripts/portable-readme-linux.txt`) documenting the runtime system libs the user needs (`libwebkit2gtk-4.1-0`, `libasound2`, `libvulkan1`, optional NVIDIA driver for nvml/CUDA).
10. **Version-triple check** and LFS/model handling are already OS-agnostic in the workflow — reuse as-is.

**Phase D — Docs + validation sign-off**

11. **`docs/MAINTENANCE.md`** — add a Linux build section (system deps, feature flags, the espeak/whisper/Vulkan toolchain notes) paralleling the existing Windows toolchain notes.
12. **`docs/PACKAGING.md`** — document the Linux tarball layout and the per-OS TTS-runtime staging.
13. **Live validation pass** on Linux against the Test Plan below; record results (this milestone is not "done" until the fullscreen-TUI + audio paths are live-verified on Linux, matching the project's live-verification bar).

## What This Milestone Does NOT Do

- **No AppImage / `.deb` / Flatpak.** Portable tarball only. Native packaging is a follow-up.
- **No Wayland-specific work** beyond what webkit2gtk/GTK give for free (X11 and XWayland are the assumption).
- **No non-x86-64 targets** (no ARM64 Linux).
- **No distro matrix** — Ubuntu/Debian-family only; RPM-family and rolling distros are best-effort and unvalidated.
- **No macOS.** Out of scope; a separate future milestone.
- **No feature parity investigation for Windows-only cosmetics** (Snap Layouts, square-corner shaping) — they stay no-ops on Linux by design.
- **No change to the Windows build** other than making shared release-staging logic OS-aware.

## Test Plan

**Phase A (spike — the gate):**
- `cargo build --release` (default) succeeds on clean Ubuntu 22.04.
- `npm run tauri build -- --no-bundle` produces a launchable binary.
- `--features stt-vulkan,tts-webgpu` build outcome recorded (build ok? runtime `.so`s? or fallback chosen?).
- App launches under webkit2gtk; a Claude tab runs the fullscreen TUI with usable mouse/scroll + hold-Alt bypass.
- TTS speaks; STT dictates (ALSA in/out); clipboard copy/paste works via the plugin.
- nvml present → GPU stats; nvml absent → `gpu: None`, no crash.
- Offload: `llama-server` + MCP host + `run_command` spawn and are cleaned up on app exit (note any orphans for the Risks follow-up).

**Phase B/C (build + package):**
- `release.yml` `build-linux` job builds green on `ubuntu-latest`.
- Full + slim tarballs extract to the correct portable layout; `cimp` runs from `bin/` and resolves `ebin/` tools, `models/`, `themes/`, `palettes/`.
- Windows job still builds green (no regression from the per-OS staging gate).

**Phase D (sign-off):**
- Full Test Plan re-run from a freshly-extracted tarball on a clean Linux box (not the dev box) to catch missing runtime libs.

## Files Most Likely Touched

- `.github/workflows/release.yml` — new `build-linux` job + per-OS runtime-staging gate (the bulk of the diff).
- `src-tauri/build.rs` — verify (likely no change) the espeak/theming copies on the Linux layout.
- `src-tauri/Cargo.toml` — possibly a `cfg(unix)`-gated dep if the TTS-fallback decision needs one (e.g. an explicit ALSA/openssl-free tweak); expected minimal.
- Small `#[cfg]` fixes in whatever `src-tauri/src/**` files the spike flags (expected near-zero).
- `scripts/portable-readme-linux.txt` — new.
- `docs/MAINTENANCE.md`, `docs/PACKAGING.md` — Linux sections.

## Risks and Open Questions

1. **`tts-webgpu` on Linux (highest risk).** Unknown whether `ort =2.0.0-rc.11` ships a Linux WebGPU (Dawn/Vulkan) prebuilt. Phase A resolves it empirically. **Fallbacks if not:** ship CPU TTS (portable, slower) or `tts-cuda` (NVIDIA-only, not portable) on the Linux release. Does not block the milestone — only decides which TTS feature the Linux zip ships.
2. **webkit2gtk input fidelity for the fullscreen TUI tabs (second-highest risk).** WebView2 → webkit2gtk swaps the entire embedded browser. The AI-tab mouse/scroll forwarding and hold-Alt local-mouse bypass are the most WebView-coupled UX in the app and have only ever been tuned against WebView2. Needs hands-on Phase A validation; may require webkit-specific input tweaks.
3. **No child-reaping backstop on Linux.** `process_guard` is a no-op off Windows (Windows uses Job Objects). If cImp is hard-killed (SIGKILL), spawned `llama-server`/MCP children can orphan. Acceptable for a first release (Phase A notes it); a proper Linux backstop (e.g. `prctl(PR_SET_PDEATHSIG)` or a process group + `killpg`) is a tracked follow-up, not a blocker.
4. **espeak-ng-sys / bindgen on Linux.** Needs `libclang` + `cmake`; the same class of toolchain the Windows build already depends on. Low risk but the espeak-rs-sys build-dir hash walk in `build.rs` should be confirmed on a real Linux build.
5. **`ebin` tool Linux builds.** `broot` and `rustnet` publish Linux assets; swapping the release-download URLs is mechanical. `rustnet` packet capture may need `setcap`/root on Linux (document in the README rather than solve here).
6. **Clean-box runtime libs.** The dev box has more installed than a user's machine; Phase D's fresh-box run is what catches a missing `libwebkit2gtk-4.1-0` / `libasound2` / `libvulkan1` before users do.

## Followups Tracked Elsewhere (candidates)

- AppImage / `.deb` native packaging.
- Linux child-reaping backstop (`PR_SET_PDEATHSIG` / process-group kill).
- Wayland-native validation; ARM64 Linux; macOS.
