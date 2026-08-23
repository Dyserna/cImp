# Design Document: cImp

## Purpose of This Document

This document captures the current architecture and design decisions of cImp. It is the authoritative reference for implementation. When a milestone specification is unclear or a design choice is not covered, this document is the fallback.

Where granular history matters (which decision changed when, what a particular milestone introduced), see the per-milestone `MILESTONE-V[N]-[N].md` files. Where deferred work is tracked, see `FUTURE-FEATURES.md`. This doc describes how cImp is, not how it evolved.

The audience is an AI coding agent working on implementation across sessions, plus any human reviewer.

---

## Project Overview

### What we are building

A cross-platform desktop application (Windows and Linux) that hosts one or more **registered AI coding harnesses** — command-line agents cImp neither owns nor ships — alongside arbitrary user-configured shell sessions, in a single multi-tab, multi-pane interface with text-to-speech output and an animated avatar overlay. The user retains the full interactive experience of every embedded subprocess — this is not a chat client that calls a model API; it is a wrapper around the harnesses' real binaries (and any shell the user configures) running in real PTYs, with all of their tools, slash commands, file editing, and TUI behavior preserved.

Which harnesses exist is **data, not code paths**. `src-tauri/src/harness/registry.rs` holds one `HarnessDescriptor` per harness — its id, label, the binaries whose file stem identifies it, its reserved built-in tab ids, its MCP consumer token, and the features core mounts extra UI for — and `src-tauri/src/harness/plugin.rs` declares the `HarnessPlugin` trait that carries the code half. Two ship registered today:

- **Claude Code** (`claude`), in two configurations — subscription / API, and a local-LLM variant that is the same binary with provider env injected at spawn.
- **OpenCode** (`opencode`), which picks its own provider and model, so it has no local variant.

A third harness is a registry row plus one `harness/<id>/` directory; core never branches on *which* harness it is looking at, it asks the registry. See `../src-tauri/src/harness/README.md` for the layering, and the per-harness READMEs (`../src-tauri/src/harness/claude/README.md`, `../src-tauri/src/harness/opencode/README.md`) for what each one's plugin actually depends on.

Capabilities beyond the underlying tools:

1. **Text-to-speech** for conversational portions of AI-tool output, using a local Kokoro TTS engine running in-process. Tools opt their conversational text into TTS by wrapping it in `[[TTS]]...[[/TTS]]` tags; technical content (code, command output) stays silent.
2. **Animated avatar overlay** with state-driven visuals, a shared transition animation between any state change, and a live audio waveform reactive to TTS playback. Floats over the terminal as a configurable, semi-transparent overlay.
3. **Spell-checking compose overlay** — a slide-up bottom sheet for composing longer messages, complementing the native input.
4. **Multi-tab terminal** with one reserved built-in tab per registered harness configuration (today `claude`, `claude-local` — the same binary with local-provider env injected at spawn — and `opencode`) and as many user-defined Shell tabs as the user wants. Each tab runs an independent PTY; tabs persist across launches.
5. **Multi-pane layout** — tabs can be split horizontally or vertically, dragged between panes, torn into new splits. The full layout tree persists across launches, and named layout presets can be saved and restored.
6. **Permission-prompt detection** for AI tabs, with a per-tab `AwaitingPermission` flag and notification. The matcher is harness-neutral; the prompt grammar it matches on is declared by each harness's plugin.
7. **Notification system** that announces tab state changes when the user is focused elsewhere.
8. **Bottom status bar** with mute / announcements / volume controls and a Layouts menu.
9. **Offline speech-to-text (dictation)** — the inverse of the TTS path: a `cpal` microphone capture feeds a bundled, fully offline Whisper model (whisper.cpp via `whisper-rs`) and the transcript lands in the compose overlay for review. Triggered by a bottom-bar record button (toggle or hold) or a push-to-talk shortcut. No cloud, no API key.

### What we are NOT building

- A standalone chat application calling a model API directly
- A replacement for any harness's own TUI
- A coding assistant with custom tool integration
- A multi-user or remote-accessible service
- A general-purpose terminal emulator (the architecture supports it, but the project stays focused on the AI-tools + shells use case)

### Primary user

A single technical user running on their main desktop (Windows, RTX 5090) with optional Linux support. CS background, prefers terse interactions. The TTS layer is for ergonomic enhancement, not accessibility-required functionality.

---

## Stack

### Backend: Rust

- **Tauri 2.x** — application shell and IPC layer
- **`portable-pty`** — cross-platform PTY (ConPTY on Windows, Unix PTY on Linux)
- **`vte`** — ANSI escape sequence parsing and terminal screen state
- **`tokio`** — async runtime
- **`ort`** (ONNX Runtime) — Kokoro TTS inference, with CUDA execution provider when available
- **`misaki-rs`** — phonemization (default features include espeak-ng fallback for OOV)
- **`cpal`** + **`rodio`** — audio output and queue management; `cpal` input streams also back speech-to-text capture
- **`whisper-rs`** (whisper.cpp) — offline speech-to-text inference, with an optional `stt-cuda` compile feature for the CUDA backend
- **`rubato`** — resampling captured mic audio to Whisper's 16 kHz mono
- **`serde` / `serde_json`** — settings persistence and IPC payloads
- **`uuid`** — pane / split / tab id generation
- **`tracing`** — structured logging

### Frontend: Svelte

- **Svelte** — component framework (low overhead, good reactive performance)
- **xterm.js** — terminal rendering inside the embedded webview
- **Canvas 2D** — waveform visualizer
- Native `<img>` and `<video>` for avatar assets — `<img>` covers PNG/JPG/GIF/WebP, `<video autoplay loop muted playsinline>` covers MP4/WebM/MOV. Element type chosen per-asset by file extension.
- Native `<textarea>` with `spellcheck="true"` for the compose overlay

### Why this stack

An interactive PTY-based TUI requires a real terminal emulator widget; xterm.js is the most mature option, and embedding it requires a webview. Tauri provides this with much lower overhead than Electron. Cross-platform desktop with native performance favors Rust. In-process Kokoro inference with GPU acceleration requires good ONNX bindings, which `ort` provides. Browser-native spell-check on a textarea gives us standard right-click correction for free.

---

## High-Level Architecture

```
┌──────────────────────────────────────────────────────────────────────┐
│                         Tauri Application                            │
│                                                                      │
│  ┌─────────────────────────────────┐  ┌─────────────────────────┐    │
│  │       Rust Backend              │  │    Svelte Frontend      │    │
│  │                                 │  │                         │    │
│  │  ┌──────────────────────────┐   │  │  ┌───────────────────┐  │    │
│  │  │  Tab Registry            │   │  │  │  Layout Tree      │  │    │
│  │  │  N × { PTY + processing  │◀──┼──┼──│  (Pane / Split    │  │    │
│  │  │      + state machine }   │   │  │  │   recursive)      │  │    │
│  │  └──┬──────────────────┬────┘   │  │  └─────┬─────────────┘  │    │
│  │     │ raw bytes        │ TTS    │  │        │ portals        │    │
│  │     ▼                  ▼        │  │        ▼                │    │
│  │  ┌──────────┐    ┌────────────┐ │  │  ┌───────────────────┐  │    │
│  │  │ Display  │    │  TTS       │ │  │  │ xterm.js per tab  │  │    │
│  │  │  bytes   │────┼─▶ Worker   │ │  │  │ (DOM mounted in   │  │    │
│  │  │  channel │    │ (Kokoro)   │ │  │  │  active pane via  │  │    │
│  │  └────┬─────┘    └─────┬──────┘ │  │  │  appendChild)     │  │    │
│  │       │                │ PCM    │  │  └───────────────────┘  │    │
│  │       │                ▼        │  │  ┌───────────────────┐  │    │
│  │       │         ┌────────────┐  │  │  │  Avatar Overlay   │  │    │
│  │       │         │ Audio Out  │  │  │  │  (focused pane's  │  │    │
│  │       │         │ + amp tap  │──┼──┼─▶│   active tab's    │  │    │
│  │       │         └─────┬──────┘  │  │  │   state)          │  │    │
│  │       ▼                │        │  │  └───────────────────┘  │    │
│  │  ┌─────────────────────┴─┐      │  │  ┌───────────────────┐  │    │
│  │  │  State Manager        │──────┼──┼─▶│  Waveform         │  │    │
│  │  │  per-tab AvatarState  │      │  │  │  (sibling of      │  │    │
│  │  │  + permission flag    │      │  │  │   avatar, follows │  │    │
│  │  │  + DoneWhileAway      │      │  │  │   audio amps)     │  │    │
│  │  └────────┬──────────────┘      │  │  └───────────────────┘  │    │
│  │           │                     │  │  ┌───────────────────┐  │    │
│  │           ▼                     │  │  │  Compose Overlay  │  │    │
│  │  ┌──────────────────────┐       │  │  │  (slides up,      │  │    │
│  │  │ Notification Queue   │──────▶│──┼──│   submits to      │  │    │
│  │  │ (per-tab dedup at    │       │  │  │   focused-pane    │  │    │
│  │  │  play-time)          │       │  │  │   active tab)     │  │    │
│  │  └──────────────────────┘       │  │  └───────────────────┘  │    │
│  │                                 │  │  ┌───────────────────┐  │    │
│  │  ┌──────────────────────┐       │  │  │  Status Bar       │  │    │
│  │  │ Settings Handle      │◀──────┼──┼─▶│  (Layouts menu,   │  │    │
│  │  │ + broadcast          │       │  │  │   mute / volume / │  │    │
│  │  │ + debounced save     │       │  │  │   announcements)  │  │    │
│  │  └──────────────────────┘       │  │  └───────────────────┘  │    │
│  └─────────────────────────────────┘  └─────────────────────────┘    │
└──────────────────────────────────────────────────────────────────────┘
```

The shape: PTY bytes per tab flow through a per-tab processing layer that fans them out to two destinations — clean bytes for that tab's xterm.js display, and extracted TTS content for the synthesis pipeline. Tab state machines feed a per-tab avatar state, surfaced through tab status indicators and (for the focused pane's active tab) the avatar overlay. The frontend layout tree owns pane structure; xterm.js DOM elements are portal-mounted into whichever pane currently shows them.

---

## Component Design

### PTY Manager and Tab Registry

The Rust backend owns one PTY per tab. The `TabRegistry` (`src-tauri/src/tabs/`) holds the live set of tabs and per-tab PTY managers; it eagerly spawns every persisted tab's subprocess at app launch, so the user never sees a startup delay when switching tabs. Each subprocess is spawned with the configured command, args, working directory (defaulting to cImp's launch directory), and environment.

