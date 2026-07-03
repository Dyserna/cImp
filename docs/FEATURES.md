# cImp — Feature Inventory

A scannable, one-line-per-feature inventory of everything cImp does, grouped by
area. Current as of **v0.30.0**. Companion to `MAINTENANCE.md` (which covers the
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

## AI Tabs
- Three AI-tool tab types, each running its tool's native fullscreen TUI
- Cloud Claude tab (subscription OAuth or `ANTHROPIC_API_KEY`)
- Claude (local) tab with `ANTHROPIC_BASE_URL`/`ANTHROPIC_AUTH_TOKEN`/`ANTHROPIC_MODEL` injected at spawn
- Claude (local) targets a local Anthropic-compatible proxy (LiteLLM bridging Ollama / LM Studio / vLLM / llama-server)
- OpenCode tab — manages its own providers/credentials; cImp injects only MCP tools + TTS/offload/graph guidance
- "AI tabs enabled" checkboxes (Claude / Claude (local) / OpenCode); Claude only on a fresh install
- Fullscreen interaction: mouse kept shell-like (select-copy, Shift+right-click paste) with a hold-`Alt` bypass to the TUI
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
- Master enable that loads/unloads the Kokoro model (frees memory when off; distinct from mute), built off the async runtime
- Out-of-band sourcing: Claude transcript JSONL tail + OpenCode `/event` stream (no terminal scraping, no `[[TTS]]` markers)
- Markdown reduced to speakable prose (code/tables/tool output/reasoning dropped), sentence-segmented and deduped
- Per-tab speak toggle (gates whether a tab reads its assistant prose), read live; `Esc` stops the current burst
- Voice-pack selection auto-discovered from `models/voices/`
- Speed and volume controls
- Speak-selection gesture (`Ctrl`+right-click)
- WebGPU GPU backend (shipped, vendor-agnostic) with automatic CPU fallback; optional CUDA (not shipped)
- GPU/CPU selectable at runtime in *Settings → Audio* ("Process on"); switching reloads the model, no restart (supersedes the old `CIMP_GPU` env var)

## Speech-to-Text (Dictation)
- Offline dictation via whisper.cpp (MIT), selectable GGML models (default `small`)
- Enable toggle loads/unloads the Whisper model (frees memory when off) and gates the record button + push-to-talk
- Toggle-button and push-to-talk (hold `Ctrl+Shift`) capture, with debounce + abort-on-other-key
- Microphone device selection; VAD / silence handling; rubato resample to 16 kHz mono
- Transcript appended to the compose overlay for review
- Vulkan GPU backend (shipped, vendor-agnostic) + optional CUDA; CPU default

## Avatar & Visualizer
- Picture/Video avatar with per-state overrides; animated frame-based sprite avatars (default `impSprites`)
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
- `offload_task` + graph tools exposed to both Claude and OpenCode tabs (OpenCode via a `--consumer opencode` child)
- Read-only Offload Server dashboard tab: per-backend metrics, queue depth, throughput, request history

## Code Knowledge Graph (V9-01)
- Per-project incremental graph in `.cimp/graph.db` (CozoDB + SQLite)
- Languages (V9-02): full symbol/call graph for Rust, TypeScript, JavaScript,
  Python, Go, Java, C, C++, C#, PHP, Bash, Scala, OCaml, Ruby, Haskell, Kotlin,
  Swift, SQL, Erlang, R, Perl, Ada (generic tree-sitter `tags.scm` engine);
  struct-search for HTML, CSS, JSON, YAML, XML, assembly; Markdown for docs
- Symbol extraction, call/import graphs, transitive reachability, cross-file name binding
- Markdown/doc-comment parsing + full-text search; semantic search via Qwen3-Embedding + HNSW
- Structural AST pattern search; FS watcher for incremental re-index
- MCP graph tools: `graph_find_symbol`, `graph_callers`, `graph_callees`, `graph_references`, `graph_imports`, `graph_outline`, `graph_transitive`, `graph_search_docs`, `graph_struct_search`
- Reserved Code Graph monitor tab: build status, node/edge counts, embedder health, unified recent-calls history

## Theming & Appearance
- 12 bundled terminal palettes + custom 22-color ANSI palette editor; per-tab palette override
- UI theme selector: TUI Orange (default) + TUI Grey, ratatui-style; external theme/palette files
- Terminal background: solid color or image (opacity/blur/size/tint), global presets + per-tab override
- Configurable terminal font family and size

## Settings & Config
- Portable global `settings.json` next to the exe; per-folder `.cimp.custom.config.json` overlay (auto-deleted when empty)
- Per-tab settings: command, CLI flags, TTS speak toggle, notification text, appearance
- Migration system with timestamped backups + per-key load validation
- Capability guidance injected on launch when relevant — offload + code-graph tool hints (Claude `--append-system-prompt`, OpenCode instructions file)

## Notifications, Shortcuts & Status Bar
- Tab-state announcements (idle / awaiting-permission / question / error; shells: error / exited)
- Done-while-away indicator, focused-tab announcement toggle, global mute
- Fully rebindable, context-aware keyboard shortcuts with conflict detection
- Status bar: mute, announcements toggle, volume, record button, usage meter, layouts popover
- Claude usage meter hides (and stops polling) when the Claude tab is disabled

## Integration, Monitoring & Build
- Claude Code permission detection (matches the Esc/Tab footer)
- Statusline subcommand (`cimp --statusline`) with context-window bar + `--settings` overlay injection
- System monitor panel (CPU/mem/GPU via sysinfo + nvml) with graceful degradation
- Anonymous usage tracking with retry/backoff
- Rolling file logs (tracing) with env-filter levels
- Portable Windows zip (single binary, models bundled) + slim no-models variant; Git LFS models w/ checksum verification; CI release workflow
