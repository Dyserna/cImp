# ccImp — Feature Inventory

A scannable, one-line-per-feature inventory of everything ccImp does, grouped by
area. Current as of **v0.19.0**. Companion to `MAINTENANCE.md` (which covers the
dependency/component breadth) — this is the *capability* breadth.

## Core App & Terminal
- Multi-tab terminal with a recursive split-pane layout tree (horizontal/vertical)
- Draggable tabs that re-parent between panes while keeping xterm state, scrollback, and PTY
- Resizable splitters with min-size clamping and per-split stored ratios
- Focus management — avatar, audio, and compose all follow the focused pane's active tab
- Named layout presets: save / restore / rename / manage, with a recent-presets list
- PTY management with per-tab scrollback persistence across restarts
- Platform shell detection (Git Bash on Windows w/ registry probe, `$SHELL` on Linux)
- Windows console-window suppression for spawned subprocesses

## Claude Code Tabs
- Cloud Claude tab (subscription OAuth or `ANTHROPIC_API_KEY`)
- Claude (local) tab with `ANTHROPIC_BASE_URL`/`ANTHROPIC_AUTH_TOKEN` injected at spawn
- Local backend support: LM Studio, Ollama, vLLM, llama-server (native Anthropic API)
- "Claude tabs enabled" radio: Cloud / Local / Both
- Per-type tab duplication via the `+` button, cloning live config, auto-named & closable
- Tab context menu: Configure / Restart / Rename / TTS toggle
- Compose submits to the focused pane's active tab

## Shell Tabs
- Custom shell tabs (`Ctrl+T` or end-of-bar `+`) with command/args/cwd config
- Restart-shell without losing the tab; closed-shell overlay with restart button
- Reduced notification set (error / exited) for shells
- Alternative shells documented (WSL, pwsh, cmd, zsh)

## Text-to-Speech
- Local Kokoro-82M ONNX synthesis (Apache 2.0), offline
- `[[TTS]]…[[/TTS]]` markup parsing with sentence segmentation and dedup
- Voice-pack selection auto-discovered from `models/voices/`
- Speed and volume controls
- Speak-selection gesture and speak-all-output per-tab mode
- User-input echo suppression
- WebGPU GPU backend (shipped, vendor-agnostic) with automatic CPU fallback; optional CUDA (not shipped)
- `CCIMP_GPU=cpu` forces CPU

## Speech-to-Text (Dictation)
- Offline dictation via whisper.cpp (MIT), selectable GGML models (default `small`)
- Toggle-button and push-to-talk (hold `Ctrl+Shift`) capture, with debounce + abort-on-other-key
- Microphone device selection; VAD / silence handling; rubato resample to 16 kHz mono
- Transcript appended to the compose overlay for review
- Vulkan GPU backend (shipped, vendor-agnostic) + optional CUDA; CPU default

## Avatar & Visualizer
- Picture/Video avatar with per-state overrides; animated frame-based sprite avatars
- Five states: Idle / Listening / Thinking / Speaking / Error, with crossfade transitions
- Visibility, position, size, opacity controls; nearest-neighbor pixel scaling
- Real-time waveform visualizer (playback + mic), configurable color/width/glow/opacity
- Idle optimization that parks the animation frame when silent

## Compose Overlay
- Slide-up textarea (`Alt+Enter`), submit `Ctrl+Enter`, min/max height, draft persistence

## Task Offload (V8)
- Offload subtasks from cloud Opus to a local/remote llama-server via the `offload_task` MCP tool
- Backend pool with fast/quality tiers; local, remote-LAN, and consent-gated cloud backends
- Router: readiness → tool-need → context budget → tier, with spill-on-busy and fail-over
- Global + per-backend concurrency gates, slot tracking, fast-reject backpressure
- Per-backend tool scope (all / web+docs only); cloud data-egress consent enforced
- Honest in-flight counts published into the tool description
- Warm-pool MCP host (stdio + Streamable-HTTP servers), tool namespacing, read-class-only filter, filesystem confinement, per-server health isolation
- Native tools `read_file` / `code_search` / `run_command` with command security policies
- Loopback endpoint + discovery file with per-launch bearer token; self-contained fallback for headless runs
- Read-only Offload Server dashboard tab: per-backend metrics, queue depth, throughput, request history

## Code Knowledge Graph (V9-01)
- Per-project incremental graph in `.ccimp/graph.db` (CozoDB + SQLite)
- Languages: Rust, TypeScript, JavaScript, Python, Markdown via tree-sitter
- Symbol extraction, call/import graphs, transitive reachability, cross-file name binding
- Markdown/doc-comment parsing + full-text search; semantic search via Qwen3-Embedding + HNSW
- Structural AST pattern search; FS watcher for incremental re-index
- MCP graph tools: `graph_find_symbol`, `graph_callers`, `graph_callees`, `graph_references`, `graph_imports`, `graph_outline`, `graph_transitive`, `graph_search_docs`, `graph_struct_search`
- Reserved Code Graph monitor tab: build status, node/edge counts, embedder health, unified recent-calls history

## Theming & Appearance
- 12 bundled terminal palettes + custom 22-color ANSI palette editor; per-tab palette override
- UI theme selector (Modern Dark, TUI Orange default, TUI Purple, TUI Yellow); external theme/palette files
- Terminal background: solid color or image (opacity/blur/size/tint), global presets + per-tab override
- Configurable terminal font family and size

## Settings & Config
- Portable global `settings.json` next to the exe; per-folder `.ccimp.custom.config.json` overlay (auto-deleted when empty)
- Per-tab settings: command, CLI flags, TTS injection prompt, notification text, appearance
- Migration system with timestamped backups + per-key load validation
- Runtime TTS-markup injection via `--append-system-prompt` (no CLAUDE.md edit)

## Notifications, Shortcuts & Status Bar
- Tab-state announcements (idle / awaiting-permission / question / error; shells: error / exited)
- Done-while-away indicator, focused-tab announcement toggle, global mute
- Fully rebindable, context-aware keyboard shortcuts with conflict detection
- Status bar: mute, announcements toggle, volume, record button, usage meter, layouts popover

## Integration, Monitoring & Build
- Claude Code permission detection (matches the Esc/Tab footer)
- Statusline subcommand (`ccimp --statusline`) with context-window bar + `--settings` overlay injection
- System monitor panel (CPU/mem/GPU via sysinfo + nvml) with graceful degradation
- Anonymous usage tracking with retry/backoff
- Rolling file logs (tracing) with env-filter levels
- Portable Windows zip (single binary, models bundled) + slim no-models variant; Git LFS models w/ checksum verification; CI release workflow