Per-tab responsibilities:

- Spawn the configured subprocess attached to a PTY pair
- Read raw bytes from the master end and forward to the per-tab processing layer via a channel
- Forward keyboard input from the active tab's xterm.js (via Tauri IPC) to the master end
- Forward composed text from the compose overlay (followed by an Enter to submit)
- Handle PTY resize when the terminal area changes size
- Detect subprocess exit and route to the state manager (Error for AI-tool tabs, Closed sub-state for Shell tabs — see below)

The wrapper acts as a drop-in replacement for **at most one** harness's CLI — the one whose plugin declares `accepts_passthrough_argv()`, looked up through `registry::passthrough_harness()` (Claude Code today). Any CLI arguments passed to `cimp` are captured at startup (`std::env::args().skip(1)`) and forwarded to that harness's tabs only; forwarding one CLI's flags into another harness is how a tab fails to launch, so two declared takers resolve to `None` rather than a guess. CLI args from settings (per-tab `args`) are applied first, then invocation args from the cImp command line, so persistent flags and one-shot flags compose.

Implementation:

- `portable-pty` handles the Windows ConPTY vs. Unix PTY split internally.
- The PTY reader runs as a dedicated tokio task. PTY reads cannot be made truly non-blocking on all platforms, so it uses a blocking task pool.
- Compose overlay submissions arrive as a single `pty_write` containing the textarea content plus a newline. The subprocess sees this as if pasted-and-submitted; no special protocol.

### Tab Kinds

Two kinds, distinguished by an enum:

```rust
pub enum TabKind {
    AiTool,
    Shell,
}
```

Kind-gated behavior:

| Behavior                               | AiTool                                  | Shell                                                                 |
|----------------------------------------|-----------------------------------------|-----------------------------------------------------------------------|
| TTS markup detection / extraction      | yes                                     | bypassed entirely in processing layer                                 |
| Permission prompt detection            | yes (every registered harness's pattern rows are loaded; tabs of the same harness share them) | no                                              |
| Avatar states reachable                | Idle / Listening / Thinking / Speaking / Error | Idle / Error only                                              |
| Notifications: idle                    | yes                                     | no (would fire constantly for an interactive shell)                   |
| Notifications: awaiting_permission     | yes                                     | no                                                                    |
| Notifications: error                   | yes                                     | yes                                                                   |
| Notifications: exited                  | no                                      | yes (with `{code}` placeholder interpolated)                          |
| Compose overlay submission             | yes                                     | yes                                                                   |
| Subprocess restart on exit             | no (manual via settings)                | yes (Closed sub-state with Enter-to-restart message)                  |

Whether an AI tab is still generating (used to disambiguate Speaking → Thinking vs Speaking → Idle) is carried by the neutral `HarnessOutputStarted` / `HarnessOutputStopped` state signals, which each harness's reader raises from its own turn-boundary evidence. Meaningful only for AI tabs.

### Tab Lifecycle

**The reserved AI tab ids are the registry's, not a literal.** Each `HarnessDescriptor` declares its own `tab_ids` in canonical order, and `registry::canonical_tab_ids()` flattens them across harnesses in declaration order — `claude` → `claude-local` → `opencode` today. That one list drives the integrity check's restore order, the tab-lifecycle inserter's position, and `AiTabId::canonical_order()`; a reserved id no descriptor claims sorts last rather than colliding with slot 0. Which of them actually materialize is the `enabled_ai_tabs` list (a `Vec<AiTabId>`, default `[claude]` on a fresh install):

- Each id in `enabled_ai_tabs` that is missing from `tabs[]` is restored at its canonical position by the integrity check; each reserved AI tab present in `tabs[]` but not in `enabled_ai_tabs` is dropped.
- `claude-local` is the second reserved tab of the *same* harness — the same binary, preconfigured to talk to a local LLM via env-var injection (V1.4-07; replaces the v1.7-and-earlier `aider` reserved id). Harnesses that pick their own provider (OpenCode) declare one tab id, not two.
- `shell-default-1` — the first Shell tab on a fresh install, with a sensible default shell per platform. Despite the reserved-looking id, it is **not** a builtin: `default_shell_1_tab(...)` returns it with `builtin: false`, and the integrity check does not re-seed it. Once the user closes it, it stays closed. Older settings files that persisted `builtin: true` on this id are demoted to `false` on load.

Backend Tauri commands expose tab CRUD:

- `create_shell_tab(name, command, args, cwd, env, notifications) -> TabId` — validates inputs, spawns the PTY, registers the new TabState, persists settings, broadcasts `tab-created`
- `close_tab(tab_id)` — rejects any reserved AI tab id (`HarnessId::from_tab_id` answers a harness); otherwise kills the subprocess, drops processing tasks, removes the TabState, persists, broadcasts `tab-closed`. `shell-default-1` is closable like any other shell.
- `rename_tab(tab_id, new_name)` — updates name, persists, broadcasts
- `reconfigure_shell_tab(tab_id, ...)` — updates persisted config without respawning; the change applies on the next restart of that shell
- `restart_shell_tab(tab_id)` — kills the running subprocess (if any) and respawns with current config
- `open_settings_window_to_tab(tab_id)` — used by the right-click Configure entry on AI tabs (V1.4-07 Phase A). Opens the Settings window and emits a `settings-deep-link` event the Settings frontend listens for to scroll/focus the matching tab section. Shell tabs continue to use the dedicated `ConfigureTabDialog` modal.

The state manager broadcasts `TabCreated`, `TabClosed`, `TabRenamed`, and `TabClosedStateChanged` events for the frontend to react to.

### Shell-Tab Closed Sub-State

When a Shell-tab subprocess exits — `exit` typed, shell crashed, killed externally — the tab transitions to a `Closed` UI sub-state (a `closed: bool` + `closed_exit_code: Option<i32>` flag on TabState; the avatar state is `Idle`). The xterm.js is overlaid with a centered message: `Shell exited (code N). Press Enter to restart, or close this tab.` Pressing Enter respawns. An `exited` notification fires once per exit event when the user is focused elsewhere. The configured notification text supports a `{code}` placeholder.

Subprocess exit on an AI tab routes to Error; AI-tab auto-restart is out of scope.

If a user-created Shell tab's command no longer resolves at launch (binary uninstalled, path moved), the tab is created in the closed state with `closed_exit_code = None` and a "Shell command not found" message. The user fixes the config via right-click → Configure (which opens `ConfigureTabDialog` for shell tabs) and restarts.

### Processing Layer

The transformation point between subprocess raw output and everything downstream. Lives in `src-tauri/src/processing/`. Per-tab; constructed at PTY-spawn time.

Responsibilities (AI tabs):

1. Parse incoming bytes through a `vte` parser to maintain awareness of terminal screen state
2. Detect `[[TTS]]...[[/TTS]]` tags in rendered text content (across ANSI styling boundaries)
3. Strip TTS tags from the byte stream before forwarding to the terminal display
4. Extract TTS-tagged content and segment it into sentence-bounded chunks for the TTS pipeline
5. Manage flush timing using the hybrid trigger model
6. Handle in-place rewrites by tracking logical document state
7. Detect in-tab prompt patterns (tool-use approvals, multi-choice questions, busy chrome) and emit `PermissionPromptDetected` / `PermissionPromptResolved` signals

Shell tabs get a slimmer pipeline: the vte parser still runs (so xterm.js receives correctly-rendered bytes and screen state stays current) and the hybrid flush keeps rendering pacing consistent, but TTS extraction, the two-view split, sentence segmentation, and permission detection are stubbed out. This is a construction-time gate, not a per-byte conditional.

#### Hybrid flush trigger

Bytes are not forwarded to xterm.js immediately; they are held briefly and flushed on whichever fires first:

- **Stability timeout** (~200 ms default, configurable) — no new bytes for a region for the timeout duration → flush. Handles in-place rewrites cleanly.
- **Maximum hold time** (500 ms, configurable) — no byte is held longer than this. Prevents bursty rendering during sustained streaming; approximates token-by-token at 2 Hz.
- **Completed TTS tag** — when `[[/TTS]]` is detected, the contained content is pushed to the TTS queue immediately, even if surrounding output is still held. Minimizes time-to-first-audio.

The user has accepted that this introduces 200–500 ms of perceptible delay between the subprocess generating output and it appearing in the terminal, in exchange for clean tag extraction and reliable rewrite handling.

#### Tag detection across ANSI boundaries

Subprocesses style text with ANSI escape sequences. A TTS tag may have styled content inside it: `[[TTS]]hello \x1b[1mworld\x1b[0m[[/TTS]]`. The detector maintains two synchronized views — **raw** (with ANSI) and **rendered** (text only) — scans the rendered view for tag boundaries, maps positions back to the raw view, strips tags from the raw stream while preserving styling on the content between them, and extracts the rendered (ANSI-stripped) inner text for TTS.

#### In-place rewrite handling

Subprocess TUIs rewrite lines (input boxes, spinners, status updates). The vte parser maintains a virtual screen state; the layer observes it and only commits text from regions that have stabilized. Implementation hint baked in: we use screen state as a *signal* of what's stable, then forward the original raw bytes for those regions — never reconstruct from screen state, that loses styling.

#### Sentence-boundary segmentation for TTS

Once a complete `[[TTS]]...[[/TTS]]` block is extracted, content is segmented into sentences before being pushed to the TTS queue. Boundaries: `.`, `?`, `!`, `\n\n`, with simple disambiguation for common false positives (decimal numbers, "Dr.", "e.g.", "etc."). Each sentence becomes one TTS request, giving Kokoro complete sentences for natural prosody while enabling streaming playback (sentence N+1 synthesizes while sentence N plays). Fragments without sentence-ending punctuation go through as-is.

#### Prompt detection

**The engine is here; the grammar is not.** `processing/permission.rs` is a harness-neutral matcher: it substring-matches the rendered (ANSI-stripped) tail of a tab's output against a list of pattern rows, where each row lists substrings that must ALL be present (`all_of`), substrings that must ALL be absent (`none_of`), and a `kind` that decides which signal fires on the absent→present (and present→absent) edge:

- `permission` — tool-use / file-edit / bash approvals. Emits `PermissionPromptDetected { tab }`, sets `awaiting_permission`. Resolution is input-driven: while a permission prompt is active, any user input to the PTY clears the flag (`PermissionPromptResolved`); a re-prompt re-sets it on the next detection.
- `question` — multi-option "pick one" prompts. Sets `awaiting_question` and fires the `question` notification template.
- `working` — the harness's busy chrome. Drives the avatar's Thinking↔Idle activity from content rather than from a byte-silence timer, so a thinking pause no longer collapses the avatar to Idle mid-work.

Veto (`none_of`) terms are evaluated only from the start of the row's own earliest `all_of` marker onward, not over the whole tail — the tail is a scrollback window, so stale menu chrome scrolled *above* a live approval prompt must not suppress it.

What a row matches *on* is a transcription of somebody else's terminal chrome, and every word of it is a dependency on a product cImp neither pins nor ships. So the rows are **declared by the harness that owns them** (`HarnessPlugin::permission_patterns`, `harness/<id>/prompts.rs`), and the shipped `patterns.json` seed is the neutral concatenation over the registry. Core never spells a marker. The per-harness state differs and the docs should not pretend otherwise:

- **Claude Code** ships enabled rows, matched by *grammar* rather than by literal default chrome — the chord labels in the footer are user-remappable and its segments are conditional, so the old exact `Esc to cancel · Tab to amend` marker is retired production data, not what the detector looks for. See `../src-tauri/src/harness/claude/README.md`.
- **OpenCode** ships its rows **disabled**, with a capture recipe attached: its `--mini` permission and working chrome has never been captured live, and a wrong guess enabled by default would mis-fire the avatar and the permission notification on every OpenCode tab. See `../src-tauri/src/harness/opencode/README.md`.

Brittleness against upstream changes is inherent and tracked as a capability row (`perm.tui_scrape`) in `harness/contract.rs` rather than assumed away. `RUST_LOG=perm_capture=debug` dumps the rendered tail the detector matches against, which is how a row gets re-characterized after an upstream UI change. Users can edit `patterns.json` directly; a deleted or corrupt file falls back to the registry-composed defaults.

Both reserved Claude tabs (subscription and local) run the same rows — they are the same harness, hence the same binary and the same chrome.

### TTS Engine

In-process Kokoro inference via the `ort` crate. Lives in `src-tauri/src/tts/`.

- Loads the Kokoro ONNX model at startup; initializes CUDA execution provider when available, falls back to CPU
- Accepts text segments via a channel from per-tab processing layers
- Phonemizes via `misaki-rs` (default features include espeak-ng fallback for OOV)
- Synthesizes audio (PCM, 24 kHz, mono) per segment
- Pushes completed buffers to the audio playback queue
- Filters at the worker by the **TTS active tab cell** — only the focused-pane active tab's segments synthesize; others are dropped at queue intake to save GPU cycles

Voice embeddings are stored separately from the main model; the engine loads the configured voice at init and on settings change.

### Audio Playback

`cpal` for the cross-platform output stream; `rodio` for queue management. Lives in `src-tauri/src/audio/`.

- Opens an output stream on the system default device at app launch (the OS default change at runtime is documented as a "reconnect audio" deferral)
- Sequential queue of synthesized buffers
- Exposes amplitude data via a small ring buffer (last N samples) for the visualizer; an amplitude-streamer task ships values to the frontend at 60 Hz via Tauri events
- Volume / mute via settings; mute drops incoming buffers without playing
- Interrupt-on-input cancels current playback when the user starts typing into the focused tab

### State Manager

Per-tab state, focused-pane-aware audio gating. Lives in `src-tauri/src/state/`.

```rust
pub struct TabState {
    id: TabId,
    kind: TabKind,
    name: String,
    avatar_state: AvatarState,         // Idle | Listening | Thinking | Speaking | Error
    awaiting_permission: bool,         // independent flag — can stack with any avatar state
    done_while_away: bool,             // UI flag (see below)
    harness_output_active: bool,       // AI tabs only
    closed: bool,                      // Shell tabs only
    closed_exit_code: Option<i32>,     // Shell tabs only
}
```

State signals (`StateSignal`), tagged with the tab they originated from: `UserKeystroke`, `UserSubmit`, `HarnessOutputStarted`, `HarnessOutputStopped`, `TtsPlaybackStarted`, `TtsPlaybackStopped`, `PermissionPromptDetected`, `PermissionPromptResolved`, `SubprocessExited`, `AudioError`, `TtsError`, `ErrorAcknowledged`, `ComposeContentChanged`, `TabActivated`.

State events broadcast (`StateEvent`): `StateChanged`, `AwaitingPermissionChanged`, `DoneWhileAwayChanged`, `ActiveTabChanged`, `TabCreated`, `TabClosed`, `TabRenamed`, `TabClosedStateChanged`, `NotificationFired`.

State transitions per tab (AI tabs):

- User keyboard activity OR non-empty compose textarea → `Listening`
- First non-input byte from PTY after user input → `Thinking`
- TTS audio starts playing → `Speaking`
- TTS queue empty + subprocess still generating → `Thinking`
- TTS queue empty + subprocess idle past stability window → `Idle`
- Subprocess exit / TTS error / audio error → `Error`
- Error acknowledged → `Idle`

For Shell tabs: only `SubprocessExited` (→ Closed sub-state) and `AudioError` / `TtsError` (which can't actually fire — TTS is bypassed — but the path is generic) move state. User input and output do not transition Shell-tab avatar state, so a running shell sits in `Idle` and a hard failure goes to `Error`.

The **focused pane's active tab** is "the active tab of the application" for routing purposes:

- The avatar reflects its state
- Audio plays only for it (the audio-target-tab gate; samples for any other tab are dropped at the audio queue)
- The compose overlay submits to it
- The window title reflects it

When the focused pane changes (or its active tab changes), the frontend pushes the new audio target to the backend. Pending audio for the previously-targeted tab is dropped — same rule as v1's tab-switch semantics, generalized to pane focus.

### Notification System

When a tab's state changes meaningfully and the user is focused elsewhere, an audible announcement plays. Lives in `src-tauri/src/notifications/`.

#### Triggers

| Transition (AI tab)                | Notification event       |
|------------------------------------|--------------------------|
| Anything → Idle                    | `idle`                   |
| Anything → AwaitingPermission      | `awaiting_permission`    |
| AskUserQuestion-style prompt fires | `question`               |
| Anything → Error                   | `error`                  |

| Transition (Shell tab)             | Notification event       |
|------------------------------------|--------------------------|
| Anything → Error                   | `error`                  |
| Subprocess exit                    | `exited`                 |

`Working` (Thinking / Speaking) is intentionally not a trigger — it would fire on every input the user submits.

#### Queue rules

Notifications go through a dedicated queue, separate from per-tab TTS:

1. **Append on trigger** — tab id, event type, configured text.
2. **Per-tab dedup at play-time** — when playback fires, the queue is filtered so for each tab only the most recent notification is retained. Notifications from different tabs all survive in arrival order.
3. **Wait for active TTS** — pending notifications wait for any currently-playing per-tab TTS to finish, then play in arrival order, then per-tab TTS resumes.

#### Edge-case rules

- **Idle is suppressed while `awaiting_permission` is set on the same tab.** When a harness stops printing to ask permission, the avatar drops to Idle (output-stopped) at roughly the same instant the permission detector fires. Without this rule the user hears both. The check runs at enqueue time against the manager's most-recent-known `awaiting_permission` flag; the Idle notification is dropped silently when that flag is true.
- **Drain is debounced ~200 ms after the first enqueue when audio is idle.** Closely-spaced related events (an Idle and an AwaitingPermission landing microseconds apart for the same logical edge) get a chance to coalesce in the queue before drain. If new events arrive during the window, the existing deadline stands — they ride the same drain. Audio idle-edges drain immediately on the next pulse; the debounce only applies to the cold-start case.

#### Configuration

- **Global "announcements enabled" toggle** in settings (default ON). When OFF, no notifications fire.
- **Quick toggle** in the bottom status bar — same effect as the settings toggle, accessible without opening settings.
- **Per-(tab, event) notification text** in settings. Empty string disables that specific notification while leaving others active.
- **`behavior.announce_focused_tab`** (default OFF) — when ON, notifications fire even for the focused-pane active tab. Default OFF preserves the historical "background-only" semantics.
- **`behavior.follow_avatar`** (default OFF) — when ON, `tts.mute` syncs to the avatar's visible/hidden state (hiding mutes, showing unmutes). The frontend handles the sync; the backend just persists the flag.

### Layout System

The content area is a recursive tree of panes and splits. Lives in `src/lib/layout/` (frontend) plus a thin persistence pass in the backend.

```typescript
type LayoutNode = SplitNode | PaneNode;

interface SplitNode {
  type: 'split';
  id: string;
  direction: 'horizontal' | 'vertical';  // CSS-flexbox naming: horizontal = side-by-side
  ratio: number;                          // first child's share, 0.0..1.0
  first: LayoutNode;
  second: LayoutNode;
}

interface PaneNode {
  type: 'pane';
  id: string;
  tab_ids: TabId[];                       // ordered tab list within this pane
  active_tab_id: TabId | null;            // null only when tab_ids is empty (transient)
}

interface LayoutState {
  tree: LayoutNode;
  focused_pane_id: string;
}
```

Direction convention follows CSS flexbox: `horizontal` arranges children side-by-side with a vertical splitter between them; `vertical` stacks them top-to-bottom. This is the *opposite* of tmux's `split-window -h` naming — we picked the flexbox convention.

Invariants:

- Tree is non-empty (at minimum, one Pane root)
- Every Pane has a unique id
- Every TabId in any pane's `tab_ids` corresponds to an existing tab in `settings.tabs`
- Every tab in `settings.tabs` appears in exactly one pane's `tab_ids` (no orphans, no duplicates) — enforced by the integrity sieve at load time
- A pane's `active_tab_id` is either `null` (empty pane, transient during operations) or an entry in its `tab_ids`

#### Pure tree operations

`src/lib/layout/tree.ts` exposes immutable operations: `findPane`, `findSplitContaining`, `insertTabIntoPane`, `removeTab`, `moveTab`, `setActiveTabId`, `splitPane`, `closePane`, `setSplitRatio`, `eachPane`, `firstPane`, `findPaneContainingTab`. All take a tree, return a new tree (or the same reference if unchanged) — callers swap the new root into the layout store, which fires Svelte reactivity.

The store (`src/lib/layout/store.ts`) wraps these ops with lifecycle concerns: focus follows the dropped tab on every drag commit; non-root empty panes collapse via the standard binary-tree-deletion rebalance (the parent Split is replaced by the surviving sibling); the root pane is never destroyed.

#### Pane rendering and tab DOM portaling

Each leaf pane is rendered by `src/lib/Pane.svelte` and contains its own tab bar plus a placeholder div for the active tab's xterm.js. Splits are rendered by `src/lib/Split.svelte` as a flexbox container with a 4 px draggable splitter between children.

Each tab's xterm.js root element lives in a hidden offscreen container, owned by `src/lib/terminals.ts`. Pane components do not own xterm.js instances; a reactive effect on each pane watches `pane.active_tab_id` and `appendChild`s the matching xterm host into its placeholder. `appendChild` *moves* DOM nodes rather than copying them, so a tab's terminal state, scrollback, and PTY connection survive every pane-and-tab operation including drag-tearing across panes. When the active tab changes, the previous tab's DOM is detached back to the offscreen container before the new one is attached.

#### Drag-and-drop tab tearing

Custom mouse-based drag handler (HTML5 DnD has too many cross-webview quirks). `src/lib/dnd/`.

Flow: `mousedown` on a tab records the source pane and tab id; movement past a 4 px threshold enters drag mode and shows a ghost tab tracking the cursor. On every `mousemove`, the cursor is hit-tested against every visible pane via `getBoundingClientRect`, and a drop zone is computed:

- ~25 % of any edge → split-zone in that direction (creates a new sibling pane)
- Center 50 % → move-to-pane zone (appends to target's tab_ids)
- Over the tab bar → reorder zone (insert before the nearest tab) or move-to-pane (past the last tab)

A translucent overlay shows where the dropped tab will land. `mouseup` commits via the appropriate tree op. `Esc` cancels.

If the dragged tab was the source's only tab, the source pane becomes empty after removal and is collapsed in the same atomic update.

#### Splitter resize

Each Split has a 4 px draggable line between its two children (`col-resize` cursor for horizontal, `row-resize` for vertical). Drag adjusts the split ratio. The data-model ratio is clamped to `[0.05, 0.95]`. A separate min-pixel clamp at the drag handler keeps neither pane below `MIN_PANE_WIDTH_PX` (200 px) or `MIN_PANE_HEIGHT_PX` (100 px). If the window shrinks such that a stored ratio would violate min sizes, the ratio is clamped on render only — the user's stored preference is not overwritten unless they actively drag.

#### Pane lifecycle and shortcuts

A pane is created when the app launches with one in the persisted layout, when a drag-drop creates a split, or when a keyboard / context-menu split fires. A pane is destroyed when its last tab moves or closes (and it isn't root) or when the user invokes Close pane (all tabs move to the surviving sibling subtree's leftmost leaf, then the source pane collapses).

Pane-aware keyboard shortcuts:

- `Ctrl+1..9` — switch active tab within the *focused pane* (1-indexed, no-op when fewer than N tabs)
- `Ctrl+Alt+ArrowKey` — move focus to the geometrically-adjacent pane (overlap-aware, no-op when no adjacent pane exists)
- `Ctrl+\` — split focused pane horizontally with a fresh Shell tab in the new pane
- `Ctrl+Shift+\` — split vertically with a fresh Shell tab
- `Ctrl+Shift+W` — close focused pane (moves its tabs to the surviving sibling, then collapses)
- `Ctrl+T` — new shell tab in the focused pane
- `Ctrl+W` — close active tab in the focused pane

Right-click on a pane's tab-bar background (not on a tab — that's the tab context menu) opens a popover with Split horizontally (creates a new pane with a fresh Shell tab), Split vertically (same, stacked), Close pane, and Move all tabs to → submenu.

#### Layout persistence

The full layout tree and `focused_pane_id` are persisted to settings. Every layout-store mutation triggers a 250 ms debounced `save_layout` IPC; the backend writes into `Settings.layout` and the existing 500 ms debounced disk save coalesces further. On launch, the persisted layout is hydrated through `validateAndRepairLayout`:

1. Drop tab ids no longer in `settings.tabs` (set per-pane `active_tab_id` to first remaining if it was dropped)
2. Repair invalid `focused_pane_id` to leftmost leaf
3. Place orphan tabs (in `settings.tabs` but in no pane) at the end of the focused pane
4. Collapse non-root empty panes
5. Defensive: empty root pane with non-empty tab list → rebuild from defaults

A backend integrity check covers steps 1 and 2 before deserialization succeeds, so a hand-edited file can't crash the app. The frontend handles the deeper repair (orphan placement, empty-pane collapse) since it owns the tree-op helpers.

#### Named layout presets

The user saves the current layout under a name from a Layouts popover in the bottom status bar. Presets live in `Settings.layout_presets`, ordered by `created_at` (RFC 3339, second precision). Restoring a preset replaces the live tree wholesale; the integrity sieve runs over the restored tree against the current tab list so orphans and missing tabs are handled the same as on load.

Presets carry only the tree, not focus — focus follows the user's next click, seeded at the leftmost leaf on restore.

The Layouts menu (status-bar popover): Save current layout as…, Recent (top 5 by `created_at`), Manage presets… (a dialog with inline rename and confirm-delete).

### Avatar Overlay (Frontend)

A floating overlay on top of the full-window terminal area, configurable in size, position, and opacity. `src/lib/AvatarOverlay.svelte` plus `src/lib/avatarConfig.ts`.

#### Layout

- Positioned at one of four configurable corners (`top-right` default; also `top-left`, `bottom-right`, `bottom-left`) with a configurable margin from the corner edges
- Configurable independent width and height (default 240 × 240)
- A thin vertical toggle button on the screen-edge side spans the full vertical extent. Clicking it hides the avatar; the toggle remains visible when hidden so the user can re-show
- The waveform visualizer renders in the bottom band of the avatar's area as a *sibling* (not child) so the avatar's opacity does not propagate to the waveform

#### State images

Each of the five avatar states has a configured asset (PNG / JPG / GIF / animated WebP / MP4 / WebM / MOV). Animated formats and videos loop while the state is active. Element type is chosen per-asset by file extension: `<video autoplay loop muted playsinline>` for video, `<img>` for everything else. If a state has no asset configured, fall back to Idle.

Image fitting: **contain** (letterbox if needed). Source assets with alpha render with transparency intact, so the terminal shows through where the artwork doesn't reach.

Cross-platform note: WebView2 plays H.264 MP4 natively; WebKitGTK depends on the host's GStreamer plugins, so on Linux prefer WebM/VP9 if codec coverage is uncertain.

#### Shared transition animation

A single transition asset (with its own duration, configurable ms) is configured globally — not per-state. The same asset plays between any state change at runtime.

Behavior:

- Plays on every state change at runtime, including transitions back into Idle
- **Does not** play at app launch — the avatar opens directly into the Idle image
- Optional: empty / null path skips transitions; state changes snap directly
- Duration-based: after the configured duration elapses, the looping state image takes over
- Interruption: a state change during an in-progress transition stops the running transition and starts a fresh one for the new change. Transitions are never queued.
- Visualizer is independent — the waveform follows Speaking state directly and does not hide during transitions

#### Opacity

The avatar overlay has a configurable global opacity (default 80 %, range 30 %–100 %), applied to the avatar image and toggle button uniformly via CSS opacity inheritance. Composes multiplicatively with any source-image alpha.

The waveform's opacity is **independent** of the avatar overlay's. The waveform is a sibling element with its own opacity setting — always rendered at its own configured opacity regardless of how transparent the avatar is.

#### Visibility persistence and settings access

The avatar's visible/hidden state persists across app restarts. A gear icon button on the avatar (top-right corner regardless of avatar position) opens the settings window; when the avatar is hidden, the gear is hidden along with it, and the configurable `open_settings` shortcut is the only way to access settings.

### Compose Overlay (Frontend)

A spell-checking textarea for composing longer messages. `src/lib/ComposeOverlay.svelte`.

- Slides up from the bottom of the application window when triggered
- Spans the full window width
- Auto-grow height: starts compact (~80 px), grows to a configured max (~300 px), then scrolls internally
- Visually distinct from the terminal (different background tone, top border)

Behavior:

- Triggered by `open_compose` (default `Ctrl+Shift+E`); receives focus on open. Browser-native spell-check (red squiggles, right-click corrections) is active on the textarea
- The terminal underneath remains fully interactive — the user can click into it, select and copy, type into the focused pane's active tab
- `submit_compose` (default `Ctrl+Enter`) sends the textarea content + newline to the **focused pane's active tab**, then closes the sheet. Append mode: the wrapper does not clear or modify any existing input line at the destination
- `cancel_compose` (default `Escape`) closes without submitting; draft is discarded
- Submit fires only when the textarea has focus; cancel fires globally while the sheet is open

While the sheet is open and contains non-empty text, the state manager treats this as user input activity for the focused-pane active tab (transitions to Listening). Empty textarea = no Listening signal from the compose sheet (terminal keystrokes still trigger Listening separately).

If the focused pane changes while the user is composing, the *next* submit targets the new focused pane's active tab. The overlay stays open across focus changes.

### Bottom Status Bar

A thin horizontal strip below the terminal area. `src/lib/StatusBar.svelte`.

- **Left side**: Layouts menu — a small button that opens a popover with Save current layout as…, Recent presets (top 5 by created_at), Manage presets…
- **Right side**: three controls in a row:
  - Mute TTS button (icon: speaker with slash when muted)
  - Disable announcements button (icon: bell with slash when disabled)
  - Volume slider (small horizontal slider with a speaker icon)

Sizing: bar height ~28 px, icons ~16-20 px, slider ~80-100 px wide. Subtle background (slightly darker than the surrounding area), thin top border, hover effects, tooltips.

Behavior is straightforward bindings: mute toggles `tts.mute`, announcements toggles `behavior.announcements_enabled`, volume binds to `tts.volume`. Same settings remain accessible via the full settings window.

### Settings Store

Two JSON files participate, both under `src-tauri/src/settings/`:

- **Global baseline** — `<exe-dir>/settings.json`. Portable; written once on first launch when missing and rewritten only on migration / integrity repair. Hand-edit to change defaults.
- **Per-folder custom overlay** — `<launch_cwd>/.cimp/config.json`, inside the project's `.cimp` data dir (the same folder that holds the code-graph `graph.db`). Partial JSON object containing only the keys that differ from the global baseline. Created automatically the first time the user customizes anything from a given working directory, deleted automatically when the diff is empty. Layered on top of global at load via deep-merge. A pre-consolidation overlay at the old loose path `<launch_cwd>/.cimp.custom.config.json` is migrated into `.cimp/` on the next launch.

This replaces an earlier design that wrote a single file under `dirs::config_dir().join("cImp")`. The portable + overlay model lets the user (a) carry the binary as a self-contained portable directory, and (b) keep per-project customizations alongside the project rather than in OS-global config.

`load(default_shell, launch_cwd)`:

1. Read and parse the global file (seeding defaults if absent or quarantining + reseeding if corrupt).
2. Read the overlay if present; deep-merge it onto the global value.
3. Run the migration cascade against the merged `serde_json::Value` so a hand-imported legacy file at the global path still upgrades.
4. Typed-deserialize into `Settings`; on failure fall back to global-only, never panic.
5. Run the integrity check (see below). If anything migrated or repaired, persist the post-pass state — to the overlay if one was in play, otherwise to global.

Loaded result is held in memory in `SettingsHandle` (`Arc<Mutex<Settings>>` plus a `tokio::sync::broadcast` channel and a save signal). The handle also carries a snapshot of the global baseline so the saver can compute diffs without re-reading disk. Mutations:

1. `SettingsHandle::set(new)` updates in-memory state, broadcasts the new struct to every subscriber (TTS engine, audio output, processing layer, notification manager, frontend), and signals the saver.
2. The saver task waits a 500 ms debounce window, then computes `diff(current, global_baseline)` and writes the result to the overlay file. An empty diff deletes the overlay rather than writing `{}`.
3. Tauri command handlers for individual settings sections call `set` after their mutation.

Components subscribe to specific slices they care about (`tts`, `avatar`, `display`, etc.) via Svelte derived stores on the frontend and broadcast receivers on the backend.

#### Migrations

`src-tauri/src/settings/migration.rs` runs on the merged `serde_json::Value` *before* typed deserialization, so older shapes can be detected and transformed without struct evolution causing errors. Each migration writes a backup of the original alongside the file (`settings.json.v1.0.bak`, `settings.json.v1.1.bak`, …, `settings.json.v1.8.bak`), with a unix-timestamp-suffix rotation if a backup of the same version already exists.

Detection runs as a cascade so a sufficiently old file passes through every step in one launch (a v1.0 file lands at v1.9 with eight backups). The full set, in order:

- **v1 → v1.2** — top-level `claude_code` object with no `tabs` array; lifts to v1.2 by synthesizing a Claude tab from the carried `extra_cli_args`.
- **v1.1 → v1.2** — `tabs` is an *object* with `claude` / `aider` keys; transforms into a v1.2 `tabs` *array* with reserved ids and the default Shell tab.
- **v1.2 → v1.3** — `tabs` is an array but the `layout` key is absent from the top-level object. Synthesizes a single root pane containing every tab in order, picks the focused-pane active tab from `session.active_tab_id` (or the first tab), and seeds `layout_presets: []`. The detector triggers only when `layout` is *entirely absent* — files with `"layout": null` are skipped so backup rotation doesn't fire on every launch.
- **v1.3 → v1.4** — V1.4-01 adds the top-level `terminal` group (with a `theme` sub-group) and per-tab `theme_override`. Drops the dead `display.theme` field at the same time.
- **v1.4 → v1.5** — V1.4-02 adds `terminal.background` and per-tab `background_override`.
- **v1.5 → v1.6** — V1.4-04 B adds `terminal.background.presets`.
- **v1.6 → v1.7** — V1.4-04 D adds `terminal.scrollback` and explicitly stamps the C.4 `terminal.background.preview_category_flips` flag.
- **v1.7 → v1.8** — V1.4-07 drops the aider tab kind, adds the global `claude_local` provider config, and adds the per-AI-tab `use_local_provider` flag. The aider tab is rewritten in place to `claude-local` (id, name, command, `use_local_provider`, `tts_injection` re-enabled). Layout-tree references to `"aider"` (in `tab_ids`, `active_tab_id`, layout presets, `session.active_tab_id`) are rewritten to `"claude-local"` in the same pass so the integrity check sees a self-consistent file.
- **v1.8 → v1.9** — adds the `claude_tabs_enabled` setting (`Cloud` | `Local` | `Both`). The migration infers the initial value from the existing tabs array so users who had both Claude tabs in v1.8 keep both.

#### Integrity check

After migration and typed deserialization, an integrity pass repairs hand-edited files:

1. **Reserved AI tabs forced to `builtin: true`.** Any tab whose id a `HarnessDescriptor` claims cannot be demoted by a hand-edit.
2. **`shell-default-1` demoted to `builtin: false`.** Older settings files that persisted `builtin: true` on this id are corrected — closability is uniform across all shell tabs.
3. **`use_local_provider` coerced on reserved AI tabs.** Each reserved id has one canonical value (`AiTabId::uses_local_provider()` — true only for the local-provider variant `claude-local`), so a hand-edit can't silently flip a subscription tab into local-LLM mode. A harness that picks its own provider is never "local".
4. **Reserved AI tabs reconciled with `enabled_ai_tabs`.** Ids listed but missing from `tabs[]` are restored at their canonical registry position; reserved AI tabs present but not listed are removed from the array. `shell-default-1` is intentionally untouched here — it's a regular closable shell.
5. **Layout sanity.** Tab refs in panes that don't exist in `settings.tabs` are dropped; an invalid `focused_pane_id` is reset to leftmost leaf.

The frontend's `validateAndRepairLayout` runs after this on hydration and handles deeper concerns (orphan placement, empty-pane collapse).

### Offload: local task delegation (V8-01) + backend pool (V8-02) + warm pool & MCP host (V8-03)

Offload lets the frontier session in an AI tab hand a self-contained, token-heavy subtask to a **subordinate local/remote model** and receive only the synthesized result — the frontier model stays in charge, but the intermediate search/read/summarize tokens never enter its window. It is exposed as a single `offload_task` MCP tool, advertised session-scoped to cImp-launched AI tabs through whatever mechanism that harness's plugin declares for delivering an MCP server (never writing into the harness's own global config).

cImp bridges two MCP roles: toward the AI tab it is an MCP **server** (the hidden `cimp --offload-mcp` subcommand, launched with that harness's registered `--consumer` token); toward any user tool servers it is an MCP **host/client**. The `llama-server` itself never speaks MCP — cImp runs the agent loop (`offload/agent.rs`), translating between the model's OpenAI-style `tool_calls` and each tool's executor. A consumer token nobody registered is **refused**, not defaulted: the proxy start fails with the registered list rather than silently serving another harness's tool set.

**Where the loop lives (V8-03).** The agent loop, the backend pool, the router, and the MCP host all run in the **long-lived cImp app** (`offload/service.rs::OffloadService`), not in the per-session child. Only the app sees every in-flight offload across all AI tabs, so it owns a **global concurrency gate** and feeds the router honest `in_flight` (which is why V8-02's spill/fail-over finally works), keeps **warm** MCP-host connections across calls, and renders the capability description from **live** health. The `--offload-mcp` child shrinks to a **proxy with a self-contained fallback**: when the app is up it forwards to an authenticated loopback endpoint; when the app is down (headless cron, mid-restart) it runs the V8-02 path itself (native-only, no warm host). Both paths share the pure router and the agent loop, so they can't drift.

```
  AI tab (frontier) ──offload_task──▶ cimp --offload-mcp  (thin proxy per session)
                                         │  POST /run · GET /describe · GET /events
                                         │  (127.0.0.1, ephemeral port + bearer token,
                                         │   discovery file next to the exe)
                                         ▼
                          cImp app · OffloadService
                            ├─ global concurrency gate (all tabs)
                            ├─ router::select(task, LIVE in_flight) ─▶ ONE backend
                            ├─ MCP host (warm): ddg/fetch/context7/git/filesystem
                            │     namespaced, read-class only, fs confined
                            └─ agent loop, scoped to the chosen backend
        ┌──────────────── backend pool ────────────────┐
        │  Local  (cImp owns the llama-server process) │
        │  Remote LAN  (health-checked URL, no process) │
        │  Remote Cloud (URL + auth + consent)          │
        └───────────────────────────────────────────────┘
                                         │
                    ONE synthesized string ─▶ the AI tab

  app down ▶ the child runs the same router + loop itself (native tools only)
```

Key types (all behind the `Backend` seam so the pool is additive over V8-01's single local server):

- **`offload/service.rs::OffloadService`** — app-owned home of the loop + pool + router + global gate + MCP host. Resolves the pool from **live** state (the supervisor's running `LlamaServer` handles for honest `in_flight`/`n_ctx`, plus cached warm `RemoteBackend` handles), routes, and runs the loop with a host-aware scoped router. Exposes `run`/`describe`/`status` and a change channel for `/events`.
- **`offload/loopback.rs::Loopback`** — the authenticated localhost endpoint (`POST /run`, `GET /describe`, `GET /events`) bound to `127.0.0.1:0`, gated by a per-launch bearer token advertised as `{port, token, pid, root}` in **two** discovery paths under the portable root, both removed on graceful exit: the authoritative per-instance `.cimp-discovery/<pid>.json` and the legacy last-writer-wins `.cimp-offload.json`. Readers go through `read_discovery_for(hint)`, which prefers the deepest per-instance `root` that is an ancestor of the hint and only falls back to the legacy file — both paths resolve, so a reader modelling one of them is wrong (see `ARCHITECTURE.md`). Loopback-only + token auth is the security boundary; the residual local-trust assumption is documented in `MAINTENANCE.md`.
- **`offload/mcp_host.rs::McpHost`** — the warm MCP **client** pool: per server it runs `initialize`+`tools/list`, **namespaces** tools (`ddg__search`), drops write/destructive tools (**read-class only**), confines a `filesystem` server to the allowed roots, multiplexes stdio JSON-RPC by id, tracks per-server health, and pulses a change event when a server connects/drops. Tools merge into the chat `tools` array (then filtered by the backend's `ToolScope`).
- **`offload/server.rs::LlamaServer`** — the Local backend: cImp owns the process (Start/Stop/Reset, autostart), parses the command for host/port/`-np`/`--jinja`/`--kv-unified`, and discovers `n_ctx` from `/props` (unified KV reports the shared window, so it is divided by `-np` to stay a per-slot number).
- **`offload/remote.rs::RemoteBackend`** — a Remote backend (LAN or cloud): a `base_url` (+ optional auth) cImp only health-checks and connects to; `n_ctx` from `/props` or a configured `declared_context`. No process, no tab.
- **`offload/router.rs::select`** — the per-task router: a pure function over `BackendView` snapshots that filters by **tool need** (the privacy hard filter — a local-data task never reaches a cloud backend), then **required context**, then **tier/complexity** and the caller's `tier` hint, then **availability** (spill on busy, fail over on down). One enabled backend → no-op.
- **Per-backend `ToolScope`** — the allow-list over the global tool pool. Local/LAN default to all tools; **cloud defaults to web/docs only**, denying `read_file`/`code_search`/`run_command`/`filesystem`/`git`. Enforced twice: the router won't route a local-data task to cloud, and the agent loop filters the `tools` array *and* refuses a disallowed call.
- **`offload/supervisor.rs::OffloadSupervisor`** — app-owned lifecycle for the Local backends (keyed by name) plus health-probe of Remote backends for the Settings status rows.

Cloud backends are gated behind an explicit data-egress consent toggle and badged distinctly from LAN backends; offloading to cloud sends the task text (and any scoped-in tool results) to a third party, documented in `NOTICE`. Context-budget policing, dynamic thinking, and the read-only-tools posture are unchanged from V8-01. The MCP host extends the read-only posture to the user's tool servers: write/destructive tools are filtered out even when a server offers them, so an offload can search/read/query but never mutate.

---

## Settings Schema

The on-disk JSON shape (`schema_version` 36). The example below is an abridged view of the fully-resolved global file — it shows the groups that matter architecturally, not every field; the per-folder overlay (`.cimp/config.json`) is a partial subset of the same shape.

Two groups are harness-shaped and neither names a harness in code. `harness` is a **map keyed by registry id**, one block per harness, so adding a harness needs no settings migration: core's own per-harness fields (`expose_commands`, `expose_code_audit`, `last_seen`, `last_verified`, `input_profile_status`, `auto_verify`) plus `ext`, an opaque object whose schema, defaults and validation belong to that harness's plugin (`HarnessPlugin::settings_schema`) — core stores it, type-checks the declared keys at the parse boundary, folds the spawn-baked ones into the spawn signature, and never names a key. `enabled_ai_tabs` is a list of reserved AI tab ids drawn from the same registry.

```json
{
  "tts": {
    "voice": "af_heart",
    "speed": 1.0,
    "volume": 1.0,
    "mute": false
  },
  "avatar": {
    "visible": true,
    "size": { "width_px": 240, "height_px": 240 },
    "position": "top-right",
    "margin": { "x_px": 16, "y_px": 16 },
    "opacity": 0.8,
    "images": {
      "idle":      "/path/to/idle.gif",
      "listening": "/path/to/listening.png",
      "thinking":  "/path/to/thinking.gif",
      "speaking":  "/path/to/speaking.gif",
      "error":     "/path/to/error.png"
    },
    "transition": { "path": "/avatar/Transition.mp4", "duration_ms": 400 },
    "waveform": {
      "color": "#bb55ff",
      "line_width": 2.0,
      "glow_intensity": 0.6,
      "opacity": 0.85
    }
  },
  "display": {
    "terminal_font_family": "Consolas, Menlo, \"DejaVu Sans Mono\", monospace",
    "terminal_font_size": 14,
    "show_tts_markup": false
  },
  "terminal": {
    "theme": {
      "name": "Default",
      "custom": null
    },
    "background": {
      "image": null,
      "color": null,
      "opacity": 0.4,
      "blur": 0,
      "size": "cover",
      "position": "center",
      "snapshot_lines": 2000,
      "presets": [],
      "preview_category_flips": true
    },
    "scrollback": {
      "ring_bytes": 262144,
      "persist": true,
      "restore_on_launch": true
    }
  },
  "harness": {
    "claude": {
      "expose_commands": true,
      "expose_code_audit": true,
      "last_seen": "",
      "last_verified": "",
      "input_profile_status": "unverified",
      "auto_verify": null,
      "ext": {
        "local.base_url": "http://localhost:4000",
        "local.auth_token": "sk-dummy",
        "local.model_alias": ""
      }
    },
    "opencode": {
      "expose_commands": true,
      "expose_code_audit": true,
      "last_seen": "",
      "last_verified": "",
      "input_profile_status": "unverified",
      "auto_verify": null,
      "ext": {}
    }
  },
  "enabled_ai_tabs": ["claude"],
  "behavior": {
    "interrupt_on_input": true,
    "auto_speak": true,
    "fallback_silent": true,
    "announcements_enabled": true,
    "follow_avatar": false,
    "announce_focused_tab": false
  },
  "compose": { "min_height_px": 80, "max_height_px": 300 },
  "shortcuts": {
    "open_compose":          "Ctrl+Shift+E",
    "submit_compose":        "Ctrl+Enter",
    "cancel_compose":        "Escape",
    "open_settings":         "Ctrl+,",
    "switch_to_tab_1":       "Ctrl+1",
    "...":                   "...",
    "switch_to_tab_9":       "Ctrl+9",
    "new_shell_tab":         "Ctrl+T",
    "close_tab":             "Ctrl+W",
    "focus_pane_left":       "Ctrl+Alt+Left",
    "focus_pane_right":      "Ctrl+Alt+Right",
    "focus_pane_up":         "Ctrl+Alt+Up",
    "focus_pane_down":       "Ctrl+Alt+Down",
    "split_pane_horizontal": "Ctrl+\\",
    "split_pane_vertical":   "Ctrl+Shift+\\",
    "close_pane":            "Ctrl+Shift+W"
  },
  "tabs": [
    {
      "kind": "ai_tool",
      "id": "claude",
      "builtin": true,
      "name": "Claude",
      "command": "claude",
      "args": [],
      "cwd": null,
      "env": {},
      "tts_injection": { "enabled": true },
      "notifications": {
        "idle": "Claude is idle",
        "awaiting_permission": "Claude is awaiting permission",
        "question": "Claude has a question",
        "error": "Claude encountered an error"
      },
      "first_launch_notice_dismissed": true,
      "theme_override": null,
      "background_override": null,
      "use_local_provider": false
    },
    {
      "kind": "ai_tool",
      "id": "opencode",
      "builtin": true,
      "name": "OpenCode",
      "command": "opencode",
      "args": [],
      "cwd": null,
      "env": {},
      "tts_injection": { "enabled": true },
      "notifications": { "...": "..." },
      "first_launch_notice_dismissed": true,
      "theme_override": { "name": "Solarized Dark", "custom": null },
      "background_override": "disabled",
      "use_local_provider": false
    },
    {
      "kind": "shell",
      "id": "shell-default-1",
      "builtin": true,
      "name": "Shell 1",
      "command": "C:\\Program Files\\Git\\bin\\bash.exe",
      "args": ["--login", "-i"],
      "cwd": null,
      "env": {},
      "notifications": {
        "error": "Shell encountered an error",
        "exited": "Shell exited (code {code})"
      },
      "theme_override": null,
      "background_override": null
    }
  ],
  "processing": { "stability_timeout_ms": 200, "max_hold_ms": 500 },
  "session": { "active_tab_id": null },
  "layout": {
    "tree": {
      "type": "split",
      "id": "split-...",
      "direction": "horizontal",
      "ratio": 0.5,
      "first":  { "type": "pane", "id": "pane-...", "tab_ids": ["claude"], "active_tab_id": "claude" },
      "second": { "type": "pane", "id": "pane-...", "tab_ids": ["opencode", "shell-default-1"], "active_tab_id": "opencode" }
    },
    "focused_pane_id": "pane-..."
  },
  "layout_presets": [
    {
      "name": "Build mode",
      "created_at": "2026-05-06T17:50:32Z",
      "tree": { "type": "pane", "id": "pane-...", "tab_ids": ["claude"], "active_tab_id": "claude" }
    }
  ]
}
```

Notes:

- Every Rust struct uses `#[serde(default)]` so a settings file written by a future or past version still loads — missing fields get defaults, unknown fields are ignored.
- The `harness` block is **machine scope**, like `harness_versions` and `sandbox`: it mixes state written out of band (the version the transcript tap observed, the auto-verify record) with configuration about the harness *install on this machine*. None of that is a property of the project you happen to have open, so `harness` is in `OVERLAY_BANNED_KEYS` and a Settings save writes it through to the physical global file. Its keys are `String`, not a typed id, so a `harness.<something>` block written by a newer build survives a load/save round trip on a build that has never heard of it; every accessor takes a `HarnessId`.
- `tts_injection.enabled` controls whether cImp injects system-prompt content for an AI tab. **The delivery mechanism is the harness's, not cImp's** — Claude Code takes `--append-system-prompt`, OpenCode takes a generated instructions file; the plugin declares which. On by default for every reserved AI tab. Local models vary in how reliably they honor the markup convention, so the local-provider variant is best-effort.
- `use_local_provider` (V1.4-07) gates env synthesis for a tab whose harness declares a local-provider variable set. When `true`, the launch-time spawn merges that harness's declared variables — for Claude Code `ANTHROPIC_BASE_URL` / `ANTHROPIC_AUTH_TOKEN` (and `ANTHROPIC_MODEL` if the alias is non-empty), filled from its `ext` keys — into the process env, with per-tab `env` entries always winning over synthesized values. A harness that picks its own provider declares no such set and is never "local".
- `enabled_ai_tabs` drives the integrity-check reconciliation of the reserved AI tabs; default `["claude"]` on a fresh install. Changing it at runtime closes / re-opens the affected tabs (kills the PTY and drops scrollback when removing; re-creates from the registry defaults when adding).
- Reserved AI tabs cannot be deleted by hand-edits — the integrity check restores them at canonical registry positions when `enabled_ai_tabs` says they should exist, and corrects `use_local_provider` if a hand-edit flipped it. The `shell-default-1` id is *not* a builtin: it ships as the first shell tab on a fresh install but is fully closable, and the integrity check does not re-seed it. User-created Shell tab ids are uuid-based and never collide with reserved ids.
- `behavior.follow_avatar` (default off) auto-mutes when the avatar is hidden; `behavior.announce_focused_tab` (default off) lets announcements fire even for the focused tab.
- `notifications.question` (per AI tab) is spoken when an AskUserQuestion-style multi-option prompt fires. Empty string disables.
- `session.active_tab_id` is a legacy field. It still exists in the struct and is updated by the runtime on tab activation, but the v1.2 → v1.3 migration drops it from the file (the layout's per-pane `active_tab_id` plus `focused_pane_id` are the source of truth for active-tab state). Cleanup of the runtime write path is deferred.
- `layout: null` on fresh installs and on the very first hydration after migration if for some reason no layout was synthesized; the frontend's `defaultLayoutForTabs` handles the null case by building a single root pane.
- `terminal.background` (V1.4-02) has independent `image` and `color` fields plus opacity/blur/size/position controls that apply only when an image is set. The four-cell rendering matrix (image × color, each Some/None) drives a discriminated `RenderingMode` — `'none'` and `'color'` use xterm.js's canvas renderer (fast); `'image'` uses the in-core DOM renderer with `allowTransparency: true` and CSS layering on the host. Toggling between fast and image categories triggers a debounced Terminal recreate; same-category changes (color tweak, slider drag) apply in place. Per-tab `background_override` is three-state: `null` inherits the global, `"disabled"` opts out (theme bg only), an object replaces the global wholesale. The Configure Tab dialog (shell tabs) and the per-AI-tab Settings section both surface the override row, both reusing the shared `BackgroundConfigEditor` component. `terminal.background.presets` (V1.4-04 B) holds named global presets the user can apply from the Background editor; `preview_category_flips` (V1.4-04 C.4) gates whether per-tab Configure dialog edits that would flip renderer category preview live or are deferred to Save. See `MILESTONE-V1.4-02-terminal-background.md` and `MILESTONE-V1.4-03-terminal-background.md`.
- `terminal.scrollback` (V1.4-04 D) configures the per-tab cross-restart scrollback ring buffer. `ring_bytes` caps in-memory size (default 256 KB); `persist` flushes on graceful exit; `restore_on_launch` replays on next `pty_start`.

- **PTY rebind protocol (V1.4-03).** xterm.js's renderer is decided at `Terminal` construction (`allowTransparency` and the canvas vs. DOM split are constructor-only), so toggling the image background requires destroying the xterm Terminal and constructing a new one. To preserve the shell session across this destroy/create cycle, cImp uses `pty_rebind_channel` — the PTY and its child stay alive; only the IPC `Channel<String>` is swapped. Implementation: a per-PTY `mpsc::Sender<ProcessorControl>` (capacity 4) lets the manager push `ChannelChange(new_channel)` to the processor task's select loop, where the existing `mpsc::Receiver::recv` cancel-safety guarantee keeps byte ordering intact. `@xterm/addon-serialize` captures a snapshot of the visible scrollback before destroy and replays it into the new xterm via `term.write` *before* `pty_rebind_channel` resolves; xterm's FIFO write queue ensures the snapshot lands ahead of any live byte arriving after the rebind. The new `Terminal` is constructed with the previous instance's `rows`/`cols` so replayed cursor positions align with the new grid.

- **Cross-restart scrollback (V1.4-04 D).** Every PTY's output is mirrored into a per-tab ring buffer capped at `terminal.scrollback.ring_bytes` (default 256 KB ≈ 600 lines of dense ANSI). On graceful exit (`tauri::RunEvent::ExitRequested`) each tab's buffer is flushed to disk under the app data dir, keyed by sanitized tab id; on next `pty_start` the file is read back and replayed into the new xterm before any live PTY bytes. Settings: `persist` and `restore_on_launch` (both default `true`) gate the disk write and read-back independently. Orphan files (tabs that no longer exist) are pruned at startup against the registry's known ids.

---

## Concurrency Model

Tokio with multiple cooperating tasks coordinated via channels. No shared mutable state except the settings struct (behind `Arc<Mutex<Settings>>` in `SettingsHandle`) and the audio target tab cell (an `Arc<RwLock<TabId>>`).

Tasks:

- **Per-tab PTY reader** — N tasks, one per tab. Blocking reads from the PTY master, sends bytes to the per-tab processing channel
- **Per-tab processing** — N tasks. Vte-parses, runs the hybrid flush, detects TTS tags + permission patterns, segments
- **TTS synthesis worker** — single. Receives text segments via mpsc from all per-tab processors, filters by audio-target-tab cell, runs ONNX inference, sends PCM to audio
- **Audio playback** — single, via `rodio::Sink`. Plays buffers sequentially, exposes amplitude data
- **Amplitude streamer** — single. At 60 Hz, reads recent amplitude data, emits to frontend via Tauri events
- **State manager** — single. Receives `StateSignal`s from PTYs, processing layers, TTS, audio, and compose; computes per-tab state transitions; broadcasts `StateEvent`s
- **Notification manager** — single. Subscribes to `StateEvent`s, applies trigger / suppression / debounce rules, drives the notification queue, plays via the same audio output
- **Settings saver** — single. Receives `set()` signals via mpsc, debounces 500 ms, writes the file

When a tab activates (or the focused pane changes such that its active tab changes):

- The frontend pushes the new audio-target-tab id to the backend
- The TTS worker filters incoming segments against that id; non-matching segments are dropped at intake
- The audio queue is cleared (in-flight playback stops, pending segments discarded)
- The previously-active tab's pending TTS synthesis queue is also cleared — segments that hadn't synthesized yet are dropped, not held

Background tabs continue to: run their PTY subprocess, process bytes (terminal state stays current), track avatar state, trigger notifications when criteria are met. Background tabs do not: synthesize TTS, play audio, or render their xterm.js to the focused pane (they're DOM-portaled offscreen until their pane becomes the focused one and they're the active tab).

---

## TTS Markup Convention

`[[TTS]]...[[/TTS]]` chosen over markdown-based markers (italics, blockquotes) because:

- It is unambiguous and has no collision with normal markdown usage
- The model has explicit control over what is spoken vs. what is displayed
- The processing layer can strip tags cleanly so the user never sees them
- Markdown markers force a constant negotiation between "this italic is for emphasis" vs "this italic should be spoken," which constrains the model's natural use of formatting

The convention is communicated to the model via a `--append-system-prompt` instruction at launch (the contents live in `src-tauri/src/tts/runtime_prompt.md` and are surfaced as the `tts_injection.instructions` settings field per AI tab).

### Fallback behavior

If a complete response contains no TTS tags, the wrapper does not speak any of it. This keeps technical responses (pure code, file edits, command output) silent.

### Local-LLM TTS reliability

The Claude (local) tab passes the same TTS injection instructions as the subscription Claude tab (Claude Code is identical between the two — only the auth/endpoint env differs). Whether `[[TTS]]…[[/TTS]]` markup actually appears in output depends on the model behind the local proxy. Smaller models (e.g., 7-13B class) often don't follow the markup convention reliably even when instructed; larger models (32B+ or proprietary class) tend to be more compliant. cImp treats missing markup the same way it treats any non-markup output — silently. This is fallback behavior, not an error.

---

## Keyboard Shortcut Handling

The frontend installs a window-level keydown listener (capture phase) that runs *before* xterm.js's own handlers. Configured shortcuts match → handler invokes → propagation stops. Otherwise the event flows through to xterm.js (or wherever it would normally go).

User-configured shortcut strings (e.g., `"Ctrl+Shift+E"`) are parsed into key event predicates at app startup and on settings change. Unrecognized or empty strings disable that action (no shortcut bound).

The full set is in the settings schema above. Behavior of `switch_to_tab_N` is binding to *position*, scoped to the focused pane: `Ctrl+1` switches to the first tab in the focused pane's tab list, `Ctrl+9` to the ninth, no-op when fewer than N tabs in that pane. Closing or moving a tab shifts higher-numbered ones down by one within their pane.

---

## Cross-Platform Considerations

Settings paths on every platform:

- Global baseline: `<exe-dir>/settings.json`
- Per-folder overlay: `<launch_cwd>/.cimp/config.json` (migrated from the legacy loose `<launch_cwd>/.cimp.custom.config.json` on first launch)

These replace the OS-config-dir paths used in earlier versions. The portable design means cImp can be packaged as a self-contained directory, and per-project tweaks live alongside the project rather than in OS-global config.

### Windows

- ConPTY for terminal (handled by `portable-pty`)
- WebView2 as the Tauri webview backend
- CUDA support for `ort` requires CUDA runtime libraries on PATH; cuDNN 9.21 lives at `v9.21\bin\12.9\x64` on the dev box and isn't on PATH by default — needed for the `ort` CUDA EP
- Default shell: Git Bash auto-detected (probes `C:\Program Files\Git\bin\bash.exe`, `C:\Program Files (x86)\Git\bin\bash.exe`, `HKLM\SOFTWARE\GitForWindows\InstallPath`, then `bash.exe` on PATH); falls back to `powershell.exe -NoLogo` with a banner warning in the New Shell Tab dialog

### Linux

- Unix PTY via `forkpty` (handled by `portable-pty`)
- WebKitGTK as the Tauri webview backend
- CUDA support for `ort` requires CUDA runtime
- Default shell: `$SHELL` env var, fallback `/bin/bash`, fallback `/bin/sh`. Invoked with `-i` (interactive); no `--login` to avoid running login scripts that produce output the user didn't expect

### Differences worth knowing

- Webview rendering may differ subtly between WebView2 and WebKitGTK. Test the visualizer, animated WebP, and avatar opacity rendering on both early
- Audio device behavior under PulseAudio vs. PipeWire on Linux may have edge cases
- `<video>` codec coverage on WebKitGTK depends on host GStreamer plugins; prefer WebM/VP9 if MP4 is uncertain
- File path separators: use Rust's `Path` and `PathBuf` consistently; never hardcode `/` or `\`
- Browser-native spell-check dictionaries are provided by the OS / WebView. Available languages depend on system installation

---

## Implementation Conventions

### Error handling

`Result<T, E>` consistently. The top-level `AppError` enum (in `src-tauri/src/error.rs`) is the cross-component error type, converted to at module boundaries. `thiserror` for ergonomic types.

User-visible errors propagate to the State Manager which transitions the affected tab to Error and surfaces a description in the UI. Internal errors that don't affect user experience are logged and otherwise swallowed.

`unwrap()` / `expect()` are restricted to:
- Initialization code where failure means the app cannot start (with informative panic messages)
- Tests
- Cases where the invariant is genuinely guaranteed by surrounding code (with a comment explaining why)

### Module boundaries

Rust code is organized by responsibility, not by technical layer:

```
src-tauri/src/
  audio/         — playback queue, amplitude tap, output stream
  ipc/           — Tauri command handlers, layout commands, tab lifecycle, settings window
  notifications/ — notification queue, dedup-at-play-time, debounce
  processing/    — vte parsing, tag detection, hybrid flush, permission patterns, segmentation
  pty/           — PTY allocation, subprocess lifecycle
  settings/      — schema, migrations, integrity check, broadcaster + debounced save
  shell/         — platform-aware default-shell detection
  state/         — per-tab state machines, signal/event types
  tabs/          — TabRegistry, per-tab metadata
  tts/           — Kokoro engine, voice loading, phonemization, worker
  error.rs       — top-level AppError
  main.rs        — app initialization, task spawning
```

Frontend code mirrors this:

```
src/lib/
  dialog/        — modal dialogs (NewShellTab, ConfigureTab, SaveLayout, ManagePresets)
  dnd/           — drag-and-drop implementation
  layout/        — layout tree, store, persistence, presets, drag drop targets
  settings/      — settings store, IPC wrappers, settings UI components
  shortcuts/     — keyboard dispatcher
  status/        — status-bar buttons (mute / announcements / volume / Layouts popover)
  tabs/          — tabs store, tab state types, tab error state
  ipc.ts         — Tauri command wrappers
  terminals.ts   — xterm.js instance registry, offscreen mounting
  composeState.ts, avatarState.ts, ...
```

### Async style

`async fn` and `.await` consistently. `tokio::spawn` for task creation, `tokio::sync::mpsc` for channels, `tokio::sync::RwLock` for the rare shared state. Avoid `std::sync` primitives in async contexts (the settings handle uses `std::sync::Mutex` because the lock is brief and never crosses an await; everything else uses tokio primitives).

### Logging

`tracing` and `tracing-subscriber`. INFO for major state transitions, DEBUG for component-level events, TRACE for high-volume things. Avoid logging in hot paths at INFO or higher. Logging output respects `RUST_LOG`; `RUST_LOG=perm_capture=debug` exposes permission-detection events for re-characterization when a harness's UI changes.

### Testing

- Unit tests for processing-layer tag detection, hybrid flush, permission patterns
- Unit tests for layout tree operations, validateAndRepairLayout, migration steps
- Unit tests for state machine transitions
- Integration tests for settings load/save round-trips and migration end-to-end
- Manual end-to-end testing for TTS, audio, transitions, drag-drop, splits, presets, notifications, cross-platform validation

### Frontend conventions

- Svelte components in single-file `.svelte` files; one component per UI element
- State management via Svelte stores (writable / derived / readable as appropriate)
- IPC: typed event names, request/response via Tauri commands, fire-and-forget via Tauri events
- No external CSS framework; hand-written CSS scoped to components

### Visual language

`src/theme.css` is the single source of truth for chrome colors, spacing, radii, motion, and typography. Components reference `var(--*)` tokens; never hardcode hex literals in `<style>` blocks. Adding a new color value means adding (or reusing) a token first.

The active theme is selected via `<html data-theme="modern-dark">`, set synchronously at module top of `src/main.ts` and `src/settings_main.ts`. Future themes plug in as additional `[data-theme="..."]` blocks in the same file; the picker in Settings → Appearance writes the chosen theme to `settings.ui.theme`.

Token surface:

- **Surfaces.** Layered slate-blue from `--surface-0` (darkest, body bg) to `--surface-4` (lightest, hover-on-elevated). Sunken variants (`--surface-sunken`, `--surface-deep`, `--surface-input`) are for content inset into a panel — input backgrounds, details summaries.
- **Text.** Five-tier scale: `--text-primary` (default), `--text-bright` (headings), `--text-secondary` / `--text-quiet` / `--text-tertiary` for descending emphasis, `--text-disabled` for inactive.
- **Accent.** Mint/teal `--accent`. Reserved for filter/toggle active states, primary CTAs, drop-zone glows, focus rings. Section selection uses surface elevation, not accent fill — the *two-tier active-state pattern*.
- **Semantics.** `--success` (mint, aliased to `--accent`), `--warning` (amber `#f0a020`), `--danger` (coral `#f06080`). Each has a shade family (e.g. `--surface-danger-bg`, `--text-danger-soft`, `--border-danger`) for banners, error overlays, destructive buttons.
- **Borders.** `--border-subtle` (faint dividers), `--border-default` (input/button borders), `--border-strong` (high-contrast).
- **Radii.** `--radius-sm: 6px` (chips, badges), `--radius-md: 10px` (buttons, inputs, popover items), `--radius-lg: 14px` (dialogs, cards), `--radius-pill: 999px` (toggles, status pills).
- **Elevation.** `--shadow-sm` / `--shadow-md` / `--shadow-lg` for popovers, dialogs, sheets respectively.
- **Motion.** `--motion-fast: 120ms` for color/background hover, `--motion-base: 180ms` for surface/transform changes. `--easing-standard` is the standard cubic-bezier. A `prefers-reduced-motion` media query zeroes both durations.
- **Typography.** `--font-size-xs..lg`, `--font-weight-{regular,medium,semibold}`. The `.tnum` utility class enables tabular numerics for value displays that update frequently.

`src/lib/Pill.svelte` is the reusable tag/badge primitive — supports `default | mint | coral | orange | accent-fill` variants and `xs | sm | md` sizes. Use for kind labels, severity tags, restart-required indicators.

The avatar overlay, waveform visualizer, and xterm.js terminal interior are explicitly *not* themed by this system — those surfaces have their own visual logic (user-supplied images, user-tunable waveform color, xterm.js's own `ITheme` for the per-tab terminal palette).

---

## Glossary

- **PTY** — pseudo-terminal
- **vte** — Rust crate that parses ANSI/VT escape sequences
- **TUI** — text-based user interface
- **Stability window / stability timeout** — duration of inactivity after which a region of terminal output is considered done
- **Hybrid flush** — combined trigger model used by the processing layer (stability timeout / max hold / completed TTS tag)
- **TTS markup / TTS tags** — the `[[TTS]]...[[/TTS]]` convention
- **Avatar state** — one of `{Idle, Listening, Thinking, Speaking, Error}`
- **Transition animation** — the single shared one-shot animation that plays between state changes at runtime
- **Avatar overlay** — the floating, configurable, semi-transparent rendering of the avatar over the terminal
- **Toggle button** — the thin vertical button adjacent to the avatar that hides/shows it
- **Compose overlay / compose sheet** — the bottom-sheet textarea with browser spell-check
- **Amplitude tap** — the mechanism by which the audio playback path exposes recent sample data for the visualizer
- **Tab** — an independently-spawned subprocess with its own PTY, processing layer, and avatar state
- **Harness** — a registered AI coding CLI cImp hosts but does not own: one `HarnessDescriptor` row in `harness/registry.rs` plus one `harness/<id>/` plugin directory. Claude Code and OpenCode ship registered.
- **Harness plugin** — the code half of a harness (`HarnessPlugin`, `harness/<id>/`): everything cImp knows about that CLI — its prompt grammar, its config artifacts, its readers, its affordance strings. The only per-harness artifact in the tree.
- **Tab kind** — discriminator between `AiTool` (a tab running a registered harness; full feature set) and `Shell` (configurable shell; reduced feature set)
- **Builtin tab** — a tab whose id a `HarnessDescriptor` claims in its `tab_ids` (`claude`, `claude-local`, `opencode` today). The integrity check restores the ones listed in `enabled_ai_tabs` at their canonical registry positions and refuses to close them at runtime. `shell-default-1` is *not* a builtin despite its reserved-looking id — it ships only on fresh installs and is closable like any user shell.
- **User tab** — a Shell tab (whether the seeded `shell-default-1` or one created later via the `+` button or `Ctrl+T`). Can be closed, renamed, reconfigured.
- **Closed sub-state** — a Shell-tab UI state when the subprocess has exited; shows a restart message; pressing Enter respawns
- **Tab status indicator** — visual element on the tab bar showing status (working, awaiting permission, error, done while away)
- **DoneWhileAway** — a UI flag set when a tab transitions to Idle while not the focused pane's active tab; cleared when the user focuses that tab
- **Notification** — an audible announcement triggered when a tab's state changes meaningfully and the user is focused elsewhere
- **Per-tab dedup at play-time** — the notification queue retains all entries on enqueue, but at playback filter to keep only the most recent per tab
- **Layout tree** — the binary tree describing pane arrangement; leaves are panes, internal nodes are splits
- **Pane** — a rectangular leaf region of the layout tree containing its own tab bar and one active tab
- **Split** — internal node of the layout tree containing two children, a direction (horizontal | vertical), and a ratio (first child's share of the available space)
- **Splitter** — the draggable line between a split's two children
- **Focused pane** — the single pane currently receiving routing for avatar, audio, compose, and most keyboard shortcuts. Distinct from each pane's own active tab
- **Drop zone** — region of a pane (left/right/top/bottom edges, center, or tab bar) that triggers a specific outcome on drop (split, move, reorder)
- **Ghost tab** — visual element following the cursor during a drag
- **Tab tearing** — dragging a tab out of its pane to create a new pane (split) elsewhere
- **Tree rebalancing** — when a pane is destroyed, its parent split is replaced by the surviving sibling — standard binary-tree-deletion
- **Layout preset** — a named saved layout tree, restorable via the Layouts menu
- **Audio target tab** — the single tab whose audio buffers are allowed to play, set by the frontend on every focus or active-tab change
- **Settings handle** — the Rust-side cheap-to-clone wrapper around the in-memory settings, the broadcast sender, and the save-signal channel

---

## Where to Find More

- **Per-milestone implementation detail**: `MILESTONE-V[N]-[N].md` files. The milestone series numbering is independent of the git tag — each milestone preamble documents which release it shipped under (e.g. V1.4-01..04 → `v1.3.2`, V1.4-07 → `v1.3.3`). Cumulative releases are tagged on the `v0.x.y` line (most recent: `v0.4.0`).
- **Deferred and future work**: `FUTURE-FEATURES.md` — both external dependencies and "we could build this but chose not to" items, with triggers for when to pick them up.
- **Maintenance and operations**: `MAINTENANCE.md` — dependency upgrade notes, model files, runtime-prompt management.
- **Harness surface**: `../src-tauri/src/harness/README.md` — the layering (capabilities → session bus → CHP → plugin → harness), what each file is for, and how to add a harness. Per-harness mechanism detail lives in `../src-tauri/src/harness/claude/README.md` and `../src-tauri/src/harness/opencode/README.md`; every dependency cImp has on a harness is a row in `harness/contract.rs`.
- **Packaging**: `PACKAGING.md` — release-build and distribution notes.

---

## Document Maintenance

This document is updated when:

- An architectural decision changes
- A new component is added or a component's responsibilities materially change
- A scope item moves between in-scope and out-of-scope
- The settings schema changes shape (not just adds optional fields)

It is not updated for:

- Implementation details below the architectural level (those go in code comments and milestone specs)
- Bug fixes
- Minor refactors that don't change responsibilities

When a major version of the architecture is in flight, prefer drafting changes in milestone specs and folding them in here once they ship. The doc reflects the *current* state, not aspirational state.
