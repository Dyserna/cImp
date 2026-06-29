# Changelog

All notable changes to ccImp are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Changed

- **OpenCode replaces Aider (V19).** The two Aider AI-tool tabs are replaced by
  a **single OpenCode** tab (`opencode`), launched inline via `opencode --mini`
  with its session config injected through a single `OPENCODE_CONFIG_CONTENT`
  env var. Unlike Claude (which needs a separate local tab because the local
  endpoint is set by a launch-time env var), OpenCode addresses many providers
  as `provider/model` and switches between them in-session from global config +
  credentials — so one tab covers cloud and local and cctts injects no provider
  block. OpenCode reaches the same ccImp capabilities the Claude tabs use — the
  offload tool, the code knowledge graph, and the web-research MCP servers — via
  the injected `mcp` block pointing at `ccimp --offload-mcp --consumer opencode`.
  Unlike the silent Aider tabs, OpenCode is given the TTS-markup convention
  through an instructions file, so the OpenCode tab can speak. cctts does **not**
  bundle OpenCode (~158 MB); install it from <https://opencode.ai/docs> (or drop
  the binary in `ebin/`). A dedicated `opencode_access` per-server flag controls
  which MCP servers OpenCode sees.
- **New `TUI - Grey` theme (OpenCode); `TUI - Red` and `TUI - Aider` removed.**
  The theme set is now `tui-orange` (Claude Code, default) and `tui-grey` — an
  OpenCode-flavored monochrome theme keyed off a cool light-grey accent
  (`#c8ccd0`) and paired with a new `OpenCode Grey` terminal palette. The
  `tui-red` (Imp Red) and `tui-green` (Aider Green) themes and their dedicated
  `Imp Red` / `Aider Green` terminal palettes are dropped. The compiled-in
  last-resort fallback (used only when the on-disk `themes/` folder is empty)
  moves from `tui-red` + `Imp Red` to `tui-orange` + `GitHub Dark`, so the
  embedded fallback now matches the new-install defaults. Settings holding a
  removed theme/palette name keep their string and fall back gracefully at
  load (unknown theme → `tui-orange` chrome; unknown palette → `Default`).

### Migration

- **Schema 18 → 19 (`migrate_v18_to_v19`).** Both reserved `aider` and
  `aider-local` tabs collapse into the single `opencode` tab (id, command, name;
  per-tab `env` preserved; `use_local_provider` reset and stored `--model` args
  dropped; TTS injection enabled; duplicate `opencode` tabs de-duplicated). The
  legacy `aider_local` provider settings are dropped (OpenCode manages its own
  providers). Layout-tree, layout-preset, and active-tab references are rewritten
  and de-duplicated, `enabled_ai_tabs` is remapped, and each MCP server's new
  `opencode_access` defaults to its existing `claude_access`. A `.bak` of the
  v18 file is written before the upgrade.

## [0.21.0] — 2026-06-29

A correctness-and-hardening release: a full-codebase baseline review (security, systemic, and per-area correctness) was triaged and the confirmed issues fixed.

### Security

- **`run_command` short-flag bypass closed.** A glued short flag (`-ccore.hooksPath=/x`) now hits the same denial as the spaced form — previously the glued spelling slipped past the `git -c` guard.
- **Palette path traversal blocked.** The status-line palette name is rejected if it contains a path component, so a crafted `settings.json` can't read outside `palettes/`.
- **`read_file` TOCTOU.** The read is now byte-bounded (not just the pre-stat), so a file growing between the size check and the read can't blow past the limit.

### Changed

- **State signals are must-deliver.** The avatar/permission signal channel is larger and the terminal subprocess-exit edge is delivered with backpressure instead of a silent drop, so a burst can't desync the avatar/permission state machine.
- **Heavy work off the async runtime.** Per-tab content capture runs on a dedicated writer thread, TTS synthesis runs on the blocking pool, and the offload supervisor releases its lock before killing a child — keeping IPC, audio, and amplitude responsive.

### Fixed

- **TTS suppression no longer leaks across tabs.** An Esc-silenced tab stays silent when a different tab produces output; only the active tab's fresh output clears the suppression.
- **Idle-notification edge cases.** The audio idle edge uses `notify_one` (no missed edge), Idle is suppressed while a question is pending (symmetric with permission prompts), and `StopAll` no longer emits a spurious Started→Stopped pair after Esc.
- **Speech-to-text errors surface.** A microphone stream error sets the error state instead of yielding a silent empty transcript, and a mid-hold record-mode flip still releases.
- **Settings reliability.** A rejected settings update rolls back the optimistic UI change, an init-time event race no longer leaves a stale store, a failed overlay removal is no longer reported as success, a backup-write failure no longer wipes settings for the session, and two migration steps were corrected (TTS-injection enable, avatar-margin overlay leak).
- **Offload robustness.** Nested `<think>` blocks strip cleanly, compaction no longer emits consecutive `user` turns, the remote-backend cache key tolerates a trailing slash, the SSE relay reconnects on a half-open socket (and reuses one HTTP client), and the in-flight count stays accurate while the global cap shrinks.
- **Terminal/processing.** The TTS tag scanner advances even with no markers present (fixing an unbounded buffer and O(n²) rescans), cursor-column clamps to a valid column, and multi-parameter cursor-forward sequences emit the right spacing.
- **Frontend.** Fixed Tauri-listener leaks on component churn, a blank-pane layout-repair gap, a `NaN` poll interval that busy-polled, a status-bar drag lockout (pointercancel), a double-click that raced two selection-TTS sessions, a context menu that could open off-screen, and the code-graph monitor's pause-button label.

### Internal

- Graph file-watch and backfill no longer panic or orphan work on thread exhaustion / a lock-gap race; assorted dead code, stale comments, and a swallowed read error were cleaned up.

## [0.20.1] — 2026-06-28

### Added

- **Cancellable offload.** Local offload requests now stream, so interrupting an `offload_task` (or the calling session going away) aborts the in-flight generation and frees the `llama-server` slot immediately, instead of leaving an orphaned request running to completion and blocking the slot.
- **Total token counts in the Offload Server dashboard.** The request history now shows total tokens (prompt + generated) with the generated count broken out (e.g. `41,841 tok · 7,939 out`), and the per-slot bar reflects true context fill (prompt + generated) rather than generated-only.
- **`broot` launches with hidden files shown.** The bottom-bar broot button now runs `broot -g -h`.

### Changed

- **Bottom-bar controls regrouped.** Left-to-right: broot · rustnet | start dictation | play/pause/restart/stop · volume · mute TTS · mute notifications | settings, with dividers between groups. The avatar show/hide toggle was removed from the bar.
- **Leaner no-models update zip.** The slim/no-models zip no longer ships the `avatars/` and `sprites/` folders — the canonical sets are embedded in the app, so it still renders them; the full zip is unchanged.

### Fixed

