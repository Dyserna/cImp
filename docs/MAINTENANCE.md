# Maintenance & Update Notes

Living list of dependencies and runtime concerns to revisit periodically. Each item: what to check, why it matters, where to look.

**Scope.** This document is the *maintenance-run* doc: the procedure and run
log, the dependency & component inventory, the deep-dive "what to check on
bump" notes, the per-feature residual limitations to periodically re-check, the
open-spike table, and the hand-run live-verify recipes. The **how-it-works
architecture narrative** for each milestone (V8 offload internals, V9-02
grammars, V10–V17 Context Engine / Token Efficiency, V12 inner loop, V22
`run_check`, V23/V25 audit & quality, V13 Workbench, V14 Workflow &
Visibility, V15 Code Graph Parity) lives in **`ARCHITECTURE.md`**; sections
below that need it link to the matching `ARCHITECTURE.md §`.

---

# Maintenance run — procedure

A periodic (roughly monthly, or after any visible Claude Code / OpenCode
update) sweep over everything this document tracks. The output is a **findings
& suggestions report**, not blind bumps — every finding gets an explicit
apply / defer / watch decision before anything is changed.

## Steps

1. **Snapshot.** Note the date, app version (`src-tauri/Cargo.toml` /
   `package.json`), branch state, and the harness versions
   (`claude --version`, `opencode --version`).
2. **Dependency scan (local).** Run the commands in *How to check for updates*
   below (`cargo outdated -R`, `cargo update --dry-run`, `npm outdated`).
   Compare against the inventory tables in this doc; note any rows where the
   doc's pin column has drifted from reality (the doc is only trustworthy if
   the run keeps it true — update the tables and the "as of vX" header).
3. **Deep-dep changelogs.** For the hairy deps with their own sections (`ort`,
   `whisper-rs`, tauri, tree-sitter grammars, cozo), read release notes for
   anything newer than the pin — looking for: fixes we're waiting on, breaking
   API changes, and smoke tests the
   dep's section says to re-run on bump.
4. **External components.** llama.cpp releases (offload + embedding server),
   newer offload/embedding model releases (Qwen line), Kokoro/whisper model
   cards, the ddg/context7 MCP servers, toolchain cadence (Vulkan SDK, LLVM,
   CUDA/cuDNN) — per the *External runtime components* table.
