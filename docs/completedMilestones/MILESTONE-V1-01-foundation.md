# Milestone 1: Foundation

## Goal

Establish a working Tauri application with an embedded terminal that runs Claude Code interactively in a real PTY, on both Windows and Linux. No TTS, no avatar, no settings — just verify that the terminal embedding works end-to-end.

## Why This Milestone First

The PTY + xterm.js integration is the architectural foundation. If it doesn't work cleanly across platforms, every subsequent milestone is built on sand. Validating it in isolation, before any other complexity is introduced, ensures we're not debugging terminal issues while also building the TTS pipeline.

## Scope

### In Scope

- Tauri 2.x project scaffolded with Svelte frontend
- A single main window containing one terminal pane (no avatar pane yet — full window width is the terminal)
- PTY allocation and management on both Windows (ConPTY) and Linux (Unix PTY) via `portable-pty`
- Spawning `claude` as a subprocess in the PTY, with the working directory set to the directory the wrapper app was launched from
- CLI argument pass-through: any arguments passed to `cctts` are forwarded to the spawned `claude` subprocess. The wrapper itself accepts no CLI arguments of its own in v1 — it is a drop-in replacement for `claude`. Running `cctts --resume <session-id>` invokes `claude --resume <session-id>` inside the PTY.
- Forwarding raw bytes from PTY → frontend → xterm.js for display
- Forwarding keystrokes from xterm.js → frontend → PTY for input
- Handling terminal resize events (window resize → xterm.js resize → PTY resize)
- Clean subprocess shutdown when the app window closes

### Out of Scope (Defer to Later Milestones)

- The processing layer (vte parsing, tag detection, flush logic) — Milestone 2
- Any TTS-related functionality
- Avatar pane and visualizer
- Settings window or settings persistence (Claude Code CLI flags, font size, etc. are hardcoded for now)
- Error states beyond basic logging
- Application packaging/installer

## Acceptance Criteria

The milestone is complete when all of the following are true:

1. Running `cargo tauri dev` (or the equivalent on a packaged build) launches a window containing a terminal
2. The terminal displays Claude Code's TUI exactly as it would appear in a native terminal — colors, the input box, status indicators, the spinner, slash command menus, all rendering correctly
3. Typing into the terminal sends input to Claude Code; Claude Code receives it and responds
4. The user can have a normal interactive Claude Code session inside the embedded terminal — including using slash commands, arrow keys for history, Ctrl+C, and any other terminal interactions Claude Code supports
5. Resizing the application window resizes the terminal, and Claude Code's TUI reflows correctly
6. Closing the window terminates the `claude` subprocess cleanly (no orphaned processes)
7. The above works on both Windows 10/11 (with WebView2) and a modern Linux distribution (with WebKitGTK)
8. The working directory inside Claude Code matches the directory the wrapper app was launched from
9. CLI arguments passed to `cctts` are forwarded to the underlying `claude` subprocess. For example, `cctts --resume <session-id>` is equivalent to running `claude --resume <session-id>` directly. Verify with at least one Claude Code flag (e.g., `--help` or `--resume`) that the argument reaches Claude Code and is interpreted correctly.

## Implementation Approach

### Project Structure

```
.
├── src-tauri/                  # Rust backend
│   ├── Cargo.toml
│   ├── tauri.conf.json
│   ├── build.rs
│   └── src/
│       ├── main.rs             # app entry, Tauri setup
│       ├── pty/
│       │   ├── mod.rs          # PTY manager public API
│       │   └── manager.rs      # PTY lifecycle, byte forwarding
│       ├── ipc/
│       │   ├── mod.rs
│       │   ├── commands.rs     # Tauri command handlers
│       │   └── events.rs       # Tauri event emission helpers
│       └── error.rs            # AppError enum
├── src/                        # Svelte frontend
│   ├── App.svelte              # main layout
│   ├── lib/
│   │   ├── Terminal.svelte     # xterm.js wrapper component
│   │   └── ipc.ts              # typed wrappers for Tauri invoke/listen
│   ├── main.ts
│   └── app.css
├── package.json
├── svelte.config.js
├── vite.config.ts
└── tsconfig.json
```

### Rust Backend

#### Dependencies (Cargo.toml)

Approximate set, pin to current stable versions when implementing:

- `tauri` (2.x)
- `tauri-build` (build dep)
- `portable-pty`
- `tokio` with features: `rt-multi-thread`, `macros`, `sync`, `io-util`
- `serde`, `serde_json` (for IPC payloads)
- `thiserror` (for error types)
- `tracing`, `tracing-subscriber`

