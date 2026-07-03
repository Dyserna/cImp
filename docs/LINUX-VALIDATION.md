# Linux Validation Runbook (native laptop)

Hands-on checklist to sign off the Linux build on a **real** Ubuntu 24.04+
machine — the things WSL2 can't fully exercise (native webkit2gtk input, a real
GPU, ALSA audio). Target here: the Intel-iGPU laptop. Intel exposes Vulkan via
Mesa/ANV, so this validates the GPU paths (correctness, not speed) **and** the
CPU fallback.

## 0. Get a build onto the laptop

Either:
- **CI artifact** — once `release.yml` has run for a tag, download
  `cimp-portable-linux-x64-<tag>.tar.gz` from the GitHub release, or
- **Dev build** — copy the binary + its bundled libs from the build box:
  `src-tauri/target/release/{cimp,libwebgpu_dawn.so,espeak-ng-data}` into a
  `bin/` folder, with `models/` as a sibling of `bin/`.

## 1. Runtime prerequisites

```bash
sudo apt-get update
sudo apt-get install -y libwebkit2gtk-4.1-0 libasound2t64 libvulkan1 mesa-vulkan-drivers
claude --version        # Claude Code must be on PATH
vulkaninfo | grep -i "deviceName"   # confirm a Vulkan device (Intel) is present
```

## 2. Launch

```bash
tar -xzf cimp-portable-linux-x64-*.tar.gz
cd cimp-portable-linux-x64-*
RUST_LOG=info ./bin/cimp
```
- [ ] Window opens and the UI renders (webkit2gtk).
- [ ] No `libwebgpu_dawn.so: cannot open shared object file` (rpath works).

## 3. Fullscreen AI-TUI input fidelity (highest-risk item)

Open a **Claude** tab (runs the native fullscreen TUI).
- [ ] Mouse click / drag / **scroll** work inside the TUI.
- [ ] **Hold-Alt** bypass gives local (host) mouse selection.
- [ ] Keyboard incl. modifiers passes through; clipboard copy/paste works
      (Ctrl+Shift+C / V) via the tauri clipboard plugin.

## 4. TTS — GPU (Intel Vulkan) and CPU

- [ ] With Settings → Audio → TTS = **GPU**: Claude's prose is spoken; log shows
      the WebGPU EP initialized (Dawn→Vulkan on Intel), not a CPU fallback.
- [ ] Switch **Process on → CPU**: still speaks (takes effect immediately).
- [ ] Audio actually plays through ALSA (not just "playing" in the UI).

## 5. STT

- [ ] Push-to-talk (Ctrl+Shift hold) records; transcript lands in the compose
      overlay. Works on both GPU (whisper-vulkan) and CPU.

## 6. System / GPU stats + offload

- [ ] System-monitor panel shows CPU/mem; **GPU section reads n/a** (no NVIDIA →
      nvml absent) without crashing.
- [ ] If offload is enabled: `llama-server` + MCP host spawn; a native
      `run_command` works.
- [ ] Quit the app, then `pgrep -a llama-server` / `pgrep -a cimp` → **no
      orphaned children** (note: Linux has no Job-Object backstop; a hard
      `kill -9` of cimp *can* orphan children — that's a tracked follow-up).

## 7. Clean-box check (optional but recommended)

Extract the tarball on a machine that never had the build toolchain and run it —
catches a missing runtime lib the dev box happened to have. Note any
`error while loading shared libraries` and add the lib to §1 / the README.

---

Record pass/fail per item. The milestone is "done" when §2–§5 pass on real
hardware. GPU here is Intel (validates the vendor-agnostic claim + CPU fallback);
NVIDIA/AMD speed is out of scope.
