# Design Document: cctts v3

## Purpose of This Document

This document captures the architecture and design decisions for cctts v3 — the user-managed-tabs evolution of the v2 design. It supersedes `DESIGN-V2.md` (v2) as the current architectural truth. The v2 document remains as a historical record of the v2 architecture as it existed at v1.1 ship.

When this document conflicts with `DESIGN-V2.md`, this document wins. Where v2 design elements are unchanged in v3, this document references them rather than restating them — read both together for the complete picture.

The product version that ships from this design is **v1.2**. The design-document version (the "v3" in the filename) is independent of the product version, following the same convention v2 used.

The audience is Claude Code working on v1.2 implementation, plus any human reviewer.

---

## What v1.2 Adds

v1.1 shipped two hardcoded AI-tool tabs (Claude Code, aider) sharing the multi-tab foundation: per-tab PTY, processing layer, state machine, permission detection (Claude Code), notifications, and a bottom status bar. v1.2 generalizes the tab system so the user can run plain shell sessions alongside the AI tabs and add as many of those shell tabs as they want.

Specific additions:

1. **Tab kind abstraction**. Tabs are no longer all the same shape. Two kinds: `AiTool` (the existing Claude / aider behavior) and `Shell` (new — runs a configurable shell, no TTS, no permission detection). The state machine and processing layer behavior is gated on the kind.
2. **Tab 3 ships as the first Shell tab** with a sensible default shell per platform.
3. **User-managed tabs**. A `+` button on the tab bar opens a "new shell tab" flow that creates a new Shell tab with user-configurable name, shell command, arguments, and working directory. There is no upper bound enforced beyond practicality (and `switch_to_tab_N` shortcuts up to N=9).
4. **Tab close and rename**. Right-click on any user tab gives close / rename / configure. The two builtin tabs (Claude, aider) cannot be closed in v1.2 — their close affordance is hidden or disabled.
5. **Tab persistence**. The set of user-created Shell tabs, their configuration, and their order persists across app launches via the settings store. The last-active tab is restored on launch.
6. **Configurable shell selection** with platform-aware defaults. On Linux: `$SHELL` (typically bash/zsh). On Windows: Git Bash auto-detected at standard locations, with documented fallbacks. The user can override per tab to point at WSL, MSYS2, PowerShell, cmd, or any other executable.
7. **Shell-tab-appropriate avatar and notifications**. Shell tabs participate in the avatar overlay (only `Idle` and `Error` states are reachable — no Listening/Thinking/Speaking) and the notification queue (only `error` and `exited` events fire — no `idle`, no `awaiting_permission`). TTS synthesis is fully bypassed for Shell tabs.
8. **Subprocess exit handling for Shell tabs**. When a user's shell exits, the tab enters a `Closed` UI state showing a "Shell exited (code N) — press Enter to restart, or close this tab" message inline in the terminal area. Restart spawns a fresh shell with the same configuration.
9. **Tab-management keyboard shortcuts**. `Ctrl+T` to create a new shell tab, `Ctrl+W` to close the current tab (no-op on builtins), and `Ctrl+1`..`Ctrl+9` to switch to tabs by ordinal position (extended from v1.1's `Ctrl+1`/`Ctrl+2`).

## What v1.2 Does NOT Change

The following v1.1 components are unchanged in v1.2:

- The PTY-based architecture for embedding interactive subprocesses (v1)
- The processing layer's vte-based parsing, hybrid flush trigger, and TTS tag detection (v1) — but TTS extraction is now skipped entirely for Shell tabs (see below)
- The TTS pipeline (Kokoro via ONNX Runtime, sentence-boundary segmentation, audio queue via cpal+rodio) (v1)
- The avatar overlay and waveform visualizer (v1)
- The compose overlay (v1) — applies to Shell tabs the same way it applies to AI tabs: text is sent verbatim to whichever tab is active
- The settings store mechanism (JSON persistence, debounced save, broadcast on change) (v1) — only the schema content changes
- The notification queue, per-tab dedup at play-time, and idle-suppression-while-awaiting-permission edge cases (v1.1 / V2-04)
- The bottom status bar layout and controls (v1.1)
- The cross-platform stack (Rust+Tauri+Svelte, Windows+Linux)

Refer to `DESIGN-V2.md` and the archived v1 `DESIGN.md` for details on these.

---

## Terminal Emulator Selection

A note on terminology before the rationale: cctts uses **xterm.js** as its terminal emulator. That has not changed and is not configurable. What v1.2 makes configurable is the **shell** — the executable spawned inside the PTY whose output xterm.js renders. The user's request to "select a terminal emulator" is interpreted as selecting the shell. Throughout this document, "shell" and "shell command" refer to the configurable subprocess, "terminal" or "terminal area" refer to xterm.js.

### Default shell, per platform

| Platform | Default | Rationale |
|----------|---------|-----------|
| Linux    | `$SHELL` env var, fallback `/bin/bash`, fallback `/bin/sh` | Matches user expectation; works out of the box |
| Windows  | Git Bash (auto-detected), fallback PowerShell with banner warning | Git Bash is bundled with Git for Windows (near-universal on dev machines) and ships with the standard Linux toolset the user wants |

Linux defaults are uncontroversial. Windows is the interesting one.

### Why Git Bash on Windows

The user's stated requirements: run bash scripts and Linux tools (grep, cat, nano, history) on Windows. The candidate options:

- **Git Bash (MSYS2-based, bundled with Git for Windows)** — ships with bash, grep, cat, sed, awk, find, less, vim, nano, tar, ssh, curl, and most of the GNU coreutils. Path translation has known quirks (the `/path` / `MSYS_NO_PATHCONV` issue) but they are well-documented and workable. Critically, Git for Windows is installed on essentially every developer's Windows machine already, so the default-found rate is very high. Works cleanly with portable-pty via ConPTY.
- **WSL (Windows Subsystem for Linux)** — full Linux userland, highest tool compatibility. But: requires a separate install and a configured distro; filesystem performance is poor when working in Windows-mounted directories (which is the typical cctts case, since cctts is launched from a Windows project directory); spawning `wsl.exe` for every shell session has noticeable startup cost. Wrong default; right *option* for users who want it.
- **MSYS2 (full installation)** — more complete than Git Bash, includes pacman for installing arbitrary GNU tools. Extra install burden. Good as a configurable alternative for power users.
- **busybox-w32** — single-binary, lightweight, but the busybox subset is too narrow. No nano, no less, no full vim. Loses too much of what the user asked for.
- **Cygwin** — historically capable but the install experience and integration are dated. Not recommended.

So: Git Bash is the default, with WSL/MSYS2/PowerShell/cmd available via configuration.

### Auto-detection on Windows

Detection runs once at app launch and the result is cached in memory (not in settings — re-detected each launch so a Git install/uninstall is reflected). Detection probes, in order:

1. `C:\Program Files\Git\bin\bash.exe`
2. `C:\Program Files (x86)\Git\bin\bash.exe`
3. The `InstallPath` value under `HKLM\SOFTWARE\GitForWindows` (registry), suffixed with `\bin\bash.exe`
4. Any `bash.exe` resolvable on `PATH`

If none are found, the default for new shell tabs falls back to `powershell.exe`. When this fallback applies, the new-tab dialog displays a small banner: "Git Bash not detected. Defaulting to PowerShell. Linux tools (grep, cat, nano) will not be available. Install Git for Windows to enable Git Bash by default, or set a custom shell below."

### Shell argument and environment defaults

Per-shell spawn defaults (applied unless the user overrides):

- **Git Bash**: invoked as `bash.exe --login -i`. The `--login` flag picks up `~/.bash_profile` and the standard MSYS2 environment, making `/usr/bin/*` tools available without further setup.
- **`$SHELL` on Linux**: invoked as the shell binary with `-i` (interactive). No `--login` to avoid running login scripts that may produce output the user didn't expect inside an embedded session — most users have their interactive setup in `~/.bashrc` / `~/.zshrc` already.
- **PowerShell fallback**: invoked as `powershell.exe -NoLogo`. The user gets PowerShell's standard prompt with no startup banner.
- **WSL**: when the user configures a tab with `wsl.exe`, no default args are added — the user can specify a distro via `-d <name>` themselves.

The working directory defaults to cctts's launch directory (same as Claude Code and aider already do per v1).

### What's not configurable

Per shell tab, the user can configure: command, args, working directory, name, environment additions/overrides (deferred — see Out of Scope). What they cannot configure: the terminal renderer (xterm.js), the PTY backend (portable-pty), the TUI rendering quality, font (currently global to the app — per-tab fonts are out of scope).

---

## Architecture Changes

### Tab Kind Abstraction

In v1.1, the `TabState` struct in the state manager is a single shape used identically for the Claude and aider tabs. v1.2 introduces a `TabKind` enum that distinguishes AI-tool tabs from shell tabs, and gates kind-specific behavior in the state machine, the processing layer, and the notification system.

```rust
pub enum TabKind {
    AiTool(AiToolKind),
    Shell,
}

pub enum AiToolKind {
    ClaudeCode,
    Aider,
    // future: more AI tools, each with their own permission detection patterns and TTS injection
}

pub struct TabState {
    id: TabId,
    kind: TabKind,
    name: String,                // user-editable display name; defaults from kind
    avatar_state: AvatarState,
    awaiting_permission: bool,   // always false for Shell kind
    done_while_away: bool,
    claude_still_generating: bool,  // only meaningful for AiTool kind
    closed: bool,                // Shell-only: subprocess has exited, awaiting restart
    closed_exit_code: Option<i32>,
}
```

Behavior gated on kind:

| Behavior                          | AiTool | Shell |
|-----------------------------------|--------|-------|
| TTS markup detection / extraction | yes    | **skipped** in processing layer |
| Permission prompt detection       | yes (Claude only in v1.1; aider deferred) | no |
| Avatar states reachable           | Idle, Listening, Thinking, Speaking, Error | Idle, Error only |
| Notifications: `idle`             | yes    | no (would fire constantly for an interactive shell) |
| Notifications: `awaiting_permission` | yes (Claude) | no |
| Notifications: `error`            | yes    | yes |
| Notifications: `exited`           | no (AI tools don't typically exit cleanly) | yes (shell exited) |
| Compose overlay submission        | yes    | yes (just writes to PTY like in AI tabs) |
| Subprocess restart on exit        | not in v1.2 | yes (manual, via Enter key on the closed-tab message) |

The `claude_still_generating` flag from v1 is meaningful only for AI tabs; it stays in the struct but is never set for Shell tabs.

The avatar state machine logic is unchanged for AI tabs. For Shell tabs, the only signals the state machine processes are `SubprocessExited` (transitions to a special closed sub-state — see below) and `AudioError` / `TtsError` (which can't actually happen because TTS is bypassed, but the error path stays generic). User input and output do not transition Shell-tab avatar state, so a Shell tab is in `Idle` whenever it's running and `Error` only on a hard failure.

### Processing Layer Changes

The processing layer for a Shell tab still runs vte parsing (so xterm.js receives correctly-rendered bytes and the layer maintains the screen state for any future use), and still runs the hybrid flush trigger (so the rendering pace is consistent with AI tabs). What it skips:

- TTS tag detection
- The two-view (raw + rendered) split needed for tag-stripping (no tags to strip)
- Sentence-boundary segmentation
- The TTS text queue per tab (Shell tabs have none)

This is a pure behavior gate at processing-layer init time: when a tab is constructed with `TabKind::Shell`, its processing layer instance has the TTS path stubbed out. No conditional at runtime per-byte; the construction picks the right pipeline shape once.

### Per-Tab PTY (Generalized)

v1.1 already has per-tab PTYs. v1.2 generalizes the spawn so the command, args, env additions, and working directory all come from the tab's configuration rather than being hardcoded to `claude` / `aider`. The eager-spawn-at-launch policy from v1.1 is retained: all tabs (builtin and user-defined) spawn their subprocesses at app launch, including persisted user-created Shell tabs.

A consequence: if the user has 8 persisted Shell tabs, app launch spawns 10 subprocesses (Claude, aider, plus 8 shells). For typical use this is fine — shells are cheap. If startup cost becomes a concern at higher tab counts, lazy-spawn-on-first-activation is a known fallback (already noted in v2 as a future option).

### Subprocess Exit Handling for Shell Tabs

When a Shell tab's subprocess exits — whether the user typed `exit`, the shell crashed, or the shell was killed externally — the tab transitions to a `Closed` UI sub-state. This is **not** a new avatar state; it is a tab-level UI flag (`closed: bool` plus `closed_exit_code: Option<i32>` in TabState). The avatar state goes to `Idle` (or stays at `Idle`) for closed tabs.

UI rendering for a closed Shell tab:

- The xterm.js instance is replaced (or overlaid) with a centered message: `Shell exited (code 0). Press Enter to restart, or close this tab.` The exit code is shown verbatim; non-zero codes display in the error color.
- Pressing Enter while the tab is active and closed re-spawns the configured shell with the same command/args/cwd/env, clears the closed flag, and reactivates xterm.js for the new PTY.
- The tab status indicator shows the `error` color when `closed_exit_code != Some(0)` (implies a crash); shows the default tab styling for clean exits (`Some(0)`); no separate "exited" indicator color in v1.2.
- An `exited` notification fires when the user is on a different tab. Notification text is configurable per tab. The exit code is interpolated via a `{code}` placeholder if present in the configured text.

The notification is triggered exactly once per exit event, not on every render of the closed message.

A subprocess exit on an AI tab (Claude or aider) is still handled the existing v1 way — surfaces an error state, no auto-restart UI in v1.2. AI-tab restart on exit is out of scope.

### State Manager Changes

The v2 `StateManager` already tracks per-tab state. v1.2 changes:

- `TabState` gains `kind`, `name`, `closed`, `closed_exit_code` (see above).
- `tabs` is now an *ordered* collection (`Vec<TabState>` or an `IndexMap` keyed by `TabId`), not a `HashMap`, because tab order is user-visible and persisted.
- A new signal, `SubprocessExited { tab: TabId, code: Option<i32> }`, was already present in v1's signal set but in v1.2 its handling is split: AI tabs route to Error; Shell tabs route to the closed sub-state.
- A new event type `TabCreated { tab: TabId, kind: TabKind, name: String, position: usize }` is broadcast when a user creates a tab. Frontend subscribes and updates the tab bar.
- A new event type `TabClosed { tab: TabId }` is broadcast when a user closes a tab. Frontend removes it from the tab bar; backend stops the PTY reader and processing tasks for that tab.
- A new event type `TabRenamed { tab: TabId, name: String }` is broadcast when a user renames a tab.

### Tab Lifecycle Operations

New backend operations exposed via Tauri commands:

- `create_shell_tab(name, command, args, cwd, env_overrides) -> TabId`: validates inputs (command must resolve), spawns the PTY, sets up the processing layer, registers the new TabState, persists settings (debounced), broadcasts `TabCreated`, and returns the new TabId. Failure modes: command not found (return error to frontend, do not register tab); cwd doesn't exist (return error).
- `close_tab(tab_id) -> Result<(), Error>`: rejects if the target is a builtin (`claude` or `aider`). Otherwise: kills the subprocess, drops the processing layer task, removes the TabState, persists settings, broadcasts `TabClosed`. If the closed tab was active, the active-tab pointer moves to the next tab (or previous if it was the last); if no tabs remain... that can't happen in v1.2 because the two builtins can't be closed.
- `rename_tab(tab_id, new_name) -> Result<(), Error>`: updates name, persists, broadcasts.
- `reconfigure_shell_tab(tab_id, command, args, cwd, env_overrides) -> Result<(), Error>`: updates the persisted config but does not respawn — the new config takes effect on next restart of that shell. Restart-on-exit picks up the new config; user can also explicitly restart via a context menu action (deferred — for v1.2, restart only happens after a clean exit / crash).
- `restart_shell_tab(tab_id) -> Result<(), Error>`: kills the subprocess if running, then respawns with current config. Useful when the user's shell config changed and they want the change applied immediately. Optional for v1.2 — could be deferred to polish.

The `create_shell_tab` and `close_tab` commands are how the frontend's `+` button and right-click close menu communicate with the backend.

### Active Tab Switching (Unchanged Mechanics)

The active-tab routing rules from v1.1 carry over verbatim. Switching tabs:

- Stops audio playback immediately (rodio `Sink::clear()`).
- Discards the previously-active tab's pending TTS synthesis queue.
- Activates the new tab's xterm.js, routes keyboard input.
- Avatar reflects the new tab's current state.

For Shell tabs specifically: there's no TTS queue to discard (it doesn't exist), there's no Speaking state to interrupt (Shell tabs never enter Speaking). The behavior simplifies but the routing path is the same.

### Concurrency Model

The v1.1 concurrency model extends straightforwardly:

- N PTY reader tasks, N processing tasks (one per tab) — count grows to include user-created Shell tabs
- Single TTS synthesis task (only AI tabs feed it; Shell tabs do not appear in its input mux)
- Single audio playback task
- Single state manager task
- Single notification queue task

The notification queue's `idle` suppression rule (v2's V2-04) doesn't fire for Shell tabs because Shell tabs don't generate `idle` notifications in the first place. The `error` and `exited` notifications follow the same dedup-at-play-time rule (most recent per tab survives).

---

## UI Changes

### Tab Bar

The tab bar gains:

- A **`+` button** at the right end of the existing tabs (after all tabs, before any tab-bar-overflow scroll affordance).
  - Click: opens the **New Shell Tab** dialog (see below).
  - Tooltip: "New shell tab (Ctrl+T)".
- A **close button** (small `×`) on the right side of each user-created tab. Hidden on the two builtin tabs.
  - Click: confirms via a small inline confirm UI ("Close this tab? [Close] [Cancel]") if the tab's shell is currently running, no confirm if the shell has exited. The confirm UI prevents accidental misclicks.
- **Right-click menu** on each tab:
  - Builtin tabs: `Rename` only (allows renaming Claude → e.g. "Claude (work)"). The actual command stays `claude` regardless of name.
  - User Shell tabs: `Rename`, `Configure...` (opens the config dialog with current values pre-filled), `Restart shell` (kills + respawns; only meaningful if shell is running), `Close`.

Tab order:

- The two builtins (Claude, aider) are pinned to the leftmost positions in that order. They cannot be reordered in v1.2.
- User Shell tabs occupy positions 3..N. They cannot be reordered in v1.2 (drag-to-reorder is out of scope).
- New shell tabs are appended to the end.

Active-tab visual styling, status indicators, and DoneWhileAway flag rendering all carry over from v1.1.

### New Shell Tab Dialog

A small modal dialog with these fields:

- **Name** (text input). Default: "Shell N" where N is the next ordinal among user-created Shell tabs (so the first user shell is "Shell 1", second is "Shell 2", etc.). User can change to anything.
- **Shell command** (text input with a small "Browse..." button next to it). Default: the auto-detected platform default (Git Bash path on Windows if found, otherwise `powershell.exe`; `$SHELL` value on Linux). The Browse button opens an OS file picker filtered to executables.
- **Arguments** (text input). Default: the platform-appropriate default args for the chosen shell (`--login -i` for Git Bash, `-i` for `$SHELL`, `-NoLogo` for PowerShell). Single-line, space-separated; quoted segments are parsed via the standard shell-style splitter that portable-pty uses.
- **Working directory** (text input with "Browse..."). Default: cctts's launch directory (matches what the AI tabs use).
- A small banner at the top of the dialog when Git Bash detection failed (Windows only, see auto-detection section above).
- **Create** and **Cancel** buttons.

Validation on Create:

- Command must resolve to an executable (file exists and is executable, or is a name resolvable on PATH). Failure shows an inline error under the field.
- Working directory must exist. Failure shows an inline error.
- Name must be non-empty. Failure shows an inline error.

On Create: calls `create_shell_tab`, closes the dialog, the new tab appears in the tab bar and becomes active.

### Configure Tab Dialog

Identical fields to the New Shell Tab dialog, pre-filled with the current tab's configuration. The Create button is replaced with **Save**. Saving updates the persisted config but, per the lifecycle operation rules above, does not restart the running shell — the change applies on next restart.

A small note text in the dialog: "Changes apply to the next shell restart. To restart now, use the right-click → Restart shell menu." (Or omit if Restart shell is deferred from v1.2.)

### Bottom Status Bar (Unchanged)

No layout changes. Mute, announcements, volume on the right; left side still reserved.

### Settings Window: Tabs Section

The settings window gains a **Tabs** section listing all current tabs in their order. Each row shows: name, kind, command summary (for Shell tabs), and either an "Edit" button (opens the configure dialog) or, for builtins, just the configurable bits (extra CLI flags, notification text — the same fields v1.1 surfaces, just relocated under the Tabs heading).

The settings window is the canonical place to edit tab configuration in detail; the inline right-click → Configure menu is a shortcut into the same UI.

---

## Settings Schema Changes

The v1.1 schema had `tabs.claude` and `tabs.aider` as fixed sibling keys. v1.2 reshapes `tabs` into an ordered list to support a variable number of user-created Shell tabs.

### New `tabs` shape

```json
"tabs": [
  {
    "id": "claude",
    "kind": "ai_tool",
    "ai_tool_kind": "claude_code",
    "builtin": true,
    "name": "Claude",
    "command": "claude",
    "args": [],
    "cwd": null,
    "env": {},
    "tts_injection": {
      "enabled": true,
      "instructions": "<TTS markup instructions appended to system prompt>"
    },
    "notifications": {
      "idle": "Claude is idle",
      "awaiting_permission": "Claude is awaiting permission",
      "error": "Claude encountered an error"
    }
  },
  {
    "id": "aider",
    "kind": "ai_tool",
    "ai_tool_kind": "aider",
    "builtin": true,
    "name": "Aider",
    "command": "aider",
    "args": [],
    "cwd": null,
    "env": {},
    "tts_injection": {
      "enabled": false,
      "instructions": ""
    },
    "notifications": {
      "idle": "Aider is idle",
      "awaiting_permission": "Aider is awaiting permission",
      "error": "Aider encountered an error"
    }
  },
  {
    "id": "shell-1735684800",
    "kind": "shell",
    "builtin": false,
    "name": "Shell 1",
    "command": "C:\\Program Files\\Git\\bin\\bash.exe",
    "args": ["--login", "-i"],
    "cwd": null,
    "env": {},
    "notifications": {
      "error": "Shell encountered an error",
      "exited": "Shell exited (code {code})"
    }
  }
]
```

Notes:

- `id` is stable across launches and is used as the lookup key. For builtins it's the fixed strings `claude` and `aider`. For user tabs, generated at creation (e.g., `shell-<unix-timestamp>` or a UUID).
- `kind` is `ai_tool` or `shell`. `ai_tool_kind` is only present on AI-tool tabs and identifies which tool (drives permission patterns, TTS injection mechanism).
- `builtin: true` means the tab cannot be deleted via UI. v1.2 ships with exactly two builtins.
- `name` is the user-visible display name. Editable for all tabs. Defaults to the tool name for builtins, "Shell N" for user shell tabs.
- `command` and `args` are the spawn command. To keep the schema unified across tab kinds, v1.2 collapses v1.1's `extra_cli_flags` into `args`. The migration step appends the v1.1 `extra_cli_flags` array to `args` (which starts empty for builtins).
- `cwd` is the working directory; `null` means cctts's launch directory.
- `env` is a map of environment variable additions/overrides applied to the spawned subprocess on top of the inherited environment. Empty by default. (User-facing config UI for env may be deferred — see Out of Scope. The schema field is reserved.)
- `notifications.idle` and `notifications.awaiting_permission` are absent on Shell tabs; absent fields are skipped at serialization rather than stored as null.
- `notifications.exited` is present on Shell tabs only. The `{code}` placeholder is interpolated with the actual exit code at notification-fire time.
- Tab order is determined by array order. The first user-created shell tab appears at index 2 (after the two builtins).

### Migration from v1.1

On first launch with a v1.1 settings file:

1. Read `tabs.claude` (object) and `tabs.aider` (object) from the old schema.
2. Convert each to the new array-element shape: copy fields verbatim, add `id`, `kind`, `ai_tool_kind`, `builtin: true`, set `name` to "Claude" and "Aider" respectively, append the old `extra_cli_flags` to `args`.
3. Construct the new `tabs` array as `[claude, aider]` (no user tabs to migrate from v1.1).
4. Remove the old `tabs.claude` / `tabs.aider` keys from the in-memory settings; write the new schema on the next debounced save.

The migration is idempotent (re-reading a v1.2 settings file is a no-op) because the schema discriminator (array vs object at the `tabs` key) is unambiguous.

### Active tab persistence

A new top-level field:

```json
"session": {
  "active_tab_id": "claude"
}
```

Updated whenever the user switches tabs (debounced like other settings). On launch, the active tab is restored to whichever ID is here, falling back to the first tab in the array if the ID isn't found (e.g., if the user manually edited settings).

### Shortcut additions

```json
"shortcuts": {
  "open_compose": "Ctrl+Shift+E",
  "submit_compose": "Ctrl+Enter",
  "cancel_compose": "Escape",
  "open_settings": "Ctrl+,",
  "switch_to_tab_1": "Ctrl+1",
  "switch_to_tab_2": "Ctrl+2",
  "switch_to_tab_3": "Ctrl+3",
  "switch_to_tab_4": "Ctrl+4",
  "switch_to_tab_5": "Ctrl+5",
  "switch_to_tab_6": "Ctrl+6",
  "switch_to_tab_7": "Ctrl+7",
  "switch_to_tab_8": "Ctrl+8",
  "switch_to_tab_9": "Ctrl+9",
  "new_shell_tab": "Ctrl+T",
  "close_tab": "Ctrl+W"
}
```

Behavior:

- `switch_to_tab_N` switches to the tab at ordinal position N (1-indexed) in the current tab order. If fewer than N tabs exist, the shortcut is a no-op. The shortcut binding is to the *position*, not to a specific tab ID, so closing a Shell tab shifts higher-numbered tabs down by one.
- `new_shell_tab` opens the New Shell Tab dialog. Identical to clicking the `+` button.
- `close_tab` requests close on the currently active tab. No-op (with a transient toast — "This tab cannot be closed") on builtin tabs.

Migration: v1.1 settings have only `switch_to_tab_1` and `switch_to_tab_2`. The migration adds the remaining defaults if absent.

---

## Tab Persistence

The `tabs` array in settings is the source of truth for which tabs exist on launch. The persistence rules:

- **On `create_shell_tab`**: append to the array, debounced save.
- **On `close_tab`**: remove from the array, debounced save.
- **On `rename_tab`**: update name field, debounced save.
- **On `reconfigure_shell_tab`**: update command/args/cwd/env fields, debounced save.
- **On tab switch**: update `session.active_tab_id`, debounced save.

Builtins are always present in the array; a startup integrity check ensures Claude and aider entries exist (creates them with defaults if missing — handles a corrupted or hand-edited settings file). User-created Shell tabs persist exactly as configured.

If a user-created Shell tab's command no longer resolves at launch (e.g., they configured a path to a binary that's been uninstalled), the tab is created in the closed state with `closed_exit_code = None` and a message: "Shell command not found: <path>. Reconfigure or close this tab." The user can fix the config via right-click → Configure and then restart.

