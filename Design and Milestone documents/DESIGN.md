# Design Document: Claude Code Chat Wrapper with TTS

## Purpose of This Document

This document captures the architecture, design decisions, and rationale for a desktop application that wraps Claude Code with a graphical interface, text-to-speech output, and an animated avatar. It is the authoritative reference for implementation. When a milestone specification is unclear or a design choice is not covered, this document is the fallback. When this document conflicts with intuition or convention, this document wins — the decisions here were made deliberately.

The audience is Claude Code itself, working on implementation across multiple sessions, plus any human reviewer. The document explains *why* not just *what*, because future implementation decisions need that context to stay consistent.

---

## Project Overview

### What we are building

A cross-platform desktop application (Windows and Linux) that runs Claude Code interactively inside an embedded terminal, processes its output through a transformation layer, and adds three capabilities Claude Code does not natively have:

1. **Text-to-speech output** for conversational portions of Claude Code's responses, using a local Kokoro TTS engine running in-process.
2. **An animated avatar overlay** with state-driven visuals, a shared transition animation that plays between any state change, and a live audio waveform overlay reactive to TTS playback. The avatar floats over the terminal as a configurable, semi-transparent overlay.
3. **A spell-check compose overlay** — a slide-up bottom sheet with a spell-checking textarea for composing longer messages, complementing the native Claude Code input for short interactions.

Critically, the user retains the full interactive Claude Code experience. This is not a chat client that calls the Claude API — it is a wrapper around the actual `claude` binary running in a real PTY, with all of Claude Code's tools, slash commands, file editing, and TUI behavior preserved.

### What we are NOT building

- A standalone chat application using the Anthropic API directly
- A replacement for Claude Code's TUI
- A coding assistant with custom tool integration
- A multi-user or remote-accessible service
- A mobile application
- A general-purpose terminal emulator (the PTY-based architecture supports this technically, but the project stays focused on the Claude Code use case for v1)

### Primary user

A single technical user running on their main desktop (Windows, RTX 5090) with an option to run on Linux desktops as well. The user prefers terse, dense interactions, has CS background, and is comfortable with terminal-based workflows. The TTS layer is for ergonomic enhancement, not accessibility-required functionality. Spell-check is for ergonomic enhancement of longer-form composition.

---

## Stack

### Backend: Rust

- **Tauri 2.x** as the application shell and IPC layer between Rust and the frontend
- **`portable-pty`** for cross-platform PTY management (ConPTY on Windows, Unix PTY on Linux)
- **`vte`** for ANSI escape sequence parsing and terminal screen state tracking
- **`tokio`** as the async runtime
- **`ort`** (ONNX Runtime bindings) for Kokoro inference, with CUDA execution provider on systems that have it
- **`cpal`** for cross-platform audio output
- **`rodio`** layered on top of `cpal` for audio queue management and playback
- **`serde` / `serde_json`** for settings persistence and IPC payloads

### Frontend: Svelte