5. **Harness watch (Claude Code + OpenCode).** Read both changelogs since the
   last run. Two lenses:
   - **Behavior changes / contract drift** — anything touching the contracts in
     *Claude Code / OpenCode CLIs — hook & plugin behavior contracts*: hook
     events & payload shapes, MCP server spawning (esp. the open "session id
     inside MCP tool calls" gap), `--settings` overlay, statusline JSON,
     transcript JSONL shape (incl. subagents), permission-prompt text, the
     OpenCode plugin API (`chat.message`, `tool.execute.*`,
     `OPENCODE_CONFIG_CONTENT`). If anything moved, run the ~10-min check and
     if needed the spike recipes in that section.
   - **New capabilities** — new hook events, plugin/skill mechanisms, MCP
     features, CLI flags that would let an existing cImp feature be built
     better (e.g. a supported API replacing a scraped/undocumented one) or
     enable a new feature.
6. **Feature-opportunity scan.** For each major feature area (TTS/STT, offload,
   code graph/intelligence, audits, usage tracking, TUI), ask: has anything
   found in steps 3–5 unlocked a better approach? Also re-check the
   *Feature-area maintenance notes* section (every "residual limitation /
   periodically re-check" item lives there), the *Open spikes & unverified
   contracts* table, and *Known runtime issues to revisit*.
7. **Report.** Produce a findings list, each tagged:
   - `bump-now` — safe update, do it (one ecosystem at a time, per the bump
     policy) — with the smoke tests to run;
   - `bump-with-care` — update wanted but has migration/breakage risk; needs
     its own task;
   - `contract-drift` — harness behavior moved; run the affected spike recipe
     before trusting the feature;
   - `feature-idea` — new capability worth a milestone/spec;
   - `watch` — nothing to do yet, re-check next run.
   Every `bump-with-care` / `contract-drift` finding gets an explicit
   close-or-defer decision recorded with the report.
8. **Record.** Append a line to the run log below; fold table updates into this
   doc; update memory if a locked decision changed.

Delegation note: steps 3–5 are independent research — fan them out (release
notes + changelog reading), keep step 7's synthesis and decisions in the
orchestrating session.

## Run log

| Date | App version | Claude / OpenCode | Outcome |
|---|---|---|---|
| 2026-08-04 | v0.49.1 (develop) | 2.1.221 / 1.18.1 | First formalized run. Report: `docs/reviews/maintenance-run-2026-08-04.md` (10 contract-drift items, 8 adoption candidates, bump batches, 9 decisions). Doc split → `ARCHITECTURE.md`; inventory refreshed; ort/MSRV/schema facts corrected. |

---

# Dependency & component inventory

A complete, scannable inventory of everything cImp depends on, for periodic
"is there a newer version?" passes. Version columns reflect the pins in
`src-tauri/Cargo.toml` and `package.json` as of **v0.49.1 (2026-08-04)**. The
*Dependencies to track* sections below cover the gotchas for the hairy ones
(`ort`, `whisper-rs`, and the Claude Code / OpenCode CLI contracts); the
per-feature check-on-change items are in *Feature-area maintenance notes*
further down, and the architecture behind them is in `ARCHITECTURE.md`. This
inventory is the breadth; those are the depth — update both when you bump.

## How to check for updates

```bash
# Rust crates — shows newest compatible + newest available per dep
cargo install cargo-outdated        # one-time
cd src-tauri && cargo outdated -R   # -R = root deps only (skip transitive noise)
cargo update --dry-run              # what a `cargo update` would move (semver-compatible)

# Frontend (npm)
npm outdated                        # current vs wanted vs latest
npx npm-check-updates               # proposes package.json bumps (review, don't blind-apply)

# Tauri toolchain sanity
cargo tauri --version ; node --version ; rustc --version
```

Bump policy: move one ecosystem at a time, run `cargo test` + `npm run check` +
`npm test`, and for `ort` / `whisper-rs` / GPU features re-run the smoke tests
called out in their sections. Pinned-exact deps (`ort = "=…"`) need a manual
version edit — `cargo update` will not move them.

## Rust crates (`src-tauri/Cargo.toml`)

| Crate | Pin | Role | Watch / notes |
|---|---|---|---|
| `tauri` | `2` | App shell / IPC / windowing | Features `protocol-asset` + `unstable` (the latter gates `Window::add_child`, the multi-webview API the Preview tab needs). Bump with `@tauri-apps/*` JS + the `tauri-plugin-*` crates together (same major). |
| `tauri-build` (build-dep) | `2` | Tauri codegen | Keep in lockstep with `tauri`. |
| `tauri-plugin-dialog` | `2` | Native file dialogs | Pairs with the JS `@tauri-apps/plugin-dialog`. |
| `tauri-plugin-clipboard-manager` | `2` | Clipboard read/write from the webview | Required because WebView2 denies `navigator.clipboard.readText` in AI tabs. Pairs with the JS `@tauri-apps/plugin-clipboard-manager`. |
| `tauri-plugin-snap-layout` | `1` | Win11 Snap Layouts on the custom title bar | Transparent child HWND returning `HTMAXBUTTON`; real impl on Windows, no-op stub elsewhere, so no cfg gating. Pairs with the JS `tauri-plugin-snap-layout`. |
| `tauri-plugin-opener` | `2` | Open URLs/paths in the default browser/file manager | Preview-tab external links + "open in system browser" fallback; pulls the `open` crate (its Linux zbus/async deps are `cfg(target_os = "linux")`-only). |
| `serde` / `serde_json` | `1` / `1` | (De)serialization everywhere | Stable; rarely needs attention. |
| `tokio` | `1` | Async runtime | Feature-rich pin (`rt-multi-thread,macros,sync,io-*,time,fs,process,net`). |
| `tokio-util` | `0.7` | `rt` helpers | — |
| `portable-pty` | `0.8` | PTY for the embedded terminals | Pre-1.0; check changelog on bump. |
| `thiserror` | `1` | Error derives | — |
| `regex` | `1` | `run_check` diagnostic parsers (tsc / gcc-style `file:line:col:`) | Pure Rust, no C deps. |
| `quick-xml` | `0.41` | Streaming XML reader for the `junit-xml` check parser | Default features; promoted from a transitive dep. Pre-1.0 — check the changelog on bump. |
| `chrono` | `0.4` | Epoch → ISO-8601 for the usage widget's `rate_limits.resets_at` | `default-features = false` + `std` only (no clock/timezone use — keeps the tz-data churn out of the tree). |
| `tracing` / `tracing-subscriber` / `tracing-appender` | `0.1` / `0.3` / `0.2` | Logging + rolling file logs | `env-filter` feature; subscriber API shifts across 0.3.x. |
| `base64` | `0.22` | Asset/data encoding | — |
| `which` | `6` | Locate `claude` / shells on PATH | — |
| `vte` | `0.13` | ANSI/terminal parser (processing layer) | Pre-1.0; tag-scanner depends on its escape parsing. |
| `shlex` | `1` | Shell-word splitting | — |
| `uuid` | `1` | IDs (`v4`) | — |
| `async-trait` | `0.1` | Async `ToolRouter` trait (offload) | — |
| `reqwest` | `0.12` | Usage tracker + offload HTTP | `default-features = false` + `rustls-tls,json` (no system OpenSSL — keeps single-binary). |
| `sysinfo` | `0.36` | System-monitor panel | `default-features = false` + `system,network`. Fast-moving pre-1.0; API churns — re-verify the monitor on bump. |
| `nvml-wrapper` | `0.12` | NVIDIA GPU stats | Loads `nvml.dll` at runtime; degrades to n/a without driver. |
| `misaki-rs` | `0.3` | TTS G2P phonemizer | **Pulls espeak-ng → binary is GPLv3** (see `NOTICE`); needs libclang to build. |
| `ort` | `=2.0.0-rc.11` | ONNX Runtime bindings (Kokoro TTS) | **Exact-pinned.** Features `download-binaries` + `webgpu` (the shipped TTS GPU path). `cuda`+`webgpu` share no prebuilt — enabling both silently falls back to CPU. Wraps ORT 1.23.2; see the deep `ort` section. |
| `bytemuck` | `1` | Zero-copy casts | — |
| `cpal` / `rodio` | `0.15` / `0.20` | Audio output | Pre-1.0; device-enumeration behavior changes across versions. |
| `whisper-rs` | `0.16` | STT (whisper.cpp bindings) | → `whisper-rs-sys 0.15`. See the deep `whisper-rs` section + build toolchain. |
| `rubato` | `0.16` | Mic resample → 16 kHz mono | — |
| `tree-sitter` | `0.26.9` | Code-graph parsing core | Grammar crates ride the `tree-sitter-language` shim, so they need not match this exactly — only the parser ABI must. |
| `tree-sitter-*` grammars (28 crates) | `0.1`–`1.3`, per crate | Per-language parsers for the code graph (V9-02 fan-out) | Individual pins live in `Cargo.toml` — not duplicated here. Each exposes `LANGUAGE*` as a version-independent `LanguageFn` (`tree-sitter-language` shim), so **grammar versions need not track `tree-sitter`'s 0.26** — only the parser ABI must match. Bump grammars individually; a grammar that fails to build against the core is an ABI break, not a semver one. Tier 1 (full call graph): rust, typescript, javascript, python, go, java, c, cpp, c-sharp, php, bash, scala, ocaml, ruby, haskell. Tier 2/3 (struct-search + anchors only): html, css, json, kotlin-ng, swift, sequel, yaml, xml, erlang, r, perl, ada, asm. |
| `cozo` | `0.7.6` | Embedded graph DB (code-knowledge graph) | `default-features = false` + `storage-sqlite,rayon`. **Deliberately omits** `graph-algo` (broken `graph_builder` vs rayon) and `storage-rocksdb` (heavy C++). |
| `ignore` | `0.4` | Gitignore-aware tree walk (indexer) | ripgrep's walker. |
| `notify` | `6` | FS watcher (incremental re-index) | `ReadDirectoryChangesW` on Windows. |
| `rfd` | `0.15` | Native file/folder picker (`graph_ignore_pick` in Settings) | `default-features = false` + `xdg-portal,tokio` — **not** gtk3, so the Linux build doesn't link a second GTK next to Tauri's. |
| `similar` | `2` | Line-level unified diff for the read advisor's diff-substitute (V17) | `default-features = false`, `features = ["text"]` (pure Rust, no C-FFI). Single call site: `graph::context::unified_diff`. |
| `url` | `2` | Parses/classifies Preview-tab navigation targets | Already transitive via reqwest/tauri, so the direct dep adds no tree entries. |
| Windows-only deps *(`cfg(windows)`)* | see notes | Registry probe, process reaping, WebView2 capture | `winreg 0.52` (Git Bash detection in `shell::detect`); `windows-sys 0.59` (Job Object backstop in `process_guard` — version matched to Tauri's to avoid a duplicate); `webview2-com 0.38` (`ICoreWebView2::CapturePreview`, **pinned to exactly what wry 0.55 resolves to** so the COM type is nominally identical, not just GUID-compatible); `windows 0.61` (`SHCreateStreamOnFileW` + `STGM_*`, pinned to match `webview2-com` 0.38.2's own dep). Drift in the last two is a compile error, not a silent misbehavior. |
| `filetime` *(dev-dep)* | `0.2` | Backdates a dir mtime in `attach::tests` (exercises `attach::prune`'s age cutoff) | Already transitive; direct pin adds no tree entries. |

## Frontend / npm (`package.json`)

| Package | Pin | Role |
|---|---|---|
| `@tauri-apps/api` | `^2.1.1` | JS ↔ Rust IPC bridge |
| `@tauri-apps/plugin-clipboard-manager` | `^2.3.2` | Clipboard plugin (JS half) — the only working clipboard read in AI tabs |
| `@tauri-apps/plugin-dialog` | `^2.0.0` | Dialog plugin (JS half) |
| `tauri-plugin-snap-layout` | `^1.0.9` | Snap Layouts plugin (JS half) — keep in step with the Rust crate |
| `@xterm/xterm` | `^5.5.0` | Terminal emulator widget |
| `@xterm/addon-canvas` | `^0.7.0` | xterm canvas renderer |
| `@xterm/addon-fit` | `^0.10.0` | xterm fit-to-container |
| `@xterm/addon-serialize` | `^0.13.0` | xterm scrollback serialization |
| `@sveltejs/vite-plugin-svelte` *(dev)* | `^7.2.0` | Svelte + Vite glue (v7 is the Vite 8-compatible line) |
| `@tauri-apps/cli` *(dev)* | `^2.1.0` | `tauri` build/dev CLI |
| `@tsconfig/svelte` *(dev)* | `^5.0.4` | TS base config |
| `svelte` *(dev)* | `^5.56.5` | UI framework (Svelte 5 / runes) |
| `svelte-check` *(dev)* | `^4.0.5` | Svelte type-check |
| `tslib` *(dev)* | `^2.8.0` | TS runtime helpers |
| `typescript` *(dev)* | `^5.6.3` | Type system (caret resolves to 5.9.3 installed — the pin floor is stale, not the tree) |
| `vite` *(dev)* | `^8.1.4` | Bundler / dev server |
| `vitest` *(dev)* | `^4.1.5` | Test runner |

Keep `@tauri-apps/api` + `@tauri-apps/plugin-dialog` +
`@tauri-apps/plugin-clipboard-manager` + `@tauri-apps/cli` aligned with the Rust
`tauri` major, and `tauri-plugin-snap-layout`'s JS half with its Rust crate.
Svelte 5, Vite 8, and Vitest 4 are majors — read migration notes before bumping
any of them; `@sveltejs/vite-plugin-svelte` majors track Vite's, so those two
move together.

## Native libraries linked/vendored through crates

Not separately installable — they ride in via a crate, but each has its own
upstream cadence worth watching.

| Component | Comes via | Effective version | Watch |
|---|---|---|---|
| ONNX Runtime | `ort = =2.0.0-rc.11` | **1.23.2** (static-linked) | <https://github.com/microsoft/onnxruntime/releases> — pyke prebuilts will never fix CUDA-on-Blackwell (no sm_120 cubins, no PTX through rc.13); the `nvrtx` EP is the Blackwell path. |
| Dawn / WebGPU EP dylibs | `ort/webgpu` prebuilt | tracks the `ort` rc | `webgpu_dawn.dll` + `dxcompiler.dll` + `dxil.dll`; update `release.yml` staging if the set changes. |
| whisper.cpp | `whisper-rs-sys 0.15` (from `whisper-rs 0.16`) | tracks the sys crate | Compiled from source via `cc`/`cmake`; bindgen #599 pitfall (build from PowerShell). |
| espeak-ng | `espeak-rs-sys` (via `misaki-rs 0.3`) | tracks misaki | **GPLv3 source** → propagates to the binary license. Needs libclang. |
| SQLite | `cozo` `storage-sqlite` | bundled by cozo | The code-graph on-disk backend. |

## Build toolchain (host machine + CI)

| Tool | Version / location | Needed for |
|---|---|---|
| Rust | edition 2021, declared MSRV 1.82 (`Cargo.toml rust-version`) — **stale: the real floor is 1.88** (`ort`/`ort-sys` rc.11 and `whisper-rs-sys` 0.15 all declare 1.88); correct the declaration, which also unblocks MSRV-aware resolution (`ignore`, `uuid` are held back by it) | everything |
| Node + npm | LTS (CI: `windows-latest`) | frontend build |
| MSVC | VS 2026, `_MSC_VER` 1950 (`cl.exe`, auto-found by `cc`) | native crates, GPU builds |
| CMake | VS-bundled 4.2.3, on PATH | whisper.cpp + espeak builds |
| Ninja | VS-bundled | `stt-vulkan` shader-gen sub-build (`CMAKE_GENERATOR=Ninja`) |
| LLVM / libclang | `C:\Program Files\LLVM\bin` (pinned in `.cargo/config.toml`) | bindgen for whisper-rs / misaki / espeak |
| Vulkan SDK (LunarG) | `C:\VulkanSDK\1.4.350.0` (`VULKAN_SDK`, pinned in `.cargo/config.toml`) | `--features stt-vulkan` only |
| CUDA toolkit *(optional)* | 13.2 for `stt-cuda`; 12.x + cuDNN 9 for `tts-cuda` | the non-shipped NVIDIA-only GPU features |
| cuDNN *(optional)* | 9.21 (`…\v9.21\bin\12.9\x64`, not on PATH by default) | `tts-cuda` / `ort` CUDA EP only |

The default `cargo build` (CPU-only feature set) needs **none** of the GPU/SDK
rows — only Rust + a C toolchain + CMake + libclang. The Vulkan/CUDA rows apply
only when building those opt-in features.

### Linux build (Ubuntu 24.04) — GPU parity

The Linux release (`release.yml` `build-linux`) builds the same
`stt-vulkan,tts-webgpu` feature set as Windows. Validated on Ubuntu 24.04 (WSL2);
CI runs on `ubuntu-24.04`.

**Distro floor is 24.04, not 22.04.** ort's WebGPU Linux prebuilt is a *static*
`libonnxruntime` compiled against **glibc ≥ 2.38 + libstdc++ from GCC 13/14**
(its objects reference `__isoc23_strtoll@GLIBC_2.38`,
`std::ios_base_library_init()@GLIBCXX_3.4.32`). Ubuntu 22.04 (glibc 2.35, GCC 11)
cannot link or run it, and glibc can't be upgraded in place. ort is the only TTS
runtime, so the whole build+runtime floor is 24.04 (glibc 2.39). The shipped
binary's floor is therefore Ubuntu 24.04+.

Build inputs beyond the obvious Tauri/ALSA `-dev` packages:

| Input | Why | How |
|---|---|---|
| `libwebkit2gtk-4.1-dev libsoup-3.0-dev libgtk-3-dev librsvg2-dev libayatana-appindicator3-dev` | Tauri v2 webview | apt |
| `libasound2-dev` | cpal/rodio (ALSA) — **new on Linux** | apt |
| `cmake clang libclang-dev llvm` | whisper.cpp/espeak-ng cmake + bindgen | apt |
| `LIBCLANG_PATH=/usr/lib/llvm-<N>/lib` | bindgen can't find libclang otherwise | `dirname $(find /usr/lib/llvm-* -name libclang.so ...)` |
| `libssl-dev` | **build-time only** — `ort-sys` build-dep `ureq`→`native-tls`→openssl on Linux; not linked into the binary | apt |
| `glslc` + recent Vulkan headers | whisper's ggml-vulkan shader-gen; Ubuntu ships neither new enough (needs `VK_EXT_layer_settings` etc.) | LunarG apt repo (`lunarg-vulkan-noble.list`) → `shaderc vulkan-headers libvulkan-dev` |
| `espeak-ng-data` | espeak-rs-sys builds only the lib on Linux, not the compiled phoneme tables; `build.rs` copies the system package's data next to the binary | apt |

Two Linux-specific bits in `build.rs`: `find_system_espeak_data()` sources
espeak-ng-data from the system pkg (and warns instead of panicking off Windows),
and `set_linux_origin_rpath()` adds an `$ORIGIN` rpath so the bundled
`libwebgpu_dawn.so` (ort's WebGPU/Dawn runtime dylib — the Linux analog of
`webgpu_dawn.dll`; there is no `dxcompiler`/`dxil` off Windows) resolves next to
the binary. The portable tarball ships `bin/cimp` + `bin/libwebgpu_dawn.so` +
`bin/espeak-ng-data/`.

## External runtime components & models (not in the repo)

Shipped in the portable zip or run as separate services. Not version-managed by
cargo/npm — check their sources manually.

| Component | What / where | Update check |
|---|---|---|
| Kokoro TTS model | `kokoro-v1.0.onnx` + `voices/*.bin` voicepacks (Apache 2.0) — fetched from the `models-v1` GitHub release by `scripts/fetch-models.ps1`, verified vs `models/CHECKSUMS.txt` | HF model card; publish updated blobs with `scripts/publish-models-release.ps1` (bump the tag on changes). |
| Whisper STT model | `ggml-small.bin` (~466 MB, MIT) — fetched from the `models-v1` GitHub release, verified vs `models/CHECKSUMS.txt` | whisper.cpp ggml model releases. |
| `llama-server` (llama.cpp) | offload backend **and** embedding server; user-run, not bundled | <https://github.com/ggml-org/llama.cpp/releases> — rebuild/redownload periodically. |
| Offload model | Qwen3.6-35B-A3B (GGUF, quantized) on the local llama-server | newer Qwen / quant releases. |
| Embedding model | Qwen3-Embedding-4B Q8_0, 2560-dim, on `mcp1:8085` (RTX 3070) | re-embed the graph if you change model/dims (auto-probed). |
| Offload MCP servers | `ddg` + `context7` as Streamable-HTTP endpoints (`172.21.1.11:17201/17202`); plus stdio `git`/`fetch`/`fs`/`context7` | each MCP server's own repo; live-reloadable in Settings → Tools. |
| WebView2 runtime | Windows system component (or installer-bundled) | OS-managed; relevant only if shipping an installer. |
| Claude Code CLI | user-installed, self-updating; hosts the V10–V14 hook contracts (injection, PreCompact, read advisor, post-edit, statusline) | see **Claude Code / OpenCode CLIs — hook & plugin behavior contracts** below; re-check after visible CLI updates. |
| OpenCode CLI | user-installed, self-updating; hosts the generated `.opencode/plugin` (injection + memory feed) | same section below. |

---

## Dependencies to track

### `ort` / ONNX Runtime — GPU TTS via the WebGPU EP (shipped); CUDA broken on Blackwell

- **Current pin:** `ort = "=2.0.0-rc.11"` (`src-tauri/Cargo.toml`), with `features = ["download-binaries", "webgpu"]` set **unconditionally on the dependency** — `webgpu` is not behind a cargo feature. cImp's own `tts-webgpu` cargo feature gates only the *runtime* use of the WebGPU EP in `tts/engine.rs`, not the dep's feature list; the optional `tts-cuda` feature is what adds `ort/cuda`. Wraps **ORT 1.23.2** (verified 2026-08-04 via `ort-sys` `ORT_API_VERSION = 23`; rc.12 wraps 1.24.2, rc.13 wraps 1.28.0). The rc.11 `cuda` prebuilt is hard-linked to CUDA major 12 (`onnxruntime_providers_cuda.dll` references `cudart64_12.dll`, `cublas64_12.dll`, `cublasLt64_12.dll`, `cufft64_11.dll`, `cudnn64_9.dll`); CUDA 13.x won't load with this version.

- **IMPLEMENTED — `tts-webgpu` is the shipped GPU TTS backend.** Kokoro runs on ONNX Runtime's native **WebGPU EP** (Dawn-backed → D3D12 on Windows, Vulkan on Linux, Metal on macOS). Validated on the dev box (RTX 5090 / Blackwell) 2026-06-15: correct output matching the CPU reference, genuinely on-GPU (ORT node-placement logs show WebGPU shader programs for every op, incl. the `ConvTranspose2D` that broke DirectML), **~5× faster than CPU** at steady state. Wired in `tts/engine.rs` as GPU-by-default with automatic CPU fallback, selectable GPU/CPU at runtime via the `tts.device` setting (*Settings → Audio → TTS → Process on*) — mirrors `stt/engine.rs`. Runtime deps: three Dawn dylibs (`webgpu_dawn.dll`, `dxcompiler.dll`, `dxil.dll`) staged into the zip by `release.yml`; `download-binaries` static-links core ONNX Runtime into `cimp.exe` (no `onnxruntime.dll`). Full write-up: `docs/features/FEATURE-tts-webgpu.md`.

- **Using the GPU is a compile-time cImp feature; the default build runs Kokoro on CPU.** Kokoro is near-real-time on CPU, so `default = []` selects **no** GPU EP at runtime and routine `cargo build`/test/rust-analyzer need no GPU SDK. Note this is a *runtime* gate only — because `webgpu` sits unconditionally on the `ort` dependency, even a default CPU-only build downloads/links the **WebGPU ORT prebuilt** rather than the plain CPU one. The cImp features:
  - **`tts-webgpu` (shipped, portable, any vendor)** — declared as `["ort/webgpu"]`, which is now redundant with the dep's own feature list; it is the flag `tts/engine.rs` reads to actually register the WebGPU EP. The release builds `--features stt-vulkan,tts-webgpu`.
  - **`tts-cuda` (optional, NVIDIA-only, not shipped)** — `["ort/cuda"]`. **Mutually exclusive with `tts-webgpu`**: `ort` has no `cuda`+`webgpu` prebuilt, so enabling both silently downloads a CPU-only ORT. Because `webgpu` is unconditional on the dep, `--features tts-cuda` alone already lands in that both-enabled state — treat `tts-cuda` as untested until the dep feature is re-gated. Broken on Blackwell (below).
  - DirectML was evaluated and rejected (Windows-only D3D12, and ORT 1.20's DML EP rejects Kokoro's `ConvTranspose`); the `directml` feature is not enabled.
  - **Follow-up:** the unconditional `webgpu` dep feature makes default (CPU-only) builds pull the WebGPU ORT prebuilt — decide whether to re-gate it behind the `tts-webgpu` feature.

- **Failure matrix** (investigated 2026-05-02 on RTX 5090, driver 596.21, CUDA toolkits 12.2 & 12.9, cuDNN 9.21):

  | EP | Failure | Root cause |
  |---|---|---|
  | CUDA | `cudaErrorSymbolNotFound` on every kernel (Slice, Split, …) | RTX 5090 is Blackwell (sm_120), released **after** ORT 1.20. The prebuilt CUDA EP has no cubin for sm_120; JIT from PTX targeting older arches fails to resolve device symbols on Blackwell. **Toolkit version is irrelevant** — reproduced on both 12.2 and 12.9. |
  | DirectML | `ConvTranspose` E_INVALIDARG (0x80070057) on `/encoder/F0.1/pool/ConvTranspose` | ORT 1.20's DML EP rejects Kokoro's F0-decoder ConvTranspose parameters. No useful config knob; not GPU-specific (DML is broken for this model on any DX12 GPU). |
  | CPU | works | — |

- **Why the failure matrix no longer bites us:** `tts-webgpu` sidesteps both broken EPs — it runs on Blackwell where the CUDA prebuilt can't, and it runs the `ConvTranspose` that DirectML rejects. The matrix above is retained as the rationale for *why* WebGPU is the shipped path. The optional `tts-cuda` build still inherits the CUDA row's Blackwell breakage (per-segment `cudaErrorSymbolNotFound`, silent output) — it's expected to work only on Pascal..Ada, which is why it's neither default nor shipped. See `FEATURE-gpu-robustness.md` for the (still-relevant) CC pre-flight idea for `tts-cuda` users.

- **What to check for on `ort` updates:**
  - The WebGPU EP is flagged **experimental** upstream. On an `ort` bump, re-run the `tts-webgpu` smoke test (`cargo test --features tts-webgpu --bin cimp -- --ignored --nocapture synthesizes`) to confirm Kokoro still produces correct audio and stays on-GPU. Watch <https://github.com/pykeio/ort/releases> and <https://crates.io/crates/ort>.
  - **Upgrading `ort` will NOT fix CUDA-on-Blackwell** (verified 2026-08-04): pyke builds its own ORT with `CMAKE_CUDA_ARCHITECTURES=75;80;90` in every release through rc.13, and `cuobjdump` on the shipped `onnxruntime_providers_cuda.dll` confirms cubins for sm_75/80/90a only with **no PTX** (so no JIT fallback either). The real Blackwell path with pyke prebuilts is the **`nvrtx` EP (TensorRT-RTX)** — JIT-compiles on-device, covers Turing→Blackwell, exists since rc.11 — if `tts-cuda` is ever revisited. Note rc.13 ships no CUDA-12 binary at all (CUDA 13.2 + cuDNN 9.23 only).
  - Watch whether the Dawn dylib set (`webgpu_dawn.dll`/`dxcompiler.dll`/`dxil.dll`) changes — if so, update the staging list in `release.yml` (both zip variants) and the layout in `PACKAGING.md`.
  - Upstream ORT release notes: <https://github.com/microsoft/onnxruntime/releases>.

- **Open follow-ups (not blocking):** validate `tts-webgpu` on a non-NVIDIA GPU (AMD/Intel) when one is available; the cold-start one-time Dawn shader-compile cost (~1.3 s on first synth, paid once by the long-lived engine); and surfacing the active TTS backend in the UI (currently log-only, matching STT — see `FEATURE-tts-webgpu.md` Phase 4). Cross-platform/Linux rationale and the "STT stays on whisper.cpp — do NOT unify runtimes yet" decision live in `FUTURE-FEATURES.md`.

### `whisper-rs` / whisper.cpp — STT build toolchain (V6-01)

- **Current pin:** `whisper-rs = "0.16"` (→ `whisper-rs-sys 0.15.0`) + `rubato = "0.16"`.
- **Build needs a C/C++ toolchain + CMake.** `whisper-rs-sys` compiles
  whisper.cpp from source via the `cc` + `cmake` crates and generates FFI
  bindings with `bindgen` (libclang). On this Windows dev box that means:
  MSVC (`cl.exe`, auto-found by `cc`), **CMake on PATH** (VS bundles 4.2.3 +
  Ninja), and `libclang` at `C:\Program Files\LLVM\bin` — already pinned via
  `src-tauri/.cargo/config.toml`'s `LIBCLANG_PATH` (shared with misaki/espeak).
  No new CI tools: `windows-latest` already has VS + CMake + LLVM and the
  workflow exports `LIBCLANG_PATH`.
- **Known pitfall (bindgen on MSVC):** `whisper-rs-sys` bindgen can emit glibc
  types and fail with a `usize` overflow when it sees MinGW/MSYS headers.
  **Build from PowerShell or the VS x64 Native Tools prompt, never Git Bash**
  (Git Bash's PATH carries `/mingw64/bin`). Validated 2026-06-14: clean build
  from PowerShell, no #599 recurrence. If it bites on a bump: pin a
  known-good version, set `BINDGEN_EXTRA_CLANG_ARGS` to force the MSVC target,
  or commit Windows-target pre-generated bindings.
- **GPU backends are compile-time features; the DEFAULT feature set is empty
  (CPU).** So routine `cargo build`/`cargo test` and rust-analyzer work from a
  plain shell with no GPU SDK / dev-env / generator requirements. GPU is opt-in:
  - **`stt-vulkan` (the release backend, recommended).** whisper.cpp's Vulkan
    backend. Produces a **portable** binary — the only GPU runtime dep is the
    system `vulkan-1.dll` (on every Win10+) — runs on any vendor's GPU and
    falls back to CPU when none is present. `release.yml` builds the zip with
    this, so end users get auto GPU/CPU with nothing bundled.
  - **`stt-cuda` (optional, NVIDIA-only).** ~20-40% faster than Vulkan but not
    portable (imports `cublas64_*.dll`) and build-heavy — see the CUDA note
    below. For local NVIDIA max-perf only; not shipped.
  - Runtime (`stt/engine.rs`): when a GPU backend is compiled, STT uses the GPU
    when `stt.device` is `Gpu` (the default) and **falls back to CPU
    automatically** if GPU init fails or no GPU is present (this is what makes
    the Vulkan binary universal). The `stt.device` setting (*Settings →
    Speech-to-text → Process on*) selects GPU vs CPU at runtime and supersedes
    the old `CIMP_GPU` env var, which is no longer read.

- **Building `--features stt-vulkan` (the saga — three Windows gotchas):**
  1. **Vulkan SDK** (LunarG) provides `glslc` + headers + `vulkan-1.lib`.
     `VULKAN_SDK` is pinned in `.cargo/config.toml` (the installer also sets it
     machine-wide). Pinned version: `C:\VulkanSDK\1.4.350.0` — bump on upgrade.
  2. **MSVC dev environment + Ninja generator.** ggml-vulkan builds its shader
     generator as a nested CMake *ExternalProject*. The VS CMake generator does
     NOT propagate the compiler into that sub-build (`No CMAKE_C_COMPILER`), so
     force `CMAKE_GENERATOR=Ninja` and build with `cl.exe` on PATH (a VS x64
     Native Tools prompt, or `vcvars64.bat` sourced). `CL=/FS` serializes PDB
     writes. NOTE these are env-only and intentionally NOT in `.cargo/config.toml`
     (that would force every CPU build through Ninja+dev-env too).
  3. **MAX_PATH on a deep repo.** The nested shader-gen path is ~264 chars from
     this repo's deep location and `cl` fails (`C1041`) even with
     `LongPathsEnabled=1`. Local fix: build with a short `CARGO_TARGET_DIR`
     (e.g. `C:\ct`). CI is unaffected — the runner path (`D:\a\cImp\cImp`) is
     short enough. Validated 2026-06-14: with all three, a local Vulkan build
     produces a clean binary importing **only** `vulkan-1.dll` (no CUDA DLLs).
- **CI (`release.yml`):** a `Setup MSVC dev environment` step (`ilammy/msvc-dev-cmd`)
  + an `Install Vulkan SDK` step (LunarG silent installer, sets `VULKAN_SDK` /
  PATH), then the build sets `CMAKE_GENERATOR=Ninja` + `CL=/FS` and runs
  `--features stt-vulkan`. If CI ever hits the MAX_PATH wall, add a short
  `CARGO_TARGET_DIR` and update the staging-copy paths.

- **Optional CUDA path (`--features stt-cuda`) — kept for local NVIDIA only:**
  `nvcc` gates the MSVC host version in `crt/host_config.h`. This box has only
  MSVC 14.50 (VS 2026, `_MSC_VER` 1950); CUDA 12.x rejects `>=1950`, **CUDA 13.2
  accepts** (`<1960`). So a CUDA build must use 13.2, and **CUDA 13.2's `bin`
  must be the first CUDA dir on PATH** (the VS-generator MSBuild CUDA
  integration injects an include path from the first CUDA bin; a 12.x there
  pulls its rejecting header even when nvcc is 13.2). That PATH entry also
  supplies the load-time `cublas64_13.dll`. Auto-detects `sm_120a` (the 5090's
  Blackwell arch — works where `ort`/Kokoro's prebuilt CUDA can't). This is why
  `stt-cuda` is NOT the default or shipped: too much setup, not portable.
- **What to check on bumps:** the `whisper-rs` API has shifted across releases
  (e.g. segment text moved to `WhisperState::get_segment(i).to_str_lossy()` in
  0.16). Re-verify `FullParams` / `WhisperContextParameters` / `WhisperState`
  against `src/stt/engine.rs` when bumping. Watch
  <https://codeberg.org/tazz4843/whisper-rs> — **the GitHub repo is archived**
  (moved to Codeberg 2026; the GitHub releases page will never update again).
  No changelog/tags exist upstream — diff commits on bump. `whisper-rs-sys`
  0.15.0 vendors whisper.cpp **v1.8.3** (upstream is at 1.9.x — v1.9.0 added
  NVIDIA Parakeet support via the same ggml model workflow).

### Claude Code / OpenCode CLIs — hook & plugin behavior contracts (V10–V14)

The two agent harnesses are user-installed, auto-updating CLIs that cImp does
**not** pin — yet several features depend on undocumented or loosely-documented
behavior contracts that a harness update can silently change. **Re-run this
checklist periodically and after any noticeable Claude Code / OpenCode
update** (both CLIs self-update aggressively; `claude --version` /
`opencode --version`).

What each feature depends on, and the early-warning signal that it broke:

| Feature | Contract it depends on | Where wired | Symptom if the contract drifts |
|---|---|---|---|
| Context injection (V10) | `UserPromptSubmit` hook stdout (`hookSpecificOutput.additionalContext`) reaches the model | `context_hook.rs`, overlay in `tabs/config.rs` | Effectiveness "chars injected" keeps growing but injected files are never followed (Advisor follow-rate collapses); agent re-explores constantly |
| Compaction survival (V11-D) | `PreCompact` hook stdout reaches the compaction prompt — spike **D0**; outcome recorded in `harness_versions.d0_status` (still `unverified` until run — see the V16 spike recipes below) | `compact_hook.rs` | Hard to observe (server-side dedup-clear stays correct regardless); post-compaction re-exploration despite the feature being on |
| Read advisor (V11-E) | `PreToolUse` deny's `permissionDecisionReason` is surfaced **to the model** — spike **E1**; outcome recorded in `harness_versions.e1_status` (`"fail"` hard-blocks the advisor: Settings toggle disabled + hook never installed) | `read_hook.rs` | `drift.read_reason.v1` fires (~100% remind→immediate full re-read = bare refusals); `drift.read_hook_silent.v1` fires (remind counter flatlines while large unchanged files keep being re-read) |
| Post-edit checks (V12) | `PostToolUse` hook fires for `Edit`/`Write`/`MultiEdit` with the documented payload shape | `postedit_hook.rs` route `/context/post_edit` | Auto-check diagnostics stop appearing after edit bursts |
| Permission detection | TUI prompt text matches "Esc to cancel · Tab to amend" | scanner (see memory / V2-03) | Permission notifications stop firing; recharacterize via `RUST_LOG=perm_capture=debug` |
| Statusline / usage | `--settings` overlay accepted at spawn; transcript JSONL `usage` fields present | `statusline/mod.rs`, OOB tap | Status bar context/usage goes blank; Usage section stops populating |
| OpenCode injection + memory (V10) | `chat.message` plugin hook + `tool.execute.after`; `OPENCODE_CONFIG_CONTENT` env | generated `.opencode/plugin` | OpenCode sessions stop appearing in Memory; no injection for OpenCode tabs |

**How to check (~10 min):** open a Claude tab with `context_injection` (and,
where enabled, `read_advisor`) on, run a couple of prompts against a large
already-read file, and watch (a) the Code Intelligence → Usage Effectiveness
counters move, (b) Activity logging `remind` events *without* an immediate
identical full `Read` right after, (c) the status-bar context/usage line
populating. For OpenCode, confirm a session shows up under Memory. Any drift:
re-run the spike recipes below before trusting the feature again.

**V16 (2026-07-12) — drift detection is now built in.** The "hardening ideas"
recorded here earlier all shipped as V16:

- **Version tripwire** — the OOB tap records the Claude CLI version from the
  transcript (`harness_versions.claude_last_seen` in the global
  `settings.json`); `opencode --version` is captured at tab spawn. When
  `last_seen ≠ claude_last_verified` the Advisor card raises
  `drift.harness_version.v1` with a **Mark verified** action — click it only
  AFTER re-running the recipes below.
- **Runtime canaries** — `drift.read_reason.v1` (~100% remind→re-read ⇒
  propose disabling `read_advisor`), `drift.read_hook_silent.v1` (large
  re-reads but zero reminds ⇒ hook not firing), `drift.injection_unseen.v1`
  (injection follow-rate ~0%), `drift.usage_fields_gone.v1` (Claude sessions
  without token fields). All on the Advisor card, `src-tauri/src/advisor.rs`.
- **Shim payload validation** — the three shims POST
  `/activity/contract_drift` when required fields go missing (still fail
  open); surfaced as `drift.payload.v1`.
- **Bypass detection** — the transcript tap counts shell reads of
  just-reminded files (`read_advisor`/`bypass` Activity events, est.);
  `drift.read_bypass.v1` proposes disabling the advisor at ≥40%.

**Spike recipes (Feature 0 — record outcomes in
`harness_versions.{e1_status,d0_status}` in the global `settings.json`):**

- **E1 (read advisor deny reason reaches the model).** With the app running
  and `graph.enabled` + `graph.read_advisor` on, open a Claude tab in a
  project with a large indexed file. Have the agent `Read` the file twice in
  one session (second read unchanged). On the second read the hook denies
  with the outline reminder. **Pass:** the model's next message references
  the outline content (it *acts on* the reminder — e.g. answers from it, or
  targets a specific symbol next). **Fail:** the model reports a bare
  permission refusal and immediately retries/hits the same wall (check the
  transcript JSONL for what the model actually received). Record
  `"e1_status": "pass"` or `"fail"`; `"fail"` disables the Settings toggle
  and blocks the hook install until changed back after a harness update.
  A hand edit takes effect on the next tab launch/restart (the spawn path
  re-reads the global file) and in a freshly opened Settings window — no
  app restart needed. Anything other than `"unverified"`/`"pass"` (any
  casing) is treated as a failure — the gate fails closed on typos.
- **D0 (PreCompact additionalContext reaches the compaction prompt).** With
  `compaction_context` on, run a session up to a `/compact` (manual is
  fine). **Pass:** the post-compaction summary retains working-set files /
  pinned notes fed by `/context/compaction` (compare against the block the
  route returned — visible via `RUST_LOG=debug`). **Fail:** summary shows no
  trace of it. Record `"d0_status"` accordingly (informational — a fail
  degrades to a no-op, nothing misbehaves).
- **OpenCode veto (V16 Feature 7 gate, still open).** In a scratch project,
  add a `tool.execute.before` handler to the generated
  `.opencode/plugin/cimp-inject.js` that throws for a known file's read and
  observe whether (a) the read is vetoed and (b) the thrown message reaches
  the model. Pass ⇒ implement the OpenCode read advisor per the V16 spec;
  fail ⇒ record Claude-only as permanent-until-upstream-changes here.

---

## Feature-area maintenance notes

The check-on-change, re-check-periodically and known-limitation items for each
feature area. The **how it works** narrative for all of these lives in
`ARCHITECTURE.md`; each subsection links to its section there.

### Offload (V8)

*Architecture: see `ARCHITECTURE.md` § Offload — backends, warm pool, loopback &
MCP host (V8).*

- **Routing** is `offload/router.rs::select`, a pure function over `BackendView`
  snapshots (readiness → tool-need → context budget → tier/availability). The
  unit tests there encode the expected behavior — update them when changing the
  selection order.
- **Cloud privacy rests on two independent checks** — the router never routes a
  local-data task to a cloud backend, and the agent loop's `NativeRouter`
  filters the `tools` array by scope *and* refuses a disallowed call. Keep
  both; they're tested in `router.rs` and `agent.rs`.
- **Keep the fallback child first-class.** The `cimp --offload-mcp` child
  carries a self-contained offload path for when the app is down (headless
  `claude -p` / cron depend on it), and the shared `router`/`agent` code must
  stay shared so the app path and the child path can't drift. Full explanation:
  `ARCHITECTURE.md` § Warm pool, loopback endpoint & MCP host (V8-03).
- **Open TODO — Windows ACLs on the discovery file.** `<exe-dir>/.cimp-offload.json`
  is `chmod 600` on Unix (best-effort); **Windows ACL tightening is not done**.
  Residual risk: a local process that can read the file can drive offloads and
  read task text. Don't log the token; don't widen the bind off loopback.
- **`offload.global_concurrency` is applied at app launch** — changing the cap
  needs a relaunch. Worth remembering when a "the new cap didn't take" report
  comes in.
- **Remote capability probing.** A remote `llama-server` exposes `n_ctx` via
  `/props`; cloud APIs usually don't and rely on `declared_context`. Re-verify
  after a llama.cpp bump that `/props` still reports `n_ctx` **per slot** (the
  double-divide budget bug came from assuming otherwise) and that a LAN
  llama-server still answers `/health` 2xx.

### Code graph grammars (V9-02)

*Architecture + the step-by-step "add a language" guide: see `ARCHITECTURE.md`
§ Code graph grammars & tags queries (V9-02).*

- On any `tree-sitter-*` grammar bump, run `cargo test`: the
  `every_vendored_query_compiles` test (`graph/tags.rs`) is the tripwire for a
  node or field name that drifted in a grammar update.
- A grammar that fails to **build** against the core is an ABI break, not a
  semver one — grammar versions need not track `tree-sitter`'s own version,
  only the parser ABI must match.
- Vendored queries live in `src-tauri/queries/<lang>/tags.scm`; re-pull from the
  grammar's upstream `queries/tags.scm` when a grammar reshapes its node names.

### Schema versions — graph & settings

Two independent version numbers; don't conflate them.

- **Graph schema — `GRAPH_SCHEMA_VERSION = 5`** (`graph/schema.rs`, verified
  against the constant 2026-08-04). Bump it whenever a `RELATIONS` column
  changes: `GraphIndex::migrate_schema` then drops+recreates the derived
  relations on first open and the normal rebuild repopulates them from source
  (every row is re-derivable, so no data is lost). History — v3 (V11–V14,
  `symbol.is_test`), v4 (V15, `confidence` on `ref` and `edge`), **v5 (V24,
  `usage_stat.origin`)**. Note that v5 is the exception to "just reset and
  re-derive": `usage_stat` survives `reset()`, so the bump *triggers* the open
  path but the column add is a bespoke recreate-and-copy
  (`GraphIndex::migrate_usage_stat_origin`, defaulting existing rows to
  `"session"`). Any future column on a rebuild-surviving relation needs the same
  treatment. Note each bump here.
- **Settings schema — `CURRENT_SCHEMA_VERSION = 28`** (`settings/schema.rs`).
  The current version is **v28: the TUI theme consolidation (2026-08-04)** —
  the four `tui-*` themes were collapsed into one built-in `tui` theme plus a
  user-picked accent (`ui.tui_accent`), and the v27 → v28 migration rewrites any
  persisted `tui-*` theme id. Earlier recent moves: v20 → v21 (V14,
  `TabConfig::Preview`), v22 → v23 (V25, Code Quality tab retired), v25 → v26
  (Graph View tab retired), v26 → v27 (Code Audit tab retired).
- **Retiring a tab takes two changes, not one:** the migration *and* a
  `RETIRED_TAB_IDS` entry for the integrity check — without the latter a stale
  layout overlay resurrects the tab.

### Context Engine memory scoping (V10) — residual limitation

*Architecture: see `ARCHITECTURE.md` § Code Intelligence — Context Engine (V10).*

The `context_recall` / `context_note` / `context_notes` MCP tools resolve a
session by recency, scoped to the calling agent (claude/opencode).

*Residual limitation (periodically re-check):* two tabs of the **same** agent
(e.g. two Claude tabs) on one project still share the same agent scope, so a
`context_note` from one can attach to the other's session, and `context_recall`
can return the other's working set. Full per-tab isolation needs a session
identifier available *inside the MCP tool call* so the tool knows which of
several same-agent sessions is calling.

- **What's missing:** Claude Code exposes a session id to **hooks** (the
  `UserPromptSubmit` payload carries `session_id` — that's how the transcript tap
  and `cimp --context-hook` get it) but **not** to the MCP servers it launches:
  the `cimp --offload-mcp` child is spawned per Claude session yet receives no
  session UUID (no arg, no env var, and no field on the JSON-RPC `tools/call`
  params). So the child literally cannot tell which session is invoking a tool.
- **What to watch for** (any of these closes the gap): a session id / session
  metadata field on the MCP `tools/call` request; a per-session env var set on
  the MCP server process at spawn (like the hook `session_id`); or an MCP
  "elicitation"/context mechanism that carries session identity. Check the MCP
  server docs + the Claude Code hooks/MCP release notes.
- **When it lands:** thread that id into `dispatch_recorded` alongside `source`,
  add `mem_event`/`session` writes keyed by it, and switch the tools from
  `mem_current_session_for(agent)` to an exact session lookup. The recording side
  already stores a real per-session id (`session.session_id`), so only the read
  path's "which session am I" resolution needs to change.

This is the same gap the maintenance procedure's harness-watch step calls the
"session id inside MCP tool calls" item.

### Check parsers & fixtures (V12 / V22)

*Architecture: see `ARCHITECTURE.md` § Code Intelligence — Agentic Inner Loop
(V12) and § Code Intelligence — run_check Generalization (V22) (incl. the
"Adding a `run_check` parser" guide).*

**`checks/` is a dependency surface: parser fixtures need upkeep alongside the
tools they parse.** `checks::parsers` has one parser per shipped `ParserKind`
(`cargo-json`, `tsc`, `eslint-json`, `pytest`, `generic-gcc`, plus the V22
additions `sarif`, `go`, `go-test-json`, `dotnet`, `junit-xml`,
`regex-custom`); each is regex/JSON-shape coupled to that tool's *current*
output format. A `cargo`/`tsc`/`eslint`/`pytest`/… release that changes its
diagnostic JSON shape or line format **silently degrades `run_check` to
zero/garbage groups rather than erroring loudly** — there's no schema
validation against the real tool, only the fixtures in `checks/parsers.rs`'s
test module.

On a bump of any toolchain this repo's own `checks:` config points at:

1. Re-run the parser fixtures (`cargo test`).
2. Add a fresh fixture captured from a real invocation of the bumped tool.
3. Spot-check the parser against that tool's latest `--help` / changelog for
   output-format notes.

The Rust `ParserKind` enum ↔ TS `ParserKind` union tripwire (`checks/mod.rs`)
fails `cargo test` if the two drift, so adding a variant can't silently skip
the frontend — but the editor dropdown list (`checksEditor.ts`'s
`PARSER_KINDS`) is hand-maintained with **no** tripwire; a new parser is
invisible in the UI until it's added there.

### Code Audit & Code Quality scanners (V23 / V25)

*Architecture: see `ARCHITECTURE.md` § Code Audit — Aggregated Security Scanning
(V23) and § Code Quality — Language-Gated Linters (V25).*

- **Nothing is bundled.** Every tool resolves override → project-local
  `node_modules/.bin` (eslint/knip only) → `ebin/` → PATH at scan time; the
  release ships no scanner binaries. A "not installed" chip is expected on a
  fresh machine, not a regression.
- **The Phase B SARIF fixtures are constructed, not captured.** osv-scanner /
  gitleaks / semgrep were not installed when the runner was written, so the
  per-tool fixtures in `audit/runner.rs`'s test module (incl. the osv `artifacts`
  coverage fixture) are faithful hand-built SARIF 2.1.0, not real tool output.
  **Live capture of real output is part of the live-verify recipe** — replace or
  augment the fixtures from a real run once the binaries are dropped in `ebin/`,
  and spot-check each tool's current SARIF shape (rule id → `Diag.code`, level →
  `Diag.severity`, `runs[].artifacts[].location.uri` for coverage) the same way
  the V22 parser fixtures are maintained.
- **Flags and exit codes were web-verified once, at implementation** — re-check
  them on a major bump of any scanner, because `Adapter::classify_exit` and the
  invocation flags encode per-tool quirks: cppcheck needs **≥ 2.16** for SARIF,
  writes to a report file and exits **0 even with findings**; golangci-lint
  needs the **v2** `--output.sarif.path stdout` form (v1's `--out-format`
  errors); typos exits 2, PMD exits 4 for findings (5 is a real error),
  cargo-machete 1, everyone else 1.
- **Semgrep registry slugs die** — a per-tool `ruleset` override exists for
  `semgrep` / `semgrep-quality` / PMD for exactly this reason; re-check the
  configured slug when semgrep scans start failing wholesale.
- **Offline degrades — and the failed chip must say why.** osv-scanner queries
  the OSV API / deps.dev and semgrep downloads its rules on first run, so an
  offline scan can fail. `runner::exit_error_message` appends a trimmed tail of
  the tool's own stderr (falling back to stdout) to the `exited with code N`
  message, surfaced as the failed chip's tooltip — a bare `exited with code N`
  with no tail means the tool printed nothing, not that the excerpt was dropped.

### Usage & transcript taps (V14) — residual limitations

*Architecture: see `ARCHITECTURE.md` § Workflow & Visibility (V14).*

- **OpenCode usage is estimate-only.** OpenCode's `/event` SSE
  `message.updated.properties.info` carries only `{id, role, time}` on the
  pinned version — no token fields — so OpenCode sessions are recorded
  `est_only` from tool-call *input* args. **Revisit if a future OpenCode
  release adds real token fields to `message.updated`**; `oob/opencode.rs`'s
  doc comment names the exact field path to re-check.
- **Sub-agent transcripts have moved once already and could move again.** Two
  layouts are handled (1.x inline `isSidechain:true` lines; 2.x
  `…/<session_id>/subagents/agent-<id>.jsonl` with the launcher tool renamed
  `Task` → `Agent`). `SubagentState::drift_tick` raises
  `drift.subagent_transcripts.v1` for "transcripts moved" or "launcher tool
  renamed", but a **simultaneous rename and relocation is invisible** from that
  vantage. If sub-agent-heavy sessions ever look suspiciously cheap with no
  canary firing, diff a live session's transcript directory against the two
  known layouts first.

### Preview tab (V14) — known limitation

*Architecture: see `ARCHITECTURE.md` § Workflow & Visibility (V14).*

The nav-policy host allowlist and the external-open scheme allowlist apply only
to the **main frame** — wry exposes no subframe-navigation hook, so a
policy-allowed page (a legitimate localhost dev server) that embeds
`<iframe src="https://some-remote-host">` loads that remote content without
either check running. Accepted for a localhost dev-preview surface; **revisit if
wry grows subframe-navigation events**, or reach
`CoreWebView2Frame::NavigationStarting` directly if this ever needs to be
airtight.

---

## Open spikes & unverified contracts

Every `TODO(spike)` and unverified-contract callout in the codebase and this
doc, in one place. "Status" is as of the last maintenance run — update it when a
spike is run, and record the outcome where the last column says.

| Spike | What it verifies | Status | Where recorded |
|---|---|---|---|
| **D0** (`compact_hook.rs`) | That a `PreCompact` hook's stdout `hookSpecificOutput.additionalContext` actually reaches the **compaction prompt** on the pinned Claude Code build. | **unverified** — degrades to a no-op if it fails (server-side dedup-clear + post-compaction flag are correct regardless) | `harness_versions.d0_status` in the global `settings.json`; recipe in *Claude Code / OpenCode CLIs* → Spike recipes |
| **E1** (`read_hook.rs`) | That a `PreToolUse` deny's `permissionDecisionReason` is surfaced **to the model**, not just the user — the whole premise of the read advisor. | **unverified** — gate fails closed; a recorded `"fail"` disables the Settings toggle and blocks the hook install | `harness_versions.e1_status`; recipe in *Claude Code / OpenCode CLIs* → Spike recipes |
| **F0** (`postedit_hook.rs`) | Which JSON field of a `PostToolUse` hook's stdout reaches the model as additional context. | **unverified** — degrades safely; a parked block still drains via the next `/context/retrieve`, and `auto_check` defaults off | `TODO(spike F0)` in `postedit_hook.rs`'s module doc (no settings field); narrative in `ARCHITECTURE.md` § V12 |
| **OpenCode veto** (V16 Feature 7 gate) | Whether a `tool.execute.before` handler in the generated `.opencode/plugin/cimp-inject.js` can veto a read **and** get the thrown message to the model. | **open** — gates whether an OpenCode read advisor is implementable at all | Recipe in *Claude Code / OpenCode CLIs* → Spike recipes; record the outcome in that section (pass ⇒ implement per the V16 spec; fail ⇒ Claude-only, permanent-until-upstream-changes) |
| **E0** (`preview/capture.rs`) | That WebView2 `ICoreWebView2::CapturePreview` produces a pixel-correct PNG (viewport bounds, true CSS-pixel scale, paint timing), plus z-order during a tab drag and focus/keyboard isolation. | **compiles clean, never run against a live instance** | `TODO(spike E0)` comments in `preview/mod.rs` and `preview/capture.rs`; narrative in `ARCHITECTURE.md` § V14 |
| **C3** (`oob/opencode.rs`) | Whether OpenCode's `/event` SSE `message.updated` carries token/usage fields. | **resolved — absent** on the pinned OpenCode; usage stays `est_only` | `oob/opencode.rs`'s module doc records the outcome; re-check on OpenCode releases (see *Usage & transcript taps (V14)*) |
| **F9** (V21 offload grounding) | Grammar-constrained decoding × thinking mode on the offload worker — whether the two can be enabled together without degrading output. | **open, not run** | The V21 milestone spec (`docs/`); run before relying on grammar-constrained offload with thinking enabled |
| **`MAL-*` claim** (V23) | That osv-scanner surfaces OpenSSF malicious-package (`MAL-*`) advisories in a **default** scan. | **high-confidence but unverified** in the original research | Step 3 of *Live-verify recipes* → Code Audit (V23) |
| **Audit SARIF fixtures** (V23) | That the hand-built SARIF 2.1.0 fixtures match what osv-scanner / gitleaks / semgrep really emit. | **constructed, not captured** — replace from a real run during live-verify | *Feature-area maintenance notes* → Code Audit & Code Quality scanners |
| **Harness version tripwire** | That the currently-installed Claude Code / OpenCode build still honors every contract in the drift table. | **re-armed on every version change** — `drift.harness_version.v1` fires until re-verified | `harness_versions.claude_last_seen` / `claude_last_verified`; the Advisor card's **Mark verified** action (click only after re-running the recipes) |
| **`tts-webgpu` on non-NVIDIA** | That the shipped WebGPU TTS backend works on an AMD/Intel GPU. | **open** — no such GPU available on the dev box | *Dependencies to track* → `ort` open follow-ups |

---

## Live-verify recipes

The hand-run verification passes. These are **not** covered by `cargo test` /
`npm test` — they need a real running build, a real agent tab, and in the audit
cases real third-party binaries.

Harness-contract spikes (**E1**, **D0**, **OpenCode veto**) keep their recipes
in *Claude Code / OpenCode CLIs — hook & plugin behavior contracts* above,
because they gate that section's drift table; everything else is here.

### Shared setup

Common to every recipe below — stated once so the per-feature sections only
carry their deltas:

- **A real build running** (`cargo tauri dev`, or a release build), not a test
  harness. Some paths only exist when the app owns the loopback endpoint.
- **A real project open in a tab**, indexed (`graph.enabled` on) for anything
  graph- or context-related.
- **Feature flags** are set in *Settings*; each recipe lists the ones it needs.
  Flags baked into a spawned agent process only take effect on the **next tab
  launch** — restart the tab after toggling.
- **Where to record the outcome:** harness-contract spikes → the
  `harness_versions.*` fields in the global `settings.json`; everything else →
  a line in the maintenance *Run log*, plus any fixture refresh the recipe calls
  for.
- **For both audit recipes (V23 + V25):** `code_audit.enabled` on. Security and
  Quality are **sub-tabs of the one Code audit view** (Tool Activity → Code
  audit); the corresponding settings live in *Settings → Code Audit* under
  **Security tools** / **Quality tools**. Only one scan runs at a time globally.

### Read advisor & token efficiency (V11 / V17)

*Architecture: `ARCHITECTURE.md` § Token Efficiency (V11) / § Token Efficiency II
(V17).* Extra preconditions: `graph.read_advisor` on, and a project with a
**large indexed file**. Same posture as the V16 E1/D0 spikes in the
CLI-contracts section above — run them against a real Claude tab.

- **Diff-substitute** — `Read` a large file, `Edit` it (or edit it in another
  tab), then `Read` it again. The second read should be denied with a unified
  diff headed ``changed since you read it (turn N) — diff against what you
  read:``, not the whole file. Activity shows a `remind` marked `(changed —
  diff substituted)`. Re-editing and re-reading re-arms up to 3×, then passes.
- **Shell interception** (`read_advisor_shell` on) — after a file is reminded,
  `cat FILE` (or `Get-Content FILE`) in a Bash tool call should be denied with
  the reason prefixed `answered without running the command —`. A `head -50
  FILE` / `sed -n 1,20p FILE` should run untouched (residual routes are the
  canary's job). Verify the same file through `Read` and through `cat` yields
  byte-identical advice modulo the prefix.
- **First-read tier** (`read_advisor_first_read_kb=256`) — first `Read` of a
  large *non-code* file (a big `.log` / `.lock` / generated `.json`) with a
  digest already cached is answered with the digest + head/tail sample; the
  first encounter (no digest) enqueues one and passes.
- **Tool surface** — the Effectiveness card's "tool surface" row reads the
  advertised graph-tool size. Note it reads **0 tools** when `graph.enabled` is
  false (nothing is advertised); toggling `lean_tools` should drop the count by
  exactly 5 and the chars by the `LEAN_HIDDEN` descriptors' size.

### run_check parsers (V22)

*Architecture: `ARCHITECTURE.md` § run_check Generalization (V22).* Extra
preconditions: a project with a `.cimp/config.json` overlay you can edit.

- **Test parsers** — add `{ "name": "test", "cmd": "cargo test", "parser":
  "cargo-test", "timeout_secs": 300 }` to `.cimp/config.json`, break a test,
  and `run_check(name:"test")` should return the failure with its `file:line`
  and a counts `Note`, not a raw dump. On a clean run it renders `ok — N
  passed`.
- Repeat the same shape for whichever newer parsers you're exercising
  (`sarif`, `go`, `go-test-json`, `dotnet`, `junit-xml`, `regex-custom`) —
  a file-reading parser additionally needs its `report_file` to resolve
  relative to the check's `cwd`, and `regex-custom` should reject a pattern
  missing a mandatory named group **at save time**, in the UI, rather than
  running to zero diagnostics.
- The remaining V22 live-verification items (detection / auto-configure, the
  ChecksEditor **Test** button) follow the V22 milestone spec's verification
  list.

### Code Audit — security scanning (V23)

*Architecture: `ARCHITECTURE.md` § Code Audit — Aggregated Security Scanning
(V23).* Run by hand before a release. Shared setup above applies.

1. **Fresh clone, no tools on PATH** → the Code audit view (Tool Activity →
   Code audit) shows all three chips
   `not installed`; **Settings → Code Audit → Detect** on each agrees (not found
   on PATH or ebin).
2. **Drop `osv-scanner.exe` + `gitleaks.exe` in `ebin/`** → Detect finds each
   (`✓ <version> — <path>`); **Scan** produces findings against this repo's
   `Cargo.lock` plus a planted test secret on a scratch branch. Confirm the
   findings-present **exit code 1 is classified as findings, not an error**
   (chip reads `✓ N findings`, not `✗ error`), and the scan-coverage line lists
   `Cargo.lock ✓` (and any other lockfile osv-scanner reports).
3. **Verify the `MAL-*` claim** → point osv-scanner at a scratch
   `package.json` + lockfile pinning a known-malicious package version from the
   OpenSSF malicious-packages set; the malicious-package finding must appear via
   osv-scanner in the default scan (this was high-confidence but unverified in
   the research).
4. **`pipx install semgrep`** → the semgrep chip lights up on the next scan and
   its SARIF findings merge into the table. The adapter forces `PYTHONUTF8=1`
   in the child env (semgrep's beta Windows support mangles output otherwise) —
   confirm the merged rows aren't mojibake.
5. **Copy-to-agent** → select 2 findings → **Copy selected** → paste into a real
   Claude tab prompt; the markdown formatting is intact and the paths are
   project-relative (so the agent can click/act on them).
6. **Cancel mid-scan** → start a scan with semgrep running, hit **Cancel**; the
   running children are killed and already-completed tools keep their findings
   (partial results retained).
7. **Timeout path** → set the scan timeout to 5s and run semgrep on a large repo;
   semgrep's chip goes `✗ error` with a "timed out after 5s" message while the
   other tools are unaffected (the timeout is per-tool, not per-scan).
8. **Capture real SARIF while you're here** — the shipped fixtures are hand-built
   (see *Feature-area maintenance notes* → Code Audit & Code Quality scanners);
   save a real run's output and refresh `audit/runner.rs`'s test module from it.

Troubleshooting a failing chip: an offline box legitimately breaks osv-scanner
(OSV API / deps.dev) and first-run semgrep (rule download). The chip's tooltip
appends the tool's own stderr tail — a bare `exited with code N` with **no**
tail means the tool printed nothing, not that the excerpt was dropped.

### Code Quality — language-gated linters (V25)

*Architecture: `ARCHITECTURE.md` § Code Quality — Language-Gated Linters (V25).*
Run by hand before a release. Shared setup above applies. For each tool: install
it per its hint, then **Detect** in **Settings → Code Audit → Quality tools**,
then scan a small fixture project of that language and confirm the finding lands
in the **Quality** sub-tab:

1. **oxlint** (`npm i -g oxlint`) → scan a JS/TS fixture with an obvious lint
   error (`==` / unused var); SARIF findings appear.
2. **ruff** (`pipx install ruff`) → scan a `.py` fixture (unused import); findings
   appear.
3. **golangci-lint** (`go install github.com/golangci/golangci-lint/v2/cmd/golangci-lint@latest`)
   → scan a `go.mod` module; confirm the **v2** `--output.sarif.path stdout`
   invocation works (v1's `--out-format` would error).
4. **cppcheck ≥ 2.16** (`winget install Cppcheck.Cppcheck`) → scan a `.cpp`
   fixture with a null-deref/style issue; confirm findings land **even though the
   process exits 0** (report-file transport, not stdout/stderr).
5. **typos** (`cargo install typos-cli`) → **works on every project**: scan THIS
   repo, expect the spell-check notes; confirm the JSONL `type: "typo"` records
   render as note-severity rows.
6. **eslint** (project-local `npm i -D eslint` + an `eslint.config.js`) → confirm
   Detect and the scan resolve **`node_modules/.bin/eslint` first**, not a global
   install; JSON reporter findings appear.
7. **knip** (project-local `npm i -D knip`) → scan a `package.json` project with an
   unused export/dep; project-local `.bin/knip` is preferred; JSON findings appear.
8. **cargo-machete** (`cargo install cargo-machete`) → scan a crate with an unused
   dependency in `Cargo.toml`; the `MacheteText` parser anchors the finding to the
   `Cargo.toml`.
9. **PMD** (`pmd.bat` on PATH/ebin, needs a JRE) → scan a `.java` fixture; SARIF
   findings appear; exit 4 classifies as findings, not error.
10. **Roslyn analyzers** (default-disabled; enable it, `.NET SDK` installed) → set
    its **per-tool timeout to ~1200 s** first, scan a `*.csproj`; it runs a real
    `dotnet build` (restores packages, writes obj/bin) and the `/p:ErrorLog` SARIF
    report merges. Confirm it does **not** run when left disabled.
11. **semgrep (quality)** (default-disabled; enable it, `pipx install semgrep`,
    online) → scans with `--config p/r2c-best-practices`; findings show under the
    **Quality** sub-tab only — the Security semgrep entry stays pure SAST.

**Gating checks (this repo has no Java/Go/Python):**

12. Open the Code audit view's **Quality** sub-tab in *this* repo → the **PMD /
    golangci-lint / ruff** chips are **absent** (census sees no `.java`/`.go`/`.py`),
    and the muted "**n tools hidden — not applicable to this project**" line
    accounts for them. oxlint/eslint/knip/cargo-machete/typos DO show (this repo
    has `.ts`, `package.json`, `Cargo.toml`). This holds **before any scan** —
    tab mount takes a census via `audit_refresh_census`.
13. In **Settings → Code Audit → Quality tools**, the gated-off tools
    (PMD/golangci-lint/ruff) show the "**not applicable to the current
    project**" hint — Settings never hides them (global config). Like the chips,
    the hints appear without a scan (Settings open refreshes the census; with
    the feature disabled the census stays empty and no hint shows).

**Quality auto-selection:**

14. Fresh config (auto mode on): open Settings → Code Audit in *this* repo →
    the Quality group reads "Selection: **automatic**" and the checkboxes match
    the project: oxlint/eslint/knip/cargo-machete/typos ON;
    ruff/golangci-lint/cppcheck/PMD OFF; dotnet-analyzers/semgrep-quality OFF
    (heavyweights stay opt-in).
15. Untick one quality tool (e.g. typos) → the group flips to manual mode (the
    "Auto-select for this project" button appears) and a later scan does NOT
    revert the edit. Press the button → selection snaps back to step 14's state
    and the automatic note returns. Security checkboxes are never touched.

**Upgrade checks:**

16. Take a pre-V25 config (`code_audit.tools` with only the three security ids —
    e.g. a v0.43/v0.44 `settings.json`), launch → the Quality sub-tab and the
    Settings Quality group are populated with all eleven tools on first load (the
    reconcile), and any customized security entry (disabled / custom path / extra
    args / timeout) is preserved verbatim.
17. Take a v0.45.0 config that has the old separate `code-quality` tab entry in
    `tabs` (schema 22), launch → the migration cascade prunes it (and, since
    schema 27, the `code-audit` entry too): ONE Code audit view with Security |
    Quality sub-tabs inside Tool Activity, no stray closable "Code Quality" or
    "Code Audit" shell tab.

**Cross-sub-tab lock + toggle stability:**

18. Start a scan in the **Quality** sub-tab; switch to **Security** → its Scan
    button is disabled and shows "**waiting — quality scan running**" (one scan
    at a time globally). The reverse holds too.
19. Toggle `code_audit.enabled` off and on a few times in Settings → the Code
    Audit tab disappears/reappears cleanly with **no rapid active-tab flapping**
    (the 2026-07-16 echo-suppression fix in App.svelte's active-tab back-sync).

---

## Known runtime issues to revisit

### ~~Spurious `[[TTS]] tag exceeded max-hold without close` warnings~~ — OBSOLETE (resolved by removal, 2026-08-04 run)

The issue lived in the PTY tag-scanner (`src-tauri/src/processing/`), which the
V20 fullscreen-TUI rework **deleted entirely** — TTS now comes out-of-band from
the transcript JSONL / `/event` SSE (`src-tauri/src/oob/`), so the max-hold
flush path no longer exists. Residue: `ProcessingSettings { stability_timeout_ms,
max_hold_ms }` still exists in `settings/schema.rs` (~line 4020) with nothing
reading it — a candidate for removal in the next settings-schema bump (needs a
migration entry, not just deletion).