---

## Avatar and Notification Behavior for Shell Tabs

### Avatar

Shell tabs only ever drive the avatar to `Idle` or `Error`. The avatar overlay is the same as for AI tabs — same image assets, same opacity, same waveform sibling — but the waveform never animates for a Shell tab because no audio plays for them.

Switching from an AI tab in `Speaking` state to a Shell tab in `Idle` cuts off the audio (existing rule from v1.1) and the avatar transitions Speaking → Idle via the configured transition asset.

### Notifications

Shell tabs participate in the notification queue with a reduced trigger set:

| Transition (Shell tab) | Notification event |
|------------------------|---------------------|
| Anything → Error       | `error` |
| Subprocess exit        | `exited` |

`idle` and `awaiting_permission` are not triggers for Shell tabs (they are not even reachable transitions for the `awaiting_permission` flag, and `idle` would fire too often to be useful for a shell that idles by definition).

The notification queue's per-tab-dedup-at-play-time rule (v1.1) applies as-is. The idle-suppression-while-awaiting-permission rule (V2-04) does not apply to Shell tabs since neither side of that rule is reachable.

The `exited` notification is event-driven (fires once on subprocess exit), not state-driven. It does not re-fire if the closed message remains on screen — it only fires on the actual exit transition.

---

## What's Out of Scope for v1.2