- **Svelte** as the component framework (chosen for low overhead, good reactive performance for the visualizer's high-frequency redraws, and modest learning curve)
- **xterm.js** for terminal rendering inside the embedded webview
- **Canvas 2D** for the waveform visualizer (sufficient performance, simpler than WebGL for this use case)
- Native browser `<img>` elements for avatar display (handles PNG, JPG, GIF, and animated WebP without additional libraries)
- Native browser `<textarea>` with `spellcheck="true"` for the compose overlay

### Why this stack

The combination of an embedded interactive Claude Code and a custom GUI with TTS pulls the architecture in specific directions:

- An interactive PTY-based TUI requires a real terminal emulator widget. xterm.js is the most mature option available, and embedding it requires a webview-based shell. Tauri provides this with much lower overhead than Electron.
- Cross-platform desktop deployment with native performance favors Rust over interpreted languages.
- In-process Kokoro inference with GPU acceleration requires good ONNX bindings. Rust's `ort` crate is solid and well-maintained.
- The waveform visualizer benefits from web technology (Canvas) for the rendering side, which Tauri's webview gives us naturally.
- Browser-native spell-check on a textarea is essentially free and gives us standard right-click correction UI, eliminating the need for a custom spell-check engine.

The alternative considered was C# + Avalonia, which was rejected because Avalonia lacks a mature embedded terminal widget. The Python option was rejected at the user's request.

---

## High-Level Architecture

```
┌─────────────────────────────────────────────────────────────────┐
│                      Tauri Application                          │
│                                                                 │
│  ┌──────────────────────────────┐  ┌──────────────────────────┐ │
│  │      Rust Backend            │  │   Svelte Frontend        │ │
│  │                              │  │                          │ │
│  │  ┌─────────┐                 │  │  ┌────────────────────┐  │ │
│  │  │   PTY   │ ─── claude ────▶│  │  │  Terminal          │  │ │
│  │  │ Manager │                 │  │  │  (xterm.js,        │  │ │
│  │  └────┬────┘                 │  │  │   full window)     │  │ │
│  │       │ raw bytes            │  │  └─────────▲──────────┘  │ │
│  │       ▼                      │◀─┼────────────┘ display     │ │
│  │  ┌──────────────────────┐    │  │              bytes       │ │
│  │  │  Processing Layer    │    │  │  ┌────────────────────┐  │ │
│  │  │  (vte + tag parser)  │────┼──┼─▶│  Avatar Overlay    │  │ │
│  │  └────┬─────────┬───────┘    │  │  │  (floating,        │  │ │
│  │       │         │            │  │  │   configurable     │  │ │
│  │       │         │ TTS text   │  │  │   corner + size)   │  │ │
│  │       │         ▼            │  │  └─────────▲──────────┘  │ │
│  │       │    ┌────────────┐    │  │  ┌────────────────────┐  │ │
│  │       │    │  Kokoro    │    │  │  │  Waveform Overlay  │  │ │
│  │       │    │  (ONNX)    │    │  │  │  (sibling of       │  │ │
│  │       │    └─────┬──────┘    │  │  │   avatar overlay,  │  │ │
│  │       │          │ PCM       │  │  │   independent      │  │ │
│  │       │          ▼           │  │  │   opacity)         │  │ │
│  │       │   ┌─────────────┐    │  │  └─────────▲──────────┘  │ │
│  │       │   │ Audio Queue │    │  │            │             │ │
│  │       │   │ (rodio/cpal)│    │  │            │ state +     │ │
│  │       │   └──────┬──────┘    │  │            │ amplitudes  │ │
│  │       │          │ amplitude │  │  ┌─────────┴──────────┐  │ │
│  │       │          │ samples   │  │  │  Compose Sheet     │  │ │
│  │       │          ▼           │  │  │  (slides up from   │  │ │
│  │       │   ┌─────────────┐    │  │  │   bottom of window)│  │ │
│  │       └──▶│   State     │────┼──┼─▶└────────────────────┘  │ │
│  │           │   Manager   │    │  │  ┌────────────────────┐  │ │
│  │           └─────────────┘    │  │  │  Settings Window   │  │ │
│  │                              │  │  │  (separate window) │  │ │
│  │                              │  │  └────────────────────┘  │ │
│  └──────────────────────────────┘  └──────────────────────────┘ │
└─────────────────────────────────────────────────────────────────┘
```

The diagram is approximate. The key shape is: PTY bytes flow into the processing layer, which fans them out to two destinations — clean bytes for the terminal display, and extracted TTS content for the synthesis pipeline. Both paths feed signals back to a state manager that drives the avatar.

---

## Component Design

### PTY Manager

Owns the lifecycle of the `claude` subprocess. Responsible for:

- Spawning `claude` as a subprocess with appropriate environment, working directory (set to the directory the wrapper app was launched from, matching native Claude Code behavior), and any CLI arguments forwarded from the wrapper invocation
- Allocating a PTY pair, attaching `claude` to the slave end
- Reading raw bytes from the master end and forwarding to the processing layer via a channel
- Forwarding user keyboard input from the frontend (received via Tauri IPC) to the master end
- Forwarding composed text from the compose overlay (also via Tauri IPC) to the master end, followed by an Enter to submit
- Handling PTY resize events when the terminal area changes size
- Detecting subprocess exit and reporting it to the state manager

The wrapper acts as a drop-in replacement for the `claude` command. Any CLI arguments passed to `cctts` are captured at startup (via `std::env::args().skip(1)`) and forwarded to the spawned `claude` subprocess. `cctts` itself accepts no CLI arguments of its own in v1; all configuration is in the settings file. This means `cctts --resume <session-id>` is functionally equivalent to running `claude --resume <session-id>` directly, but with the wrapper UI around it.

There are two sources of CLI arguments that get passed to `claude`:

1. **Persistent flags** from settings (`claude_code.extra_cli_flags`): applied on every launch and on every Claude Code restart triggered from settings. Useful for flags the user always wants (e.g., a specific model selection).
2. **Invocation arguments** captured from the `cctts` command line at startup: applied only for the duration of this app session. If the user triggers a Claude Code restart from settings during this session, the invocation arguments are reused (not lost).

The combined argument list is `[persistent flags] ++ [invocation arguments]` — settings flags first, then command-line arguments. If the same flag appears in both, Claude Code's argument parser determines which wins (typically the last occurrence).

Implementation notes:

- Use `portable-pty` for the PTY abstraction. It handles the Windows ConPTY vs. Unix PTY split internally.
- The PTY reader runs as a dedicated tokio task, blocking on reads (PTY reads cannot be made truly non-blocking on all platforms, so use a blocking task pool).
- The working directory and CLI arguments must both be captured from the process environment at app launch, before any initialization that might change them.
- Compose overlay submissions arrive as a single `pty_write` containing the textarea content plus a newline. Claude Code receives this as if pasted and submitted. No special protocol required.

### Processing Layer

The single transformation point between Claude Code's raw output and everything downstream. This is the most architecturally significant component because it is the seam where Claude Code's behavior meets our application's requirements.

Responsibilities:

1. Parse incoming bytes through a `vte` parser to maintain awareness of terminal screen state
2. Detect `[[TTS]]...[[/TTS]]` tags in the rendered text content (across ANSI styling boundaries)
3. Strip TTS tags from the byte stream before forwarding to the terminal display
4. Extract TTS-tagged content and segment it into sentence-bounded chunks for the TTS pipeline
5. Manage flush timing using the hybrid trigger model (see below)
6. Handle in-place rewrites by tracking the logical document state

#### The hybrid flush trigger

Bytes are not forwarded to the terminal immediately. They are held briefly in a buffer and flushed based on three triggers, whichever fires first:

- **Stability timeout (~200ms default, configurable)**: if no new bytes have arrived for a region of output for the timeout duration, that region is flushed. This handles in-place rewrites cleanly — we wait until the screen has stopped changing.
- **Maximum hold time (500ms)**: regardless of incoming activity, no byte is held longer than this. This prevents the bursty rendering that would otherwise occur during sustained streaming. Approximates token-by-token rendering at 2Hz update rate, which feels responsive without being jittery.
- **Completed TTS tag**: when a `[[/TTS]]` is detected, the contained content is extracted and pushed to the TTS queue immediately, even if surrounding terminal output is still being held. This minimizes time-to-first-audio.

The user has explicitly accepted that this introduces 200–500ms of perceptible delay between Claude Code generating output and it appearing in the terminal. This is a deliberate tradeoff for clean tag extraction and reliable rewrite handling.

#### Tag detection across ANSI boundaries

Claude Code styles text with ANSI escape sequences. A TTS tag may have styled content inside it: `[[TTS]]hello \x1b[1mworld\x1b[0m[[/TTS]]`. The detector must:

1. Maintain two synchronized views of the byte stream: the **raw view** (with ANSI codes) and the **rendered view** (text only, ANSI stripped)
2. Scan the rendered view for tag boundaries
3. Map rendered-view positions back to raw-view positions
4. Strip tags from the raw view at the mapped positions, preserving styling on the content between them
5. Extract the rendered (ANSI-stripped) content from inside the tags for TTS

The two-view approach is necessary because we want styled output to reach xterm.js (so the user sees colors and formatting) but we want plain text for TTS synthesis (Kokoro doesn't care about styling).

#### In-place rewrite handling

Claude Code's TUI rewrites lines (the input box redraws as the user types, the spinner animates, status lines update). The processing layer must not double-emit text that gets rewritten.

The vte parser maintains a virtual screen state. We observe this state and only commit text from regions that have stabilized (no changes within the stability window). When a region rewrites, we discard the previous version and only emit the final stable version.

Implementation hint: do not try to reconstruct output from screen state — it loses styling. Instead, use screen state as a *signal* of what regions have stabilized, and forward the original bytes for those regions.

#### Sentence-boundary segmentation for TTS

When a complete `[[TTS]]...[[/TTS]]` block has been extracted, its content is segmented into sentences before being pushed to the TTS queue. Sentence boundaries are detected on `.`, `?`, `!`, and `\n\n`, with simple disambiguation for common false positives (decimal numbers, abbreviations like "Dr.", "e.g.", "etc.").

Each sentence becomes one TTS synthesis request. This gives Kokoro complete sentences for natural prosody while still enabling streaming playback (sentence N+1 can begin synthesizing while sentence N is playing).

If a TTS block contains only a fragment (no sentence-ending punctuation), it is sent to TTS as-is rather than held.

### TTS Engine

In-process Kokoro inference via the `ort` crate.

Responsibilities:

- Load the Kokoro ONNX model at startup
- Initialize CUDA execution provider if available, fall back to CPU
- Accept text segments via a channel from the processing layer
- Synthesize audio (PCM, 24kHz, mono) for each segment
- Push completed audio buffers to the audio playback queue

Implementation notes:

- Kokoro requires phoneme input. The phonemization step must happen before ONNX inference. Investigate which Rust crate is appropriate (existing Rust Kokoro implementations like `kokoros` are reference points). If phonemization in Rust is impractical, this is the one place where falling back to a small Python sidecar process is acceptable — but try the pure-Rust path first.
- Voice embeddings are stored separately from the main model file. The TTS engine needs to load the configured voice's embedding and pass it alongside the text input.
- Synthesis happens on a dedicated tokio task to avoid blocking other work.
- On the 5090, synthesis will be much faster than realtime. There is no need to optimize for synthesis speed; bottleneck is elsewhere.

### Audio Playback

`cpal` for the cross-platform audio output stream, `rodio` for queue management.

Responsibilities:

- Open an output stream on the system default audio device at app launch
- Maintain a queue of synthesized audio buffers
- Play buffers sequentially with no gaps
- Expose amplitude data (recent samples or RMS values) for the visualizer
- Handle volume and mute (mute = drop incoming buffers without playing, not pause)

Implementation notes:

- The output device is queried at app launch via `cpal::default_host().default_output_device()`. If the OS default changes during the session, the app does not follow — this is documented behavior for v1, with a "reconnect audio" feature parked for the future.
- For amplitude data: maintain a small ring buffer (e.g., last 1024 samples) that the visualizer reads via IPC at 60Hz. Computing amplitude in Rust and shipping aggregate values is more efficient than shipping raw samples, but raw samples give the frontend more flexibility for visualizer styles. Start with raw samples; optimize if there's a measurable problem.
- "Interrupt-on-input" behavior (cancel current playback when the user starts typing) requires being able to stop the queue immediately. `rodio`'s `Sink` supports `clear()` for this.

### State Manager

Observes events from the PTY, processing layer, TTS engine, audio playback, and compose overlay to compute the current avatar state and broadcast changes to the frontend.

Avatar states (five total):

- **Idle**: default state, no activity
- **Listening**: user is interacting with input — typing in the terminal, or has content in the compose overlay textarea — and Claude has not started responding
- **Thinking**: Claude is generating output but no TTS audio is currently playing (e.g., during tool calls, before first sentence, between TTS blocks)
- **Speaking**: TTS audio is actively playing
- **Error**: a recoverable or unrecoverable error occurred (subprocess died, TTS failed, audio device unavailable)

State transitions are computed from observed events:

- User keyboard activity in the terminal OR non-empty compose overlay textarea → Listening
- First non-input byte received from PTY after user input → Thinking
- TTS audio buffer starts playing → Speaking
- TTS queue empty + Claude still generating → Thinking
- TTS queue empty + Claude response complete (idle PTY for stability window) → Idle
- Subprocess exit, TTS error, audio device error → Error
- After error acknowledgment or recovery → Idle

The state manager broadcasts state changes to the frontend via Tauri events. The frontend handles the visual transition and image swap. If the user has only configured one image (no per-state images), the visual is the same across states but the underlying state still drives behavior like waveform visibility.

The state manager itself is unaware of transition animations, the avatar's visual appearance, or compose overlay UI — those are entirely frontend concerns. The backend just emits state changes; the frontend chooses how to render the change visually.

### Avatar Overlay (Frontend)

The avatar is a floating overlay on top of the full-window terminal, configurable in size, position, and opacity.

#### Layout

- The terminal occupies the full window (no separate avatar pane).
- The avatar overlay is positioned at one of four configurable corners (top-right default, also top-left, bottom-right, bottom-left) with a configurable margin from the corner edges.
- The overlay has a configurable width and height (independent dimensions, default 400×400).
- Adjacent to the avatar (on the side facing the screen edge — right side for right-positioned avatar, left side for left-positioned) is a thin vertical button (the "toggle button") spanning the full vertical extent of the avatar. Clicking it hides the avatar; clicking again shows it. The toggle button remains visible even when the avatar is hidden, so the user can re-show it.
- The waveform visualizer renders in the bottom band of the avatar's area but is structured as a sibling of the avatar (not a child) so that the avatar's opacity does not propagate to the waveform.

#### State images

Each of the five states has a configured image asset (any of PNG, JPG, GIF, or animated WebP). Animated formats loop while the state is active. If a state has no image configured, fall back to the Idle image. Source images with alpha channels (transparent backgrounds) render with their transparency intact, allowing the terminal to show through wherever the artwork doesn't reach.

Image fitting strategy: **contain** (letterbox if needed). The image scales to fit within the configured area while preserving its aspect ratio. If the configured dimensions don't match the image's aspect ratio, empty space appears around the image. Because the avatar overlay is on a transparent base, this empty space simply shows the terminal underneath.

#### Shared transition animation

A single transition asset (with its own duration) is configured globally for the application. It is not per-state — the same asset plays between any state change.

Behavior rules:

- **Plays on every state change at runtime**: when the avatar transitions from any state to any other state, the transition asset is rendered for its configured duration before the new state's looping image takes over. This includes transitions back into Idle.
- **Does not play at app launch**: when the app first opens, it shows the Idle state image directly with no transition.
- **Optional**: if no transition asset is configured (or the path is null/empty), all state changes snap directly to the new state image with no intermediate effect.
- **Duration-based completion**: the transition has a configured duration in milliseconds; after that duration elapses, the transition asset is replaced by the looping state image.
- **Interruption**: if a state change occurs while the transition is in progress, the in-progress transition stops immediately and a fresh transition begins for the new state change. Transitions are never queued.
- **Visualizer is independent**: the waveform visualizer follows the Speaking state directly. It does not hide during transitions.

#### Opacity

The avatar overlay has a configurable global opacity (default 80%, range 30%–100%). This applies to the avatar image and the toggle button uniformly via standard CSS opacity inheritance. It composes multiplicatively with any alpha channel in the source image — a 50%-alpha image at 80% global opacity renders at 40% effective opacity.

The waveform's opacity is **independent** of the avatar overlay's opacity. The waveform is a sibling element, not a child, with its own opacity setting. This is intentional: the waveform is always rendered at its own configured opacity regardless of how transparent the avatar is.

#### Visibility persistence

The avatar's visible/hidden state is persisted in settings and restored across app restarts. If the user hides the avatar and quits the app, the next launch starts with the avatar hidden.

#### Settings access

A gear icon button on the avatar (top-right corner of the avatar regardless of avatar position) opens the settings window. When the avatar is hidden, the gear button is hidden along with it; in that case the configurable `open_settings` keyboard shortcut is the only way to access settings.

### Compose Overlay (Frontend)

A spell-checking textarea for composing longer messages, complementing the native Claude Code input.

#### Layout

- Slides up from the bottom of the application window when triggered.
- Spans the full window width.
- Has an auto-grow height: starts compact (e.g., 3 lines / ~80px) and grows to a configured maximum (e.g., 10 lines / ~300px) as content is added. After the max, the textarea scrolls internally.
- Visually distinct from the terminal (different background tone, top border, slight elevation shadow) so it's clear this is a different input mode.

#### Behavior

- Triggered by a configurable keyboard shortcut (`open_compose`, default `Ctrl+Shift+E`). Has no effect if the sheet is already open (no toggle).
- Receives focus on open. The user types a message with browser-native spell-check active (red squiggles, right-click corrections).
- The terminal underneath remains fully interactive — the user can click into the terminal, select and copy text, or even type directly into Claude Code while the compose sheet is open.
- A configurable shortcut (`submit_compose`, default `Ctrl+Enter`) submits the textarea content. The text is sent to the PTY as-is, followed by a newline (which Claude Code interprets as Enter to submit). The sheet then closes.
  - This is "append mode": the wrapper does not clear or modify Claude Code's existing input line. Composed text is appended to whatever was already there. If the user has stale input in Claude Code's box, they should clear it before submitting the compose sheet.
- A configurable shortcut (`cancel_compose`, default `Escape`) closes the sheet without submitting. The draft is discarded.
- Submit shortcut only fires when the textarea has focus (not when focus is in the terminal).
- Cancel shortcut fires globally while the sheet is open, regardless of where focus currently is.

#### Avatar interaction

While the compose sheet is open and contains non-empty text, the state manager treats this as user input activity (transitions to Listening). Empty textarea = no Listening signal from the compose sheet (although terminal keystrokes can still trigger Listening separately).

### Settings Store

JSON file in the OS config directory:

- Windows: `%APPDATA%\<app-name>\config.json`
- Linux: `~/.config/<app-name>/config.json`

Use Tauri's path API to resolve the directory. Use `serde` for serialization.

Settings are loaded at app launch and held in memory. Changes from the settings UI:

1. Update the in-memory settings struct
2. Write the JSON file (debounced, e.g., 500ms after last change)
3. Broadcast a settings-changed event to all subscribers

Components subscribe to specific slices of settings they care about.

#### Settings schema

```
{
  "tts": {
    "voice": "af_bella",
    "speed": 1.0,
    "volume": 1.0,
    "muted": false
  },
  "segmentation": {
    "boundary_mode": "sentence",
    "min_chunk_length": 0
  },
  "avatar": {
    "visible": true,
    "size": {
      "width_px": 400,
      "height_px": 400
    },
    "position": "top-right",
    "margin_px": 16,
    "opacity": 0.8,
    "images": {
      "idle":      "/path/to/idle.gif",
      "listening": "/path/to/listening.png",
      "thinking":  "/path/to/thinking.gif",
      "speaking":  "/path/to/speaking.gif",
      "error":     "/path/to/error.png"
    },
    "transition": {
      "path": "/path/to/transition.gif",
      "duration_ms": 400
    },
    "waveform": {
      "color": "#00ff88",
      "line_width": 2,
      "glow_intensity": 0.6,
      "opacity": 0.85
    }
  },
  "display": {
    "terminal_font_family": "monospace",
    "terminal_font_size": 14,
    "theme": "dark",
    "tts_markup_visibility": "hidden"
  },
  "behavior": {
    "interrupt_on_input": true,
    "auto_speak": true,
    "fallback_silent": true
  },
  "compose": {
    "min_height_px": 80,
    "max_height_px": 300
  },
  "shortcuts": {
    "open_compose": "Ctrl+Shift+E",
    "submit_compose": "Ctrl+Enter",
    "cancel_compose": "Escape",
    "open_settings": "Ctrl+,"
  },
  "claude_code": {
    "extra_cli_flags": [],
    "claude_md_override": null
  },
  "processing": {
    "stability_timeout_ms": 200,
    "max_hold_ms": 500
  }
}
```

Notes:

- `avatar.visible` is the persisted visibility toggle.
- `avatar.position` accepts `"top-right"`, `"top-left"`, `"bottom-right"`, or `"bottom-left"`.
- `avatar.opacity` ranges 0.3–1.0 (UI enforces; values outside this range silently clamped).
- `avatar.transition.path` set to null/empty means no transition asset configured.
- `compose.min_height_px` and `max_height_px` bound the auto-grow textarea.
- `shortcuts.*` values use a `Modifier+Modifier+Key` string format. The frontend parses these into key event predicates.
- The previous `display.pane_split_ratio` setting has been removed; there is no longer a two-pane layout.

This schema is the source of truth.

---

## Concurrency Model

The Rust backend uses tokio with multiple cooperating tasks coordinated via channels:

- **PTY reader task**: blocking reads from PTY master, sends bytes to processing layer via `mpsc::channel`
- **Processing task**: consumes bytes, runs vte parser, manages stability buffers, emits processed events
- **TTS synthesis task**: receives text, runs ONNX inference, sends PCM buffers to audio queue
- **Audio playback task**: managed by `rodio::Sink`, plays buffers sequentially, exposes amplitude data
- **Amplitude sampler task**: at 60Hz, reads recent amplitude data and sends to frontend via Tauri event
- **State manager task**: receives signals from PTY, processing, TTS, audio, and compose overlay; computes state transitions; broadcasts state to frontend
- **Settings watcher task**: receives settings changes from frontend, updates in-memory state, debounces persistence to disk, broadcasts to subscribers

No shared mutable state except where unavoidable (the settings struct, behind a `RwLock`). All inter-task communication is via channels.

---

## TTS Markup Convention

The convention `[[TTS]]...[[/TTS]]` was chosen over markdown-based markers (italics, blockquotes) because:

- It is unambiguous and has no collision with normal markdown usage
- Claude has explicit control over what is spoken vs. what is displayed
- The processing layer can strip tags cleanly so the user never sees them
- Markdown-based markers force a constant negotiation between "this italic is for emphasis" vs "this italic should be spoken," which constrains Claude's natural use of formatting

The convention is communicated to Claude via a `CLAUDE.md` file shipped with the application or installed in the user's global Claude Code config.

### Fallback behavior

If a complete Claude response contains no TTS tags, the wrapper does not speak any of it. This keeps technical responses (pure code, file edits, command output) silent.

---

## Keyboard Shortcut Handling

Several application-level keyboard shortcuts must be intercepted before xterm.js processes them, since xterm.js is greedy about forwarding keys to the PTY.

The frontend installs a window-level keydown listener (in capture phase) that runs before xterm.js's own handlers. When a configured shortcut matches, the listener invokes the corresponding action and stops propagation. When no shortcut matches, the event is allowed to flow through to xterm.js (or wherever it would normally go).

Shortcuts in v1:

- `open_compose` — opens the compose sheet
- `submit_compose` — submits the compose sheet (only when textarea has focus)
- `cancel_compose` — closes the compose sheet without submitting (fires globally while sheet is open)
- `open_settings` — opens the settings window

User-configured shortcut strings are parsed into key event predicates at app startup and on settings change. Unrecognized or empty shortcut values disable that action (no shortcut bound).

---

## What's Out of Scope for v1

Items raised during design that are deliberately deferred:

- **"Read everything" override**: per-message instruction to read all output verbatim regardless of TTS tags
- **Audio device selection in settings**: v1 uses system default only; switching devices requires app restart
- **"Reconnect audio" button**: rebuild the audio stream without restarting the app
- **Conversation/session UI**: list past sessions, resume specific sessions — Claude Code's native behavior handles all of this
- **Voice mixing / blending**: Kokoro supports it, settings UI does not expose it
- **STT (speech-to-text) input**: only output is in scope
- **Multiple concurrent Claude Code sessions**: one PTY, one Claude instance per app launch
- **Mobile or web deployment**: desktop only
- **Conversation logging or transcript export**: rely on Claude Code's native session storage
- **Theming beyond color customization for waveform and basic terminal colors**
- **Per-state transition animations**: only a single shared transition asset is supported
- **Transition animation at app launch**: the avatar opens directly into the Idle state image with no transition
- **General-purpose terminal emulator usage**: while the architecture would support it, the application stays focused on the Claude Code use case for v1
- **Avatar position beyond four corners**: no free positioning, no center placement
- **Image fitting modes other than "contain"**: no stretch, no cover/crop
- **Replace mode for compose submissions**: composed text appends to existing input rather than replacing
- **Compose draft preservation across cancellations**: cancel always discards
- **Toggle-avatar keyboard shortcut**: visibility is via the toggle button only
- **Configurable shortcut for any action other than the four listed above**

These are valid future enhancements. Do not implement them in v1.

---

## Implementation Conventions

### Error handling

Use `Result<T, E>` consistently. Define a top-level error enum (e.g., `AppError`) for cross-component errors and convert to it at module boundaries. Use `thiserror` for ergonomic error types.

User-visible errors propagate to the State Manager which transitions to the Error state and surfaces a description in the UI. Internal errors that don't affect user experience are logged and otherwise swallowed.

Do not use `unwrap()` or `expect()` outside of:
- Initialization code where failure means the app cannot start (and the panic message is informative)
- Tests
- Cases where the invariant is genuinely guaranteed by surrounding code (with a comment explaining why)

### Module boundaries

Organize Rust code into modules by responsibility, not by technical layer:

```
src/
  pty/          — PTY management, subprocess lifecycle
  processing/   — vte parsing, tag detection, flush logic
  tts/          — Kokoro ONNX wrapper, voice management
  audio/        — playback queue, amplitude tap
  state/        — state manager, transitions
  settings/     — settings schema, persistence, change broadcast
  ipc/          — Tauri command handlers and event emission
  error.rs      — top-level error type
  main.rs       — app initialization, task spawning
```

Each module exposes a small public API and keeps internals private.

### Async style

Use `async fn` and `.await` consistently. Use `tokio::spawn` for task creation, `tokio::sync::mpsc` for channels, `tokio::sync::RwLock` for the rare shared state. Avoid `std::sync` primitives in async contexts.

### Logging

Use `tracing` and `tracing-subscriber`. Log at INFO for major state transitions, DEBUG for component-level events, TRACE for high-volume things. Avoid logging in hot paths at INFO or higher.

### Testing approach

- Unit tests for the processing layer's tag detection and flush logic
- Integration tests for the PTY layer
- Integration tests for settings load/save roundtrips
- Manual end-to-end testing for TTS, audio, transitions, visualizer, and compose overlay

### Frontend conventions

- Svelte components in single-file `.svelte` files
- One component per UI element
- State management via Svelte stores
- IPC: typed event names, request/response via Tauri commands, fire-and-forget via Tauri events
- No external CSS framework; hand-written CSS scoped to components

---

## Cross-Platform Considerations

### Windows

- ConPTY for terminal (handled by `portable-pty`)
- Settings path: `%APPDATA%\<app>\config.json`
- WebView2 as the Tauri webview backend
- CUDA support for `ort` requires CUDA runtime libraries on PATH

### Linux

- Unix PTY via `forkpty` (handled by `portable-pty`)
- Settings path: `~/.config/<app>/config.json`
- WebKitGTK as the Tauri webview backend
- CUDA support for `ort` requires CUDA runtime

### Differences worth noting

- WebView rendering may differ subtly between WebView2 and WebKitGTK. Test the visualizer, animated WebP transitions, and avatar opacity rendering on both early.
- Audio device behavior under PulseAudio vs. PipeWire on Linux may have edge cases.
- File path separators: use Rust's `Path` and `PathBuf` consistently; never hardcode `/` or `\`.
- Browser-native spell-check dictionaries are provided by the OS / WebView. Available languages depend on system installation. The textarea uses whatever the WebView's default is.

---

## Glossary

- **PTY**: pseudo-terminal
- **vte**: a Rust crate that parses ANSI/VT escape sequences
- **TUI**: text-based user interface
- **Stability window / stability timeout**: the duration of inactivity after which a region of terminal output is considered done
- **Hybrid flush**: the combined trigger model used by the processing layer
- **TTS markup / TTS tags**: the `[[TTS]]...[[/TTS]]` convention
- **Avatar state**: one of {idle, listening, thinking, speaking, error}
- **Transition animation**: the single shared one-shot animation that plays between state changes at runtime
- **Avatar overlay**: the floating, configurable, semi-transparent rendering of the avatar over the terminal
- **Toggle button**: the thin vertical button adjacent to the avatar that hides/shows it
- **Compose overlay / compose sheet**: the bottom-sheet textarea with browser spell-check for composing longer messages
- **Amplitude tap**: the mechanism by which the audio playback path exposes recent sample data for the visualizer

---

## Implementation Phasing (Reference Only)

Detailed milestone specifications are separate documents. The expected phasing is:

1. **Foundation**: Tauri scaffold + xterm.js + PTY running Claude Code end-to-end. No avatar overlay, no TTS, no settings. Verifies the terminal embedding works on both target platforms.
2. **Processing layer**: vte integration, hybrid flush, tag detection and stripping. TTS extraction goes to a stub that just logs the extracted content.
3. **TTS pipeline**: Kokoro integration, audio playback. TTS extracted in step 2 now actually speaks.
4. **Avatar overlay**: floating overlay, state machine, image rendering, shared transition, toggle button, opacity, position. No waveform yet.
5. **Visualizer**: waveform overlay reactive to audio playback, rendered as a sibling of the avatar overlay with independent opacity.
6. **Settings**: full settings window with all live-updatable controls (including position, size, opacity, visibility persistence, shortcuts). Wire up everything that was previously hardcoded.
7. **Compose overlay**: spell-checking bottom sheet with configured shortcuts. Submits to the PTY in append mode.
8. **Polish**: error states, edge cases, cross-platform validation, performance tuning if needed.

Each milestone produces a working app at its level of completeness. Milestones are sequential; do not interleave.

---

## Document Maintenance

This document should be updated when:

- A design decision changes (record the change and the reason)
- A scope item moves between in-scope and out-of-scope
- A component's responsibilities materially change
- A new component is added

Do not update this document for:

- Implementation details below the architectural level (those go in code comments and milestone specs)
- Bug fixes
- Minor refactors that don't change responsibilities