- **Window resize no longer fires the "idle" chime.** A resize repaints the terminal, which was tripping the avatar's byte-burst activity fallback (Idle → Thinking → Idle) and firing a spurious notification.
- **Offload connection reliability.** The chat client now retries a transient transport (connect/send) error once and no longer reuses idle keep-alive sockets, fixing the `error sending request for url …` failures (including under concurrent requests) caused by the local server closing a pooled connection between requests.
- **No more silent-empty offload results.** When the model ended a turn with no answer (e.g. it reasoned entirely inside a `<think>` block), the agent returned an empty string as success; it now makes one forced-final attempt and surfaces a real answer or an explicit placeholder.

## [0.20.0] — 2026-06-28

### Added

- **Bundled external tools (`ebin/`).** The portable zip now carries a sibling `ebin/` ("external binaries") folder with `broot` (MIT) and `rustnet` (Apache-2.0), so the bottom-bar quick-launch buttons and shell tabs work without the user installing those tools. Command resolution checks `ebin/` first, then PATH; drop any executable into `ebin/` to add a tool. (rustnet needs Npcap installed to capture traffic; aider is not bundled because pip's launcher isn't portable.)
- **External-tool path override (Settings → Bottom bar).** Point `rustnet` or `broot` at a specific executable in any folder (with a file picker), overriding the `ebin/` → PATH lookup. Leave blank to resolve normally.

### Fixed

- **Crash-safe child reaping (Windows).** Offload child processes (`llama-server`, the warm MCP-host servers, `run_command`) are now assigned to a kill-on-job-close Job Object, so the OS terminates them whenever ccImp dies for any reason — a crash, `panic = abort`, `taskkill /F`, or the dev hot-reload — not just on a clean exit. Fixes orphaned `llama-server` processes piling up and holding VRAM across dev cycles.
- **Aider tab gating.** Enabling an Aider tab (cloud or local) is now rejected with a clear message when the `aider` command can't be resolved (not in `ebin`, not on PATH), instead of materializing a dead "command not found" tab. Claude is not gated.

## [0.19.0] — 2026-06-27

### Added