Items that surfaced during v1.2 design but are deferred:

- **Drag-to-reorder tabs** — fixed order in v1.2 (builtins first, then user tabs in creation order).
- **Closing/hiding builtin tabs** — Claude and aider are always present.
- **Per-shell-tab font, theme, or xterm.js options** — global xterm.js config in v1.2.
- **Per-shell-tab environment variable UI** — the `env` field exists in the schema (so a hand-edited settings file can use it) but no settings UI for editing it. Deferred.
- **Restart-shell context menu action** — initial implementation handles restart only via the closed-tab Enter affordance. The right-click `Restart shell` may slip to v1.3 if not trivial.
- **Profiles/templates for shell tabs** (saved configurations the user can spawn from) — deferred to v1.3 if there's demand.
- **Shell auto-restart on crash** — v1.2 surfaces the closed message and waits for user input; no automatic respawn.
- **Tab groups, splits, or panes** — one shell per tab in v1.2.
- **SSH-as-tab-kind** as a first-class option — for v1.2, the user configures a Shell tab with `ssh user@host` as the command. Works fine; no special UI.
- **Aider permission detection** — still deferred from v1.1, no progress in v1.2.
- **Aider TTS markup injection** — still pending upstream aider CLI support.
- **Per-tab avatar configuration** (different images per tab) — global avatar config; state driven by active tab.
- **Per-tab TTS settings** — global only.
- **Notification text variables beyond `{code}`** — e.g., `{name}`, `{tab_position}` — could be added but not in v1.2.
- **History/log of subprocess exits** — fire-and-forget notifications, no log UI.