#### PTY Manager (`pty/manager.rs`)

Public API roughly:

```
pub struct PtyManager {
    // owns the PTY pair, the child process handle, and a writer for input
}

impl PtyManager {
    pub async fn spawn(working_dir: PathBuf, extra_args: Vec<String>) -> Result<Self, AppError>;
    pub async fn write_input(&self, bytes: &[u8]) -> Result<(), AppError>;
    pub async fn resize(&self, rows: u16, cols: u16) -> Result<(), AppError>;
    pub async fn shutdown(self) -> Result<(), AppError>;
}
```

Internal behavior:

1. On `spawn()`: allocate a PTY pair via `portable_pty::native_pty_system().openpty(...)`, build a `CommandBuilder` for `claude` with `cwd` set to `working_dir` and any extra arguments appended, spawn the child, then start a background tokio task that reads from the master end and forwards bytes to the frontend via Tauri events
2. The byte-forwarding task uses `spawn_blocking` (PTY reads are blocking on most platforms) and emits a `pty-output` event with the byte chunk as a base64 string or as a `Vec<u8>` (Tauri 2 supports binary payloads cleanly)
3. `write_input()` accepts UTF-8 bytes and writes them to the PTY master writer
4. `resize()` calls `portable_pty`'s resize on the master
5. `shutdown()` kills the child process and joins the reader task

Key implementation note: the PTY master in `portable-pty` returns a `Box<dyn MasterPty>`. The reader is `master.try_clone_reader()?` and the writer is `master.take_writer()?`. The reader goes to the blocking task; the writer is held in the manager for `write_input()` calls (wrap in a `Mutex` if needed for Send/Sync).

#### Working Directory and CLI Argument Capture

In `main.rs`, capture the launch directory and CLI arguments **before any other initialization** that might change the current directory or consume args:

```
let launch_cwd = std::env::current_dir()?;
let claude_args: Vec<String> = std::env::args().skip(1).collect();
```

Pass both into the PTY manager when spawning Claude Code. `std::env::args().skip(1)` drops the binary name itself, leaving everything else to be forwarded as-is to `claude`.

Since `cctts` accepts no arguments of its own in v1, every received argument is forwarded blindly. If `cctts`-specific arguments are added in a future version, they would need to be parsed and stripped before the remainder is forwarded — but that's out of scope here.

#### IPC Surface

Tauri commands (frontend → backend):

- `pty_write(input: String)` — forward keystrokes to the PTY
- `pty_resize(rows: u16, cols: u16)` — forward terminal resize

Tauri events (backend → frontend):

- `pty-output` — payload is the byte chunk read from PTY (binary or base64)
- `pty-exit` — payload is the exit code or error description

Define typed TypeScript wrappers in `src/lib/ipc.ts` so the frontend never invokes raw strings.

#### Application Lifecycle

In `main.rs`:

1. Initialize `tracing_subscriber`
2. Capture `launch_cwd`
3. Build the Tauri app with the PTY manager held in app state
4. On the main window's `setup` callback, spawn the PTY (passing `launch_cwd` and empty extra args for now)
5. On the window's close event, call `PtyManager::shutdown()`

### Svelte Frontend

#### Dependencies (package.json)

- `@tauri-apps/api` (2.x)
- `xterm` (the xterm.js core)
- `xterm-addon-fit` (for resize-to-container)
- Svelte and Vite (set up by the Tauri project template)

#### Terminal Component (`Terminal.svelte`)

Approximate structure:

```svelte
<script lang="ts">
  import { onMount, onDestroy } from 'svelte';
  import { Terminal } from 'xterm';
  import { FitAddon } from 'xterm-addon-fit';
  import 'xterm/css/xterm.css';
  import { listen } from '@tauri-apps/api/event';
  import { invoke } from '@tauri-apps/api/core';

  let containerEl: HTMLDivElement;
  let term: Terminal;
  let fitAddon: FitAddon;
  let unlistenOutput: () => void;
  let unlistenExit: () => void;
  let resizeObserver: ResizeObserver;

  onMount(async () => {
    term = new Terminal({
      fontFamily: 'monospace',
      fontSize: 14,
      cursorBlink: true,
      // ... reasonable defaults
    });
    fitAddon = new FitAddon();
    term.loadAddon(fitAddon);
    term.open(containerEl);
    fitAddon.fit();

    // forward keystrokes to backend
    term.onData((data) => {
      invoke('pty_write', { input: data });
    });

    // forward resize to backend (debounced)
    resizeObserver = new ResizeObserver(() => {
      fitAddon.fit();
      const { rows, cols } = term;
      invoke('pty_resize', { rows, cols });
    });
    resizeObserver.observe(containerEl);

    // listen for output
    unlistenOutput = await listen<number[]>('pty-output', (event) => {
      // event.payload is bytes; write to terminal
      term.write(new Uint8Array(event.payload));
    });

    unlistenExit = await listen('pty-exit', () => {
      term.write('\r\n[Process exited]\r\n');
    });
  });

  onDestroy(() => {
    unlistenOutput?.();
    unlistenExit?.();
    resizeObserver?.disconnect();
    term?.dispose();
  });
</script>

<div bind:this={containerEl} class="terminal-container"></div>

<style>
  .terminal-container {
    width: 100%;
    height: 100%;
  }
</style>
```

The exact payload format for `pty-output` depends on Tauri 2's binary event support. Check current Tauri 2 documentation for the most efficient way to ship binary data; falling back to a base64-encoded string is fine if binary events are awkward.

#### App Layout (`App.svelte`)

For this milestone, the entire window is the terminal:

```svelte
<script lang="ts">
  import Terminal from './lib/Terminal.svelte';
</script>

<main>
  <Terminal />
</main>

<style>
  :global(html, body) {
    margin: 0;
    padding: 0;
    height: 100%;
    background: #000;
  }
  main {
    width: 100vw;
    height: 100vh;
  }
</style>
```

In later milestones we'll add the avatar overlay (Milestone 4) and other elements as floating siblings. For now, full-window terminal is correct.

## Validation Steps

Run through these manually before declaring the milestone complete:

1. **Basic launch**: `cargo tauri dev` opens a window with a working terminal showing Claude Code's startup output
2. **Input**: Type a message and submit it to Claude Code; verify Claude responds
3. **Slash commands**: Type `/help` (or any other slash command); verify the menu appears and is navigable
4. **Resize**: Drag the window edges; verify the terminal resizes smoothly without corruption
5. **Special keys**: Verify Ctrl+C, Ctrl+L, arrow keys (history), Tab (completion in shells if applicable), Esc all work
6. **Color rendering**: Verify Claude Code's colored output (syntax highlighting, status indicators) renders correctly
7. **Working directory**: Inside Claude Code, ask it to confirm the working directory; verify it matches where you launched the app
8. **Clean exit**: Close the window; check OS process list to confirm `claude` is not orphaned
9. **CLI argument passthrough**: launch the app with an argument that Claude Code recognizes (e.g., `cctts --resume <some-session-id>` or `cctts --help` if Claude Code supports it). Verify the argument reaches Claude Code (by behavior — the resumed session loads, or `--help` output is rendered in the terminal). Verify launching with no arguments still works as plain `claude` invocation.
10. **Cross-platform**: Repeat all of the above on the second platform (Windows if developed on Linux, or vice versa)

## Known Risks and Mitigation

- **Tauri 2 binary event payloads**: if shipping raw bytes through events is awkward in Tauri 2's current API, fall back to base64 strings. The performance cost is negligible at terminal byte rates.
- **WebView2 vs WebKitGTK rendering differences**: xterm.js is well-tested across browsers, so this should be a non-issue, but verify on both early.
- **PTY resize signal propagation**: SIGWINCH must reach the child process. `portable-pty` handles this, but verify that Claude Code's TUI actually responds to resize (it should — it's a TUI, that's what they do).
- **Working directory edge cases**: on Windows, the directory might be a UNC path or have unusual characters. Test with the launch directory being a path with spaces.

## What "Done" Looks Like

A user can launch the app, have a complete interactive Claude Code session inside it (including all the things they would normally do in a terminal), and close it cleanly. The experience should be indistinguishable from running `claude` in a regular terminal, except that the terminal lives inside an application window.

If the user notices anything different from native Claude Code at this point — input lag, missing key bindings, broken colors, layout glitches — fix those before moving on. The next milestone introduces the processing layer, which will modify the byte stream; we cannot debug processing-layer bugs on top of foundation bugs.

---

## Next Milestone

Milestone 2: Processing Layer. Introduces vte-based parsing, the hybrid flush trigger, and TTS tag detection (with stub TTS extraction that just logs detected content). Terminal continues to display correctly with tags stripped.