- **Code knowledge graph (V9-01).** A per-project graph of code (files, symbols, references, calls, imports) and docs (Markdown + doc-comments), built in-process with tree-sitter and stored in an embedded CozoDB/SQLite database under `.ccimp/`. Queryable by both the cloud Claude session (MCP tools) and the local offload worker (native tools): `graph_find_symbol`, `graph_callers`, `graph_callees`, `graph_references`, `graph_imports`, `graph_outline`, `graph_transitive`, `graph_search_docs`, `graph_semantic_docs`, and `graph_struct_search` (tree-sitter structural patterns). Covers Rust, TypeScript, JavaScript, Python, and Markdown.
- **Semantic doc search.** Embeds doc chunks via an OpenAI-compatible `/v1/embeddings` endpoint and ranks them with a CozoDB HNSW vector index (epoch-scoped by model + dimension), degrading to full-text search when the embedder is unreachable.
- **Live re-indexing + monitor tab.** A filesystem watcher incrementally re-indexes on change; a reserved, app-rendered **Code Graph** tab shows index/embedder status and counts, an on-demand embedder **Test connection** probe, and a unified **Recent calls** history (cloud + offload). Full Settings panel for languages, ignore globs, size limits, and the embedding endpoint.
- **Warm query path.** Cloud Claude's graph queries now run against the app's single warm index over the loopback (with a direct read-only fallback when the app isn't running), eliminating a cross-process double-open of the SQLite store and feeding the call history.

### Fixed

- Reserved feature tabs (Offload Server, Code Graph) now materialize/disappear immediately when toggled in Settings, instead of only on the next launch.

## [0.18.0] — 2026-06-26

### Added

- **HTTP MCP tool servers for offload.** The offload tool host now speaks the
  MCP 2025-06-18 Streamable HTTP transport, so HTTP/SSE MCP servers (e.g. a LAN
  DuckDuckGo + Context7) work alongside stdio servers and their read-class tools
  are offered to offloaded subtasks.
- **Live MCP server editor (Settings → Offload → Tools).** Add, remove, and
  enable/disable MCP tool servers (name + url) and have the change applied
  without restarting ccImp. A read-only "MCP server status" health section sits
  above the editable list.
- **Offload queue backpressure.** A configurable max queue depth fast-rejects new
  offloads once the pool is saturated and that many tasks are already waiting
  (blank = the old unbounded blocking queue). Live queue depth is shown in the
  warm-pool readout and the per-backend dashboard card.
- **Parallel-offload awareness.** The `offload_task` description now tells Claude
  it can fan out independent subtasks concurrently and reports the live
  concurrent-slot count, so subtasks are issued at once instead of serially.
- **`speak_selection` keyboard shortcut** — read the active terminal's selection
  aloud, the keyboard equivalent of the Ctrl+right-click gesture.

### Fixed

- **Offload queue counter could wedge the queue cap.** The app-wide waiter count
  is now decremented via an RAII guard, so a cancelled/aborted offload no longer
  leaks the counter and permanently triggers the "queue full" fast-reject.
- **An unreachable MCP server could stall parallel offloads.** The HTTP client
  gained a short connect timeout, and an unchanged tool-server config now skips
  the host reconcile entirely, so one dead LAN server no longer serializes every
  in-flight offload behind the reconcile lock or freezes the Settings save.
- **Stale MCP session handling.** The `Mcp-Session-Id` is captured from the
  tools/list response and refreshed on every call, so a server that assigns or
  rotates the id late no longer wedges later calls with a `400`.
- SSE response bodies are read incrementally and return on the first JSON-RPC
  result, so a server that streams progress events first no longer blocks to the
  request timeout. Content-type matching is now case-insensitive.
- A freshly added MCP editor row with no endpoint is no longer connect-attempted
  (no more confusing empty-command error), and text edits persist on blur instead
  of a fire-and-forget write per keystroke that could land a half-typed URL.

### Changed

- **Selection-read diagnostics.** The TTS worker and frontend now log each
  selection chunk's path and warn when a chunk produces no audio or fails, so a
  dropped read can be pinned to a frontend split gap vs a backend skip.
- Drop the `// ` comment prefix on hint text in the tui-green/orange/red themes.

## [0.17.3] — 2026-06-26

### Changed

- **Waveform visualizer no longer burns GPU while idle.** The avatar waveform
  rescheduled its `requestAnimationFrame` render loop unconditionally,
  repainting the canvas (with a `shadowBlur` glow) at display rate even in
  silence. In the WebView2/Chromium compositor that held the GPU at ~10–15%
  while ccImp was otherwise idle — unrelated to the loaded TTS/STT/LLM weights,
  which only compute on an in-flight request. The loop now parks itself once
  the buffer drains to a flat line (~1s after audio stops) and restarts the
  instant TTS or microphone audio resumes.

### Fixed

- **Settings window null-safety.** The offload command-policy lookup
  dereferenced the settings snapshot without the null guard its sibling
  helpers use, risking a crash if invoked before settings finished loading.
  It now returns nothing until settings are loaded.
- Two pre-existing `svelte-check` type errors are resolved (the Custom-palette
  resolver write and the snapshot guard above); `npm run check` is green.

## [0.17.2] — 2026-06-25

### Added

- **Command security policies (Settings → Offload → Tools).** The offload
  `run_command` tool's hardening is now a set of visible, editable per-program
  **security policies** instead of a hidden, git-only special case. Each policy
  names a program and the argument flags / subcommands it refuses plus the
  environment variables it forces at spawn; the previous git hardening ships as
  the seeded default. The Offload settings are reorganized into **Pool** and
  **Tools** sub-tabs, and the allowlist now shows, per program, whether a policy
  is hardening it. Existing config files inherit the default git policy
  automatically.

### Fixed

- **`run_command` argument-policy bypass (security).** A value-consuming global
  flag (e.g. git `--namespace x config …`) could shift the "first non-flag
  token" off the real subcommand, slipping a denied subcommand like `git config`
  past the guard and re-enabling arbitrary code execution via repo-local config.
  The default git policy now denies every value-taking global, restoring the
  guard's soundness; custom policies document the same requirement.
- **Custom terminal palette corrupted the bundled Default.** Resolving a Custom
  theme assigned colors onto the shared `defaultPalette()` object in place,
  corrupting the Default for the rest of the session. It now clones before
  overlaying.
- **Read-only offload tool filter — camelCase gap.** A camelCase mutating tool
  name like `configSet` evaded the write-verb filter that `config_set` would
  hit; all filter tiers now split camelCase sub-words.
- **Loopback auth token** is now compared in constant time (timing side-channel).
- **Offload concurrency-cap reconcile** no longer spawns a task per trigger that
  piles up behind the resize lock during a slow shrink; a single runner absorbs
  concurrent triggers and converges to the latest config.
- **Settings migration** no longer re-migrates (and regenerates a backup) every
  launch if a cascade leaves a file under-migrated; the pre-migration backup
  can't be overwritten on name-probe exhaustion.
- **Offload backend handles** are pruned when a backend is renamed, removed, or
  disabled (was a slow per-edit leak).

## [0.17.1] — 2026-06-25

A sweep of 37 confirmed correctness issues surfaced by a multi-agent bug hunt
and verified before fixing (full write-up in `docs/BUG-HUNT-2026-06-25.md`).
Grouped by area below.

### Fixed

#### Offload

- **Rotated auth tokens take effect immediately.** Editing a remote backend's
  bearer token from one value to another no longer silently reuses the old
  token — the cached handle's reuse check now fingerprints the actual token
  value, not just whether one is present, so a rotated key forces a rebuild
  instead of failing health/props probes with the stale credential.
- **Read-only worker boundary tightened.** The tool-name filter that keeps the
  local offload worker read-only now also catches common mutating verbs that
  previously slipped through (`cancel`, `abort`, `force`, `sync`, `evict`,
  `flush`, `upsert`, `amend`, `persist`).
- **`run_command` git hardening.** `git config` (and `--git-dir` / `--work-tree`)
  are now refused — writing `core.pager` / `core.sshCommand` / aliases let a
  later allowlisted `git` invocation execute arbitrary code. Git is also spawned
  with `GIT_PAGER=cat`, an empty `GIT_SSH_COMMAND`, and ambient config disabled
  as defense in depth.
- **Honest concurrency accounting.** The global concurrency gate is reconciled
  under a lock and the cap is published only after permits are added/reclaimed,
  so the in-flight count no longer transiently mis-reports during a resize. A
  remote `llama-server` restarted with fewer slots (`-np`) is now rebuilt rather
  than over-scheduled against a stale larger gate.
- **Context-budget robustness.** Auto-compaction now hard-truncates an oversized
  retained turn (or a huge original context) as a last resort, so it can't loop
  re-sending an over-budget prompt. A git/diff/commit task now requires the real
  `run_command` tool instead of a dedicated `git` MCP server, so a capable local
  backend isn't wrongly refused. `budget_high_water_pct` is clamped to a non-zero
  floor so a bad value can't zero out the working budget.
- **Clearer native-tool feedback.** `read_file` distinguishes an oversized single
  line from an offset past end-of-file, and flags an ambiguous relative path that
  resolves under multiple roots instead of silently picking the first.
- **Offload can be enabled mid-session.** Turning offload on in Settings after
  launching with it off now starts the loopback discovery endpoint (and warm
  host / health watch / metrics poller) instead of requiring a relaunch.

#### Terminal, tabs & settings

- **No more lost settings on concurrent edits.** Tab create/close/rename, layout
  save, preset edits, and active-tab persistence now compose atomically, so a
  layout auto-save (splitter drag) overlapping a tab operation — or an edit in
  the Settings window — can no longer clobber the other and make a tab vanish or
  a layout reset on next launch.
- **No orphan tabs.** If creating a tab fails partway through, the settings and
  registry entries are now rolled back instead of leaving a phantom tab that
  reappears on next launch.
- **First keystrokes into a new tab are no longer dropped** while the state
  manager is still registering the tab.
- **`pty_start` no longer stalls other terminal commands** by holding the tab
  registry lock across a blocking scrollback file read.
- **The last settings edit before quitting is saved.** Settings are flushed
  synchronously on shutdown, so an edit made within the 500 ms debounce window
  isn't lost; failed atomic writes no longer leave orphaned temp files; and a
  rapid re-migration can no longer overwrite the original pre-migration backup.

#### Audio & TTS

- **Read-along highlight stays in sync.** The selection-read "now playing" and
  "done" edges are retried instead of dropped when the state channel is briefly
  full, so the highlight no longer sticks on the last sentence or skips a chunk.
  A mark/queue desync on a same-tick drain-and-enqueue is also fixed.
- **The avatar no longer lip-syncs to frozen audio while paused.**
- **Cleaner sentence boundaries.** Speak-all no longer splits a sentence at an
  abbreviation (`Dr.`, `e.g.`), and a long legitimate sentence is no longer
  truncated at the head by the runaway-buffer guard.
- **Avatar state recovers correctly** after a subprocess exit or error mid-output
  (no longer stuck in *Thinking*), and a cross-tab idle announcement can no
  longer be silently suppressed by a stale echo-suppression guard.

#### Speech-to-text & theming

- A fully bracketed/parenthesized real utterance (e.g. "(parenthesize this)") is
  no longer discarded as a non-speech marker.
- The mic waveform no longer briefly shows the previous recording's tail.
- Palette verification now accepts only valid CSS hex lengths (3/4/6/8 digits).

#### Frontend

- **Escape only stops TTS when something is playing** (no per-keystroke IPC).
- Custom terminal palettes from `settings.json` are validated key-by-key, so a
  malformed value can't corrupt the xterm theme.
- The usage meter distinguishes "not logged in" from a transport error, so it
  appears within one poll after login instead of backing off up to ~5 minutes.
- A stale sprite-animation frame timer is cleared before loading the next
  animation; the terminal rebind fallback spawns at the live geometry; and
  dictation is appended to a trimmed compose buffer (no double spaces / glued
  lines).

## [0.16.2] — 2026-06-24

### Fixed

- **No more empty console windows on Windows.** Windows allocates a console
  window whenever the GUI process spawns a console executable. The
  `llama-server`, MCP server, and `run_command` spawns now pass
  `CREATE_NO_WINDOW` so they run hidden — their output is already captured over
  piped stdout/stderr and surfaced in the Offload Server tab.

## [0.16.1] — 2026-06-24

### Added

- **Per-backend Offload Server dashboard, grouped Local / Remote.** The Offload
  Server tab now shows one live card per enabled backend instead of only the
  local server, split into **Local** and **Remote** sections. A reachable LAN
  `llama-server` gets the full dashboard — slots busy/total, queue depth,
  throughput, context-fill %, per-slot rows, and request history — just like a
  Local backend; cloud and unreachable backends show a compact status row. The
  raw server log stays available for Local backends (ccImp owns their process).

### Fixed

- **Remote backend slot count.** A remote `llama-server`'s real parallel
  capacity (`-np`) is now discovered from `/props` `total_slots` instead of
  assuming a single slot. The Settings status line and the dashboard show the
  true slot count, and the concurrency gate grows to match so offloads are no
  longer serialized to a multi-slot box.

## [0.16.0] — 2026-06-24

### Added

- **Offload Server dashboard tab + clearer errors (V8-03).** When offload is enabled
  there's now a read-only, non-closable **Offload Server** tab with a live
  **dashboard** of the local `llama-server`: slots busy/total, queue depth,
  throughput (tokens/sec), context-fill %, a per-slot row with live token counts +
  tokens/sec + progress bars, and a request **history** (start/end, duration,
  tokens, avg speed) — plus the raw server log tucked into a collapsible section.
  It polls the server's `/slots` and `/metrics` endpoints rather than scraping log
  text; add `--metrics` to your server command for the true context-fill %, the
  server-side queue depth, and server-computed throughput.
  A Local backend now also requires a genuine llama.cpp server: if something else is
  serving the port (e.g. an LM Studio instance), it shows a clear **error** with
  guidance instead of a false "ready" with an unknown context window. The tab
  appears/disappears on the next launch after toggling **Enable offload**.
- **Warm offload pool + MCP host (V8-03).** The offload machinery moves out of the
  per-call `ccimp --offload-mcp` child and into the long-lived ccImp app, which now
  owns the agent loop, the backend pool, the router, and — finally — the **MCP
  host**: warm, long-lived connections to your configured tool servers
  (`duckduckgo`, `fetch`, `context7`, `git`, `filesystem`) so an offload reaches
  real tools without paying an `npx`/`uvx` cold-start per call. Tools are
  namespaced (`ddg__search`), filtered to **read-class only** (write/destructive
  tools are dropped), and `filesystem` is confined to the allowed roots; per-server
  health shows in **Settings → Offload**. The app enforces a single **global
  concurrency gate** across every Claude tab, so the capability description and the
  router's spill/fail-over now run on **honest, health-accurate** state. The child
  shrinks to a thin proxy: when the app is up it forwards over an authenticated
  loopback endpoint (ephemeral port + per-launch token, advertised in a discovery
  file next to the exe — never `~/.claude`) and relays `tools/list_changed` when a
  backend or tool server goes up/down; when the app is **down** (headless cron,
  mid-restart) it falls back to the self-contained path, so offload still works
  without the app.
- **Offload backend pool + capability-aware routing (V8-02).** The single local
  offload server generalizes into a **pool of backends** — a local `llama-server`,
  a LAN machine, and/or a cloud OpenAI-compatible endpoint — and ccImp routes each
  `offload_task` to the right one. The router picks a backend by required tools,
  required context, tier (`fast`/`quality`), and live availability (spilling when
  one is busy, failing over when one is down); Claude can bias it with a new
  `tier` argument, and the tool description reports the whole pool so Opus knows a
  fast tier exists. **Remote backends** are configured by URL (+ optional auth)
  and health-checked — no local process, no tab. **Per-backend tool scoping** is
  the privacy boundary: a **cloud** backend defaults to web/docs only (local
  file/search/command/git tools denied at both the routing and tool-array layers)
  and is unusable until you grant explicit data-egress consent; LAN backends keep
  data on your network. Manage it all in **Settings → Offload → Backend pool**.
  Additive settings migration: an existing single `server_command` becomes one
  Local backend in the pool.
- **Local task offload (V8-01).** ccImp can now hand token-heavy subtasks —
  broad codebase searches, large-file/log summarization, web research — from the
  main Claude (Opus) session to a local LLM, so the cloud session's context grows
  by a paragraph instead of a megabyte. You point ccImp at a `llama-server`
  command (e.g. Qwen3.6-35B-A3B) in **Settings → Offload**; it injects an
  `offload_task` MCP tool into the Claude tabs it launches (session-scoped via
  `--mcp-config`, never touching `~/.claude`), and the local model does the
  searching/reading/summarizing while only the synthesized result returns to
  Opus. The agent loop, the MCP server toward Claude (the hidden
  `ccimp --offload-mcp` subcommand), and the native tools (`read_file`,
  `code_search`, allowlisted `run_command`) all live in the single ccImp binary —
  no Node/Python sidecar. ccImp discovers the server's context window and slot
  count, budgets each task against the per-slot window, and bounds it by step
  count and a wall-clock timeout. Off by default; the model is user-supplied
  (not bundled). File access is confined to configurable `allowed_roots` and
  `run_command` is deny-by-default (allowlist only).

### Fixed

- **Offload per-slot budget (V8-03).** llama.cpp's `/props` reports the *per-slot*
  context window (the total `--ctx-size` already divided by `-np`), but ccImp
  divided by the slot count a second time — so every offload got roughly half its
  real working window. Fixed; offloads now budget against the full per-slot window.
- **Cross-backend offload spill now works (V8-03).** Because each per-call
  `--offload-mcp` child was blind to every other in-flight offload, it always
  reported `in_flight == 0`, so V8-02's spill-on-busy and fail-over never fired in
  production — the router saw every backend as free. The long-lived app-side
  service sees all in-flight offloads across every Claude tab and feeds the router
  honest counts, so a busy backend now spills to a free one (and a full pool queues
  coherently behind the global gate) as designed.
- **Claude Code fullscreen renderer disabled.** Recent Claude Code versions default
  to an alternate-screen "fullscreen" TUI that repaints the whole screen and enables
  mouse tracking — both break ccImp's core assumption of a linear, append-only output
  stream. The result was leaked literal `[[TTS]]` markers (visible on select), double
  paste and double copy-on-select, and a dead Ctrl+right-click speak-selection. ccImp
  now forces the classic inline renderer by setting
  `CLAUDE_CODE_DISABLE_ALTERNATE_SCREEN=1` for every Claude tab (overridable via per-tab
  env), restoring all four behaviors.

## [0.15.0] — 2026-06-20

### Added

- **Context-window bar in Claude's status line.** ccImp now injects a status
  line into the Claude Code tabs it launches, showing live context usage —
  e.g. `Opus  ▓▓▓▓▓░░░░░ 50% (100k/200k)` — themed to your terminal palette.
  It renders from the new hidden `ccimp --statusline` subcommand (no external
  script, no Node/Python/PowerShell dependency) and is wired up via a
  session-scoped `--settings` overlay, so it appears only inside ccImp and
  never touches your global `~/.claude` configuration. Enabled by default;
  toggle it in Settings → Bottom bar → Claude context bar.

## [0.14.0] — 2026-06-18

### Added

- **Tool launch buttons in the bottom bar.** Two quick-launch buttons —
  `rustnet` (🌐) and `broot` (🌳) — each open a fresh tab running that tool
  (`rustnet`, `broot -g`). Every press opens another tab, and each is an
  ordinary closable Shell tab with a close `×`, so these situational tools can
  be spun up and torn down freely. The command is resolved via `PATH` at spawn
  time; a missing tool still opens the tab and shows the standard
  "command not found" overlay. Backed by the new `open_tool_tab` IPC.

### Changed

- **Default UI theme is now `tui-orange`** (was `tui-red`) so the chrome accent
  matches Claude Code's orange; the paired default terminal palette is
  `GitHub Dark`. The avatar still defaults to the `impSprites` imp. Existing
  settings keep their persisted theme.
- **Speech-to-text is enabled by default.** The bottom-bar record button now
  shows out of the box (a Whisper `ggml-*.bin` model under `models/` is still
  required to transcribe).

### Removed

- **Persistent `broot` builtin tab.** broot is no longer auto-started on fresh
  installs, and the Settings → Tabs "Utility tabs" toggle (and its
  `set_broot_enabled` IPC) is gone — broot now launches on demand from the
  bottom-bar button like rustnet. The settings schema bumps to v16; the
  v15 → v16 migration drops the old auto-seeded `shell-broot` tab from existing
  files (any closable broot tabs you open from the button are unaffected).

## [0.13.1] — 2026-06-17

### Fixed

- **Imp avatar rendered as garbage.** The `impSprites` manifest declared
  `tile: 20` while its frames are 320×320. The sprite player computes its
  alpha-bbox crop in tile space and reuses those coordinates as the source
  rectangle on the full-resolution frame, so it sampled a ~20px patch from
  each frame's top-left corner and blew it up — every imp animation broke. The
  `work_think1` (Speaking) frames were also 1024×1024, inconsistent with the
  rest. Fixed by downscaling `work_think1` to 320×320 so the set is uniform and
  setting the manifest `tile` to 320 to match the real frame size.

### Changed

- **`tui-red` and `tui-orange` now use the `GitHub Dark` terminal palette**
  (was `Imp Red` and `Tomorrow Night`). Existing settings.json files keep
  whatever palette they were persisted with.

## [0.13.0] — 2026-06-17

### Added

- **ccImp imp mascot is now the default avatar.** The `impSprites` set ships
  its first art pass — six pixel-art animations (idle blink, dance bounce, two
  think loops, a burning-tokens loop, and a surprise expression) that cover all
  five avatar states via the manifest's `groups`. New installs default to the
  imp; Clawd (`claudeSprites`) stays selectable in Settings → Avatar.
- **`tui-red` theme — the new ccImp default.** A ratatui-style theme keyed off
  the imp's scarlet accent (`#e23c3c`), paired with a new `Imp Red` terminal
  palette. New installs land here.
- **`tui-green` theme ("TUI - Aider").** A selectable theme keyed off Aider's
  terminal green (`#2eb82e`, brightened from Aider's `#14b014` logo green),
  paired with a new `Aider Green` terminal palette — one TUI theme per tool the
  app fronts.

### Changed

- **Default UI theme is now `tui-red`** (was `tui-orange`) and the default
  terminal palette is `Imp Red` (was `Tomorrow Night`). Both prior defaults
  remain selectable. Existing settings.json files keep whatever theme/palette
  they were persisted with.

### Removed

- **Dropped the `modern-dark`, `tui-yellow`, and `tui-purple` themes.** The
  theme set is now `tui-red` (imp / default), `tui-orange` (Claude Code), and
  `tui-green` (Aider). The pre-V1.13 legacy `"tui"` theme value now migrates to
  `tui-orange` (the surviving Gruvbox theme) instead of the removed
  `tui-yellow`. With no remaining native-chrome theme, the custom TUI title bar
  always mounts; the OS-chrome path stays as forward-compat for any future
  `decorations: true` theme.

## [0.12.0] — 2026-06-16

### Added

- **Per-tab "TTS all output" mode.** Right-click a Claude tab → **TTS all
  output** to make that tab speak **all** new terminal output and ignore the
  `[[TTS]]…[[/TTS]]` markers, instead of speaking only the marked segments. A
  speaker icon on the tab shows which tabs have it on. Output is
  ANSI-stripped, sentence-segmented, and deduped; an in-progress trailing
  sentence is held until it completes, and unterminated TUI chrome (spinner /
  status line / box-drawing / input prompt) is dropped rather than spoken.
  `Esc` still stops playback until the next output burst. The toggle is
  per-tab, applies live (no restart), skips the existing backlog, and
  persists in the per-folder overlay (`.ccimp.custom.config.json`). Cleanest
  for plain/line-oriented output (e.g. a local-LLM tab with no markers). The
  user's own typed/submitted input is registered and skipped when the TUI
  echoes it (even behind the `> ` prompt prefix), so the question isn't read
  back. Note: it speaks everything Claude prints, including reasoning/thinking
  shown above the answer — for answer-only speech, use the default `[[TTS]]`
  marker mode instead.
- **Manifest-driven animated sprite avatars.** The per-state animation
  mapping moved out of hardcoded app code into each sprite set's
  `manifest.json` `groups` (state → animation rotation list), so a new sprite
  set fully defines its own behaviour without code changes. Ships an
  `impSprites` set scaffolding the project's own imp mascot alongside the
  bundled `claudeSprites`.

### Changed

- **Renamed `cctts` → `ccImp`.** The app, binary (`ccimp.exe`), crate, npm
  package, window titles, log prefix (`ccimp.log`), per-folder overlay
  (`.ccimp.custom.config.json`), and GPU env var (`CCTTS_GPU` → `CCIMP_GPU`)
  all move to the new name — renaming the project after its mascot rather
  than a single feature. Still fully portable (writes only next to the exe).
  Re-set any `.cctts.*` overlay or `CCTTS_GPU` usage under the new names.
- **Stronger TTS injection prompt for full-answer coverage.** The default
  runtime prompt now makes "wrap your whole answer, not a summary" the
  headline rule, so Claude marks its entire prose answer for speech instead
  of just a sentence or two. Existing tabs adopt it via *Settings → Tabs →
  TTS markup injection → Reset*, then *Restart Tab*; fresh installs get it by
  default.

### Fixed

- **No spurious "idle" announcement on startup.** A freshly-spawned Claude
  tab no longer speaks its idle notification when the welcome banner settles;
  the idle announcement now fires only after the tab has had real user
  interaction.

## [0.11.0] — 2026-06-15

### Added

- **GPU-accelerated text-to-speech via WebGPU.** Kokoro TTS now runs on ONNX
  Runtime's WebGPU execution provider (Dawn-backed), making GPU TTS **portable
  and vendor-agnostic** — it uses any GPU (NVIDIA/AMD/Intel) and falls back to
  CPU automatically, exactly like the Vulkan STT path. Measured ~5× faster than
  CPU, and it works on GPUs where the old CUDA path couldn't (including Blackwell
  / RTX 50-series). Nothing CUDA-specific is bundled — just three small Dawn
  dylibs. (`CCTTS_GPU=cpu` forces CPU.) Source builds default to CPU; build
  `--features tts-webgpu` for the GPU variant. See `docs/features/FEATURE-tts-webgpu.md`.

### Changed

- **TTS GPU is now a compile-time feature, not the `CCTTS_GPU=cuda` runtime
  opt-in.** The released binary ships `tts-webgpu`; the old NVIDIA-only CUDA path
  survives only as the optional, non-default `tts-cuda` build (mutually exclusive
  with `tts-webgpu`, and not shipped). `CCTTS_GPU=cpu` forces CPU for both TTS
  and STT.

## [0.10.0] — 2026-06-15

### Added

- **Offline speech-to-text (dictation).** Press the new microphone button in
  the bottom bar, or hold the push-to-talk shortcut (default `Ctrl+Shift`), to
  dictate by voice. A fully offline, bundled Whisper model (whisper.cpp)
  transcribes your speech into the compose overlay for review before you send
  it — no cloud, no API key, nothing leaves your machine. Enable it under
  Settings → Speech-to-text, where you can pick the model, input device,
  language, translate-to-English, and the record-button mode (toggle vs hold).
  Drop additional `ggml-*.bin` models into the `models/` folder to switch
  between them. The released portable binary is **GPU-accelerated via Vulkan**:
  it automatically uses any GPU (NVIDIA/AMD/Intel) and falls back to CPU when
  none is present — no install, the only requirement is Windows' built-in
  `vulkan-1.dll`. (`CCTTS_GPU=cpu` forces CPU.) Source builds default to CPU;
  build `--features stt-vulkan` for the GPU variant. See `docs/MAINTENANCE.md`.

### Changed

- **Three default shortcuts moved off `Ctrl+Shift`** so they don't collide
  with the new push-to-talk chord: Open compose `Ctrl+Shift+E` → `Alt+Enter`,
  Split pane (vertical) `Ctrl+Shift+\` → `Alt+\`, Close pane `Ctrl+Shift+W` →
  `Ctrl+Alt+W`. These are new-install defaults only — existing settings keep
  your current bindings; re-bind them under Settings → Shortcuts if you want
  the new defaults.
- **Compose overlay: `Enter` now sends**, and `Alt+Enter` (or `Shift+Enter`)
  inserts a newline — a one-handed flow that pairs well with dictation. The
  default `submit_compose` shortcut changed `Ctrl+Enter` → `Enter`; the compose
  box handles these keys directly, so the behavior applies without re-binding.
  Also fixed a flicker where the terminal area briefly shifted down when the
  compose sheet opened.

## [0.9.2] — 2026-06-12

### Changed

- **Internal cleanup — no user-facing behavior change.** Removed dead code
  across the Rust backend (unused functions, methods, enum variants, and the
  unused cell-attribute/row-timestamp bookkeeping in the terminal screen
  model) now that all milestones are complete, and cleared the remaining
  `#[allow(dead_code)]` suppressions. Applied mechanical clippy cleanups and
  de-duplicated a few frontend helpers (`AiTabId`, terminal-palette
  application, error-to-string formatting). Terminal colors are unaffected —
  they have always been rendered by xterm.js from the raw byte stream.

## [0.9.1] — 2026-06-12

### Fixed

- **No more blank window on startup.** The main window is now created hidden
  and revealed only once the UI has mounted and the window chrome has settled,
  so the empty WebView that used to flash for a couple of seconds on launch —
  along with the brief title-bar jump as the TUI themes drop the OS
  decorations — is no longer visible. A short safety-net timeout reveals the
  window regardless if the chrome setup stalls.

## [0.9.0] — 2026-06-10

### Added

- **Default `broot` tab.** Fresh installs now ship a `broot` tab alongside
  the default shell. It launches `broot -g` (the broot file browser with
  git info shown in the tree) with no `cwd`, so it opens in the directory
  cctts was started in. `broot` is resolved via `PATH` at spawn time; if it
  isn't installed the tab shows the standard "command not found" overlay
  until you install it. Existing installs get the tab injected by the
  v14 → v15 settings migration (schema bumped to 15); the frontend's layout
  repair places it in the focused pane on first launch after upgrade.
- **broot enable/disable in Settings → Tabs.** A new *Utility tabs* group
  exposes a `broot (git)` checkbox. While enabled the broot tab is a
  builtin — it has no close `×` and can't be closed from the tab bar or
  context menu; untick the checkbox to remove it (kills its PTY and drops
  its scrollback). Re-ticking re-creates it. Backed by the new
  `set_broot_enabled` IPC, mirroring how the AI tabs are gated by
  `enabled_ai_tabs`.

## [1.3.3] — 2026-05-07

### Added

- **Second Claude Code tab for a local LLM.** A new `claude-local` builtin
  AI tab runs the same `claude` binary as the subscription Claude tab but
  with `ANTHROPIC_BASE_URL` and `ANTHROPIC_AUTH_TOKEN` injected at spawn
  time (and optionally `ANTHROPIC_MODEL`), pointing at a local proxy that
  translates the Anthropic Messages API to your local model. Replaces the
  pre-V1.4-07 Aider tab in the same id slot.
- **Local LLM provider settings group.** New `claude_local: { base_url,
  auth_token, model_alias }` settings group exposed in *Settings → Local
  LLM provider*. The auth-token field is password-masked with a show /
  hide toggle. Helper text links to the LiteLLM docs and notes that cctts
  does not start the proxy itself.
- **Per-AI-tab `Use local LLM provider` toggle.** A checkbox on each AI
  tab in *Settings → Tabs* gates env synthesis from the global
  `claude_local` group. Off by default for the subscription Claude tab;
  on by default for the new Claude (local) tab. When enabled, the
  effective env is shown inline as helper text. Per-tab `env` entries
  always override synthesized values, so power users can still target a
  different provider per tab.
- **AI-tab Configure routes to Settings → Tabs scoped to that tab.** The
  right-click *Configure tab* entry on AI tabs (which previously only
  worked on Shell tabs because the dialog is shell-only) now opens the
  Settings window scrolled and expanded to the matching tab section. New
  IPC `open_settings_window_to_tab(tab_id)` plus a `settings-deep-link`
  event the Settings frontend listens for. Cold-open path uses a backend
  state cell consumed via `consume_settings_deep_link`; hot-open path
  uses the event. Shell tabs continue to use `ConfigureTabDialog.svelte`
  unchanged.

### Changed

- **Per-tab theme and background overrides now reach the right-click
  Configure flow on AI tabs.** Schema and the Settings → Tabs UI already
  exposed `theme_override` / `background_override` on AI tabs (V1.4-01 /
  V1.4-02 / V1.4-04); the right-click Configure entry now also routes to
  that surface so the more discoverable path works for the Claude tabs
  too. The runtime application path (`terminals.ts`'s `effectiveTheme`
  / `effectiveBackgroundMode`) was already kind-agnostic — the gap was
  purely UI routing.
- **`AiToolKindWire` enum collapsed.** Pre-V1.4-07 the schema carried
  `ai_tool_kind: "claude_code" | "aider"` on every AI tab; V1.4-07 drops
  the discriminator entirely. AI tabs are simply Claude Code, with an
  optional `use_local_provider` flag that gates env synthesis. The
  state-side `AiToolKind` enum collapses to the same shape (`TabKind::AiTool`
  with no inner data).
- **Default install layout.** Fresh installs now ship with two AI tabs
  (`claude` + `claude-local`) plus the default Shell tab. The integrity
  check restores any of the three reserved ids if a hand-edit removes
  them. The check also coerces `use_local_provider` to its canonical
  value on each builtin so a hand-edit can't silently flip the
  subscription Claude tab into local-LLM mode.
- **`Ctrl+2` switch label** in *Settings → Shortcuts* now reads "Switch to
  Claude (local) tab" (was "Switch to Aider tab").

### Removed

- **Aider tab kind.** `AiToolKindWire::Aider`, `AIDER_TAB_ID`,
  `default_aider_tab()`, the `AiderFirstLaunchNotice.svelte` overlay,
  the aider-specific TTS-injection no-op warning, and the aider-specific
  install-hint in the tab-error overlay are all gone. Aider permission-
  detection patterns (always empty in practice) are also removed.
- **`docs/features/FEATURE-aider-parity.md`** deleted; the two
  aider-related entries in `docs/FUTURE-FEATURES.md` (TTS injection
  blocked on upstream support, permission-pattern enumeration) moved to
  the historical section as superseded by the Aider removal.

### Migrated

- **v1.7 → v1.8** — adds the global `claude_local` group; drops the
  `ai_tool_kind` field from every AI tab; adds `use_local_provider:
  false` to every AI tab; rewrites the legacy aider tab in place
  (`id` → `claude-local`, `name` → `Claude (local)`, `command` →
  `claude`, `args` → `[]`, `use_local_provider: true`, `tts_injection`
  re-enabled with the runtime prompt as the default instructions, and
  the canonical "Aider …" notification strings rewritten to "Claude
  (local) …" — user customizations to env, theme/background overrides,
  and notification text are preserved). Layout-tree references to the
  legacy `"aider"` id are recursively rewritten to `"claude-local"` in
  `layout.tree`, every `layout_presets[].tree`, and
  `session.active_tab_id`. Backup at `config.json.v1.7.bak.<ts>`.
- A v1.2 file lands at v1.8 in one launch with six backups (v1.2, v1.3,
  v1.4, v1.5, v1.6, v1.7).

### Notes

- The auth token is stored cleartext in `settings.json`. Local proxies
  typically accept dummy tokens, so this is acceptable; OS keychain
  integration is a future enhancement if real Anthropic API keys end up
  in the field.
- TTS markup compliance on the Claude (local) tab depends on the
  underlying model. Smaller local models often don't honor the
  `[[TTS]]…[[/TTS]]` convention reliably; cctts treats missing markup
  as silent (the existing fallback behavior).
- Tool-use (Edit / Write / Bash / etc.) on the Claude (local) tab
  depends on the local model supporting Anthropic-style tool calling —
  test before committing to a particular model.

## [1.3.2] — 2026-05-07

### Added

- **Modern Dark theme.** Refreshed visual language: cool slate-blue surfaces,
  mint/teal accent (`#3eddb6`), coral semantics (`#f06080`), generous rounded
  corners (10/14/pill scale), and soft elevation shadows on dialogs / popovers
  / sheets.
- **Centralized design tokens.** `src/theme.css` defines the full token
  surface (surfaces, text, accent, semantics, borders, radii, shadows,
  spacing, motion, typography). Components reference `var(--*)` everywhere;
  no more component-local hex literals.
- **Settings → Appearance → UI theme.** A theme picker for the cctts chrome,
  distinct from the per-tab terminal palette under Display. Initial release
  ships only "Modern Dark"; the entry exists so future themes (light,
  high-contrast) plug in without UI plumbing churn. Persisted as
  `settings.ui.theme`.
- **Pill-shaped active tabs.** Active tab now reads as an elevated rounded
  pill (`--surface-3` fill on `--surface-2` bar) instead of a flush rectangle
  with a bottom-border accent. The two-tier active-state pattern reserves
  mint accent fill for filter toggles and primary CTAs; section selection
  uses surface elevation.
- **`<Pill>` primitive** (`src/lib/Pill.svelte`) — reusable tag/badge with
  `default | mint | coral | orange | accent-fill` variants and three sizes.
  First use site: the "restart required" indicator in Settings.
- **`prefers-reduced-motion` support.** Hover / focus transitions become
  instant when the OS-level reduce-motion preference is enabled.
- **Tabular numerics** on settings value labels (Speed, Volume, Opacity,
  Glow, Line width) so the label width doesn't jitter as the slider moves.
- **Terminal color themes (V1.4-01).** ~12 bundled xterm.js palettes (Default,
  Dracula, Solarized Dark/Light, Nord, Tomorrow Night, Gruvbox Dark/Light,
  One Dark, Monokai, Tokyo Night, GitHub Dark) plus a 22-color custom editor.
  Selectable globally in *Settings → Appearance → Terminal palette*, with
  per-tab override in *Configure Tab → Appearance*. Override travels with the
  tab through drag-and-drop. Live theme swap via `term.options.theme = ...` —
  no terminal recreation, no scrollback loss.
- **Terminal background image / solid color (V1.4-02 / V1.4-03).** Image or
  solid-color background beneath terminal text, with opacity, blur, size, and
  position controls. Per-tab override (custom config / "use global" /
  "disabled") in the Configure Tab dialog. Backgrounds force the xterm.js DOM
  renderer; only tabs that opt in pay the perf cost. Scrollback survives
  renderer flips: the outgoing xterm's state is captured via
  `serializeAddon.serialize()` and replayed into the new instance with
  `term.write()`.
- **Terminal background presets (V1.4-04 B).** Save the current background
  configuration as a named preset from *Settings → Appearance*; load presets
  from either the global page or the per-tab Custom branch. Manage / rename /
  delete from the Manage presets dialog.
- **Live preview in Configure Tab (V1.4-04 C).** Background changes in the
  Configure Tab dialog apply to the target terminal in real time while the
  dialog is open; closing without Save reverts to the original. Optional
  `terminal.background.preview_category_flips` toggle defers image-path swaps
  and category flips until Save for users with many tabs.
- **Cross-restart scrollback (V1.4-04 D).** Per-tab PTY ring buffer (256 KB
  default) persists to `<config-dir>/scrollback/<tab-id>.bin` on graceful
  exit, replayed via `term.write()` on next launch. Settings group
  `terminal.scrollback` (`ring_bytes`, `persist`, `restore_on_launch`).
  Best-effort recovery — hard kills (SIGKILL / Task Manager) lose the buffer.

### Changed

- **`settings.json` shape (UI chrome).** New top-level `ui: { theme: string }`
  block, defaulted to `"modern-dark"`. Existing v1.3 files load unchanged via
  serde defaults; the field is added on next save. No explicit migration
  required.
- **DropZoneOverlay** — switched from a flat blue fill to a mint dashed
  border with a soft inner glow, more visible against dark terminal panes.
- **Dialog elevation** — dialogs now use `--shadow-lg` and `--radius-lg`
  (14 px corners). Inputs sit on `--surface-sunken` with a mint accent
  border on focus.
- **Status bar toggles** (mute, announcements) — pill-shaped with
  `accent-muted` bg + accent border + accent text when active, indicating
  "filter engaged."
- **Snapshot cap and alt-screen detection (V1.4-04 A).** Renderer-flip
  scrollback capture is bounded by `terminal.background.snapshot_lines`
  (default 2000). When the alt-screen buffer is active (`vim`, `less`,
  `htop`, …) snapshot capture and replay are skipped — the live shell
  survives the rebind, but alt-screen contents are dropped (press Ctrl+L
  in the TUI to redraw).
- **Recreate-debounce stagger (V1.4-04 A).** Mass-recreate (e.g., a global
  category flip with many tabs) staggers across two animation frames at
  60 Hz instead of firing all timers in the same frame.

### Migrated

- **v1.3 → v1.4** — adds `terminal.theme = { name: "Default", custom: null }`,
  stamps `theme_override: null` on every existing tab, removes the dead
  `display.theme` field. Backup at `config.json.v1.3.bak.<ts>`.
- **v1.4 → v1.5** — adds `terminal.background` group (`image`, `color`,
  `opacity`, `blur`, `size`, `position`) and stamps `background_override:
  null` on every existing tab. Backup at `config.json.v1.4.bak.<ts>`.
- **v1.5 → v1.6** — adds `terminal.background.presets: []`. Backup at
  `config.json.v1.5.bak.<ts>`.
- **v1.6 → v1.7** — adds `terminal.scrollback` group and
  `terminal.background.preview_category_flips: true`. Backup at
  `config.json.v1.6.bak.<ts>`.
- A v1.3.0 file lands at v1.7 in one launch with four backups.
- The `ui` block continues to load via serde defaults — no explicit migration
  step is needed for the chrome theme.

### Removed

- **Per-tab avatar configuration** and **Per-tab TTS settings** — both were
  planned as items 3 and 4 of `docs/features/FEATURE-per-tab-overrides.md`
  and slated for V1.4-05 / V1.4-06. Cancelled as a scope decision: cctts
  ships exactly one avatar and one TTS voice, customized globally only.
  The skeleton plans were removed; the feature doc and `FUTURE-FEATURES.md`
  were updated to reflect the decision. No code or schema changes (the
  override fields were never added).

## [1.3.0] — 2026-05-06

### Added

- **Multi-pane layout.** The terminal area is now a recursive tree of panes
  and splits. Drag a tab to a pane edge to tear it into a new split, or to a
  pane center / tab bar to move it. Drag-and-drop uses a custom pointer-event
  handler with a 4 px threshold so clicks still register as clicks.
- **Splitter resize.** Each split has a 4 px draggable line between its two
  children (`col-resize` / `row-resize` cursor). Min-pane sizes (200 px wide,
  100 px tall) clamp during drag; window resize re-clamps visually without
  overwriting the user's stored ratio.
- **Pane-aware keyboard shortcuts.**
  - `Ctrl+\` — split focused pane horizontally with a fresh Shell tab.
  - `Ctrl+Shift+\` — split vertically with a fresh Shell tab.
  - `Ctrl+Alt+Arrow` — move focus to the geometrically-adjacent pane.
  - `Ctrl+Shift+W` — close focused pane (tabs migrate to the surviving
    sibling, then the empty pane collapses).
- **Pane right-click context menu** with Split horizontally / vertically,
  Close pane, and Move all tabs to → submenu.
- **Layout persistence.** The full layout tree and focused pane id persist to
  `settings.json` on a 250 ms debounce. Re-launching restores the exact pane
  arrangement from the previous session.
- **Named layout presets.** Save the current layout under a name from the
  Layouts popover in the bottom status bar; restore via Recent presets or the
  Manage presets dialog (with inline rename and confirm-delete).
- **Per-pane tab bar overflow.** When more tabs fit in a pane's width than
  display, the tab bar scrolls horizontally with thin scrollbars and edge-fade
  gradients. The `+` button stays pinned at the right. Activating an
  off-screen tab (via `Ctrl+N` or click) scrolls it into view.
- **Accessibility:** `role="group"` + dynamic `aria-label` on each pane
  (announces ordinal, total panes, and active tab name). `role="separator"` +
  `aria-orientation` + `aria-label="Resize panes"` on splitters. `:focus-visible`
  outlines on tabs, panes, splitters, and the new-tab button. `aria-hidden`
  on the drag ghost so screen readers don't follow it.

### Changed

- **`Ctrl+1`..`Ctrl+9` are now pane-scoped.** They switch to the Nth tab in
  the **focused pane**, not the Nth tab in the global list. This is the only
  behavior change for v1.2 users — closing or moving a tab shifts higher-
  numbered ones down by one within their pane, just as before, but the
  numbering is per-pane.
- **`Ctrl+T` and `Ctrl+W` are now pane-scoped** (new tab into focused pane,
  close active tab in focused pane).
- **Focused-pane indicator** is a 2 px top accent on the focused pane's tab
  bar (placed at the top so it doesn't merge with the active-tab underline,
  which uses the same accent color at the bottom).
- **Avatar overlay, audio playback, and the compose overlay** now route to
  the **focused pane's active tab** rather than a single global active tab.
  Switching pane focus retargets all three.

### Migrated

- v1.2 → v1.3: settings files without a `layout` key are migrated by
  synthesizing a single root pane containing every tab in order, picking
  active from `session.active_tab_id` (then dropped). A
  `settings.json.v1.2.bak` backup is written alongside before the rewrite.

### Known issues

- `Ctrl+Shift+W` may collide with WebView2's "close window" on some Windows
  configurations. If the close shortcut steals the keypress, remap
  `close_pane` to `Ctrl+Q` or `Ctrl+Alt+W` in *Settings → Shortcuts*.
- `Ctrl+Alt+Arrow` may collide with GNOME / KDE workspace switching on
  Linux. Remap `focus_pane_*` to `Ctrl+Shift+Arrow` if so.
- Tearing a tab into its own top-level window is not implemented — tabs
  always live within the single application window.
- No keyboard equivalent for moving a tab between existing panes; use drag
  or the Move all tabs to → context-menu submenu.