---

## Glossary Additions

In addition to the v1 and v2 glossaries:

- **Tab kind**: discriminator between `AiTool` (Claude / aider; full feature set) and `Shell` (configurable shell; reduced feature set).
- **Builtin tab**: one of the two pre-defined tabs (Claude, aider). Cannot be closed or removed from the tab bar in v1.2.
- **User tab** (or **user-created tab**): a Shell tab created via the `+` button or `Ctrl+T`. Can be closed, renamed, reconfigured.
- **Shell tab**: a tab of kind `Shell` running a configurable shell process. No TTS, no permission detection, reduced avatar/notification behavior.
- **Closed tab state**: a Shell-tab UI sub-state when the subprocess has exited. Shows a restart message; pressing Enter respawns.
- **New Shell Tab dialog**: the modal opened by `+` / `Ctrl+T` for creating a user-defined Shell tab.
- **Configure Tab dialog**: the modal for editing an existing Shell tab's name, command, args, cwd.
- **Shell auto-detection**: the platform-aware probe for a sensible default shell command at app launch (Git Bash on Windows, `$SHELL` on Linux).

---

## Implementation Phasing for v1.2

Detailed milestone specifications are in separate `MILESTONE-V3-*.md` files. The expected phasing:

1. **Tab kind abstraction and Shell tab type** (`MILESTONE-V3-01-tab-kinds.md`): introduce `TabKind`, refactor `TabState` and the processing layer to gate behavior on kind, add the third tab as a hardcoded Shell tab using auto-detected default shell, verify it spawns correctly on Windows and Linux. Ships with three tabs visible — Claude, aider, and a single Shell tab — but no creation/close UI yet. The Shell tab participates in the avatar / notification system per the rules above. Subprocess exit handling for Shell tabs lands in this milestone.

2. **Tab creation, close, rename UI** (`MILESTONE-V3-02-tab-management.md`): `+` button, New Shell Tab dialog with shell command browse and validation, close button on user tabs, right-click menus, `Ctrl+T` and `Ctrl+W` shortcuts. Persistence is *not* in this milestone — created tabs survive app launch only, lost on restart. This milestone proves the UI flow before persistence layers on top.

3. **Tab persistence and settings migration** (`MILESTONE-V3-03-persistence.md`): rework `tabs` settings schema from object-of-keys to array, write the v1.1 → v1.2 migration step, persist user-created Shell tabs across launches, persist active tab ID. Settings window's Tabs section also lands here.

4. **Polish and edge cases** (`MILESTONE-V3-04-polish.md`): subprocess command-not-found at launch (closed-state with reconfigure prompt), notification text editing UI for Shell tabs, `{code}` placeholder interpolation, `Ctrl+3`..`Ctrl+9` shortcut wiring, bottom status bar still working with N tabs, cross-platform shell auto-detection validation. Restart-shell context menu action lands here if time allows; otherwise slips to v1.3.

Each milestone produces a working app at its level of completeness. Milestones are sequential.

---

## Document Maintenance

This document is updated when:

- A v1.2 architectural decision changes
- A new component is added in v1.2 scope
- A scope item moves between in-scope and out-of-scope for v1.2

If a v1.3 design happens, it would supersede this document with a new `DESIGN-V4.md`, leaving this v3 document as a historical record (Option B convention from the v1 design discussion).
