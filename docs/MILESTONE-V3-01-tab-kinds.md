# Milestone V3-01: Tab Kind Abstraction and Shell Tab Type

## Purpose

Introduce the `TabKind` abstraction that distinguishes AI-tool tabs from Shell tabs, refactor the per-tab pipeline to gate TTS / permission detection / avatar transitions on kind, and ship a hardcoded third tab ("Shell 1") that runs an auto-detected default shell. After this milestone, the app launches with three tabs — Claude, aider, Shell 1 — all functional, but there is no UI yet for creating, closing, or reconfiguring tabs.

This is the architectural lift of v1.2. UI work and persistence depend on this milestone landing cleanly. Subsequent milestones layer UI (M2), persistence (M3), and polish (M4) on top of the abstraction established here.

Read `DESIGN-V3.md` first; this document assumes its terminology.

## What This Milestone Delivers

1. A `TabKind` enum (`AiTool(AiToolKind)` / `Shell`) usable across the codebase.
2. `TabState` extended with `kind`, `name`, `closed`, `closed_exit_code` fields.
3. The processing layer constructed differently for Shell tabs — no TTS extraction pipeline.
4. The PTY spawn generalized to accept `(command, args, cwd, env)` from a tab's configuration rather than hardcoded `claude` / `aider`.
5. A shell auto-detection module producing the default `ShellSpec` for the current platform.
6. A hardcoded third tab "Shell 1" spawned at launch, using the auto-detected default shell.
7. Subprocess exit handling for Shell tabs: the tab transitions to a `closed` UI state with a centered "Shell exited (code N) — press Enter to restart, or close this tab" message, and pressing Enter respawns the same shell.
8. Avatar state machine routing Shell-tab signals correctly: only `Idle` and `Error` are reachable; user input and output do not transition Shell-tab avatar state.
9. Notification system extended with the `exited` trigger; per-kind notification allowlists (Shell tabs get `error` and `exited` only; AI tabs get `idle` / `awaiting_permission` / `error` as before).
10. `Ctrl+3` shortcut wired to switch to the third tab.

## What This Milestone Does NOT Do

- No `+` button on the tab bar (M2).
- No close button on tabs (M2).
- No right-click menu (M2).
- No New Shell Tab dialog or Configure Tab dialog (M2).
- No persistence — the third tab is hardcoded in code, not loaded from settings (M3 reshapes the schema).
- No `Ctrl+4`..`Ctrl+9` shortcuts (M4 — they have nothing to switch to in M1).
- No restart-shell context menu (M4).
- No `{code}` placeholder interpolation in the `exited` notification text (M4 — for now the notification text is the literal string from settings; the placeholder lands in M4).
- No env var UI (out of scope for v1.2).
- No tab order or count changes — order is fixed: Claude, aider, Shell 1.

## Implementation Steps

### 1. Add `TabKind` and `AiToolKind` enums

In `src/state/mod.rs`:

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum AiToolKind {
    ClaudeCode,
    Aider,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TabKind {
    AiTool { ai_tool_kind: AiToolKind },
    Shell,
}
```

The `serde` tag layout is chosen to match the v1.2 settings schema described in `DESIGN-V3.md`. M3 will leverage this; M1 only uses it internally.

### 2. Extend `TabState`

In `src/state/mod.rs`, update the existing `TabState` struct:

```rust
pub struct TabState {
    pub id: TabId,
    pub kind: TabKind,
    pub name: String,
    pub avatar_state: AvatarState,
    pub awaiting_permission: bool,
    pub done_while_away: bool,
    pub claude_still_generating: bool,
    pub closed: bool,
    pub closed_exit_code: Option<i32>,
}
```

Initialize `closed: false` and `closed_exit_code: None` for all tabs at construction. `claude_still_generating` is only set for `AiTool` tabs whose `ai_tool_kind` is `ClaudeCode`; the field exists on Shell tabs but is never observed.

### 3. Create the shell auto-detection module

Add `src/shell/mod.rs` and `src/shell/detect.rs`:

```rust
// src/shell/mod.rs
pub mod detect;

pub struct ShellSpec {
    pub command: PathBuf,
    pub args: Vec<String>,
}
```

In `src/shell/detect.rs`, implement `pub fn default_shell() -> ShellSpec`. Behavior per `DESIGN-V3.md`:

- **Linux**: read `$SHELL`. If set and the binary exists, return it with `["-i"]`. Else `/bin/bash` with `["-i"]`. Else `/bin/sh` with `["-i"]`.
- **Windows**: probe in order:
  1. `C:\Program Files\Git\bin\bash.exe`
  2. `C:\Program Files (x86)\Git\bin\bash.exe`
  3. The `InstallPath` value under `HKLM\SOFTWARE\GitForWindows` registry key, suffixed with `\bin\bash.exe`. Use the `winreg` crate (already in dependencies if it's pulled in by Tauri's Windows side; otherwise add it).
  4. `bash.exe` resolved on `PATH` (use the `which` crate — add as dep if not present).
  
  If found, return with `["--login", "-i"]`.
  
  If none found, return `powershell.exe` with `["-NoLogo"]`.

The function runs once at app launch (call site is `src/main.rs`) and the result is cached in app state. Re-running at runtime is fine but unnecessary in M1.

Also implement `pub fn was_default_git_bash_found() -> bool` so M2's UI can show the fallback banner when relevant. M1 doesn't surface the banner but exposing the helper now keeps M2 simpler.

### 4. Generalize the PTY spawn

The current `pty::spawn_*` functions in `src/pty/` are hardcoded per AI tool. Replace with a generic:

```rust
pub fn spawn(
    command: &Path,
    args: &[String],
    cwd: Option<&Path>,
    env: &HashMap<String, String>,
) -> Result<PtyHandle, AppError>;
```

Internal: same `portable_pty::native_pty_system()` call as before; use `CommandBuilder::new(command)` and apply args/cwd/env. The two AI-tool spawns become callers of this generic function with hardcoded args (matching v1.1 behavior).

### 5. Refactor the processing-layer factory

In `src/processing/mod.rs`, the per-tab processing layer is currently constructed assuming TTS extraction is always wanted. Add a constructor parameter:

```rust
pub fn new_for_tab(kind: TabKind, /* existing params */) -> ProcessingLayer { ... }
```

When `kind` is `Shell`:
- Skip wiring the TTS tag detector
- Skip the two-view (raw + rendered) tracking required only for tag-stripping — Shell mode is single-view (just rendered bytes for xterm.js, no extraction)
- Skip the sentence-boundary segmenter
- Hybrid flush trigger still runs (consistency of pacing across tabs)

The concrete code shape is up to the implementer — either a runtime-checked branch in `ProcessingLayer::process()` or two separate `ProcessingLayer` variants behind an enum/trait. Prefer the latter (different shape, different code path, no per-byte conditionals in the hot loop). Whichever is chosen, document the choice in a comment at the top of the module.

### 6. Spawn the third tab at launch

In `src/main.rs`, after spawning the Claude and aider tabs, spawn a third using the auto-detected shell:

```rust
let shell_spec = shell::detect::default_shell();
let shell_tab_id = TabId::new("shell-1"); // hardcoded ID for M1
state_manager.register_tab(TabState {
    id: shell_tab_id.clone(),
    kind: TabKind::Shell,
    name: "Shell 1".to_string(),
    avatar_state: AvatarState::Idle,
    awaiting_permission: false,
    done_while_away: false,
    claude_still_generating: false,
    closed: false,
    closed_exit_code: None,
});
let pty_handle = pty::spawn(&shell_spec.command, &shell_spec.args, None /* cwd default */, &HashMap::new())?;
let processing = ProcessingLayer::new_for_tab(TabKind::Shell, /* ... */);
spawn_pty_reader_task(pty_handle, processing, shell_tab_id);
```

The `shell-1` ID is intentionally non-numeric and string-based to match how M3 will generate IDs for user-created tabs.

### 7. Route `SubprocessExited` per kind

The state manager already receives `SubprocessExited { tab: TabId, code: Option<i32> }` signals from the PTY reader tasks (this is wired in v1). M1 splits the handling:

- For `AiTool` tabs: existing behavior (transition to `Error`, log).
- For `Shell` tabs:
  1. Set `state.closed = true`, `state.closed_exit_code = code`.
  2. Avatar state stays `Idle` (no `Error` transition for clean shell exits).
  3. If the active tab is *not* this Shell tab, fire an `exited` notification.
  4. Broadcast a `TabClosedStateChanged` event to the frontend so the closed overlay renders.

Define `TabClosedStateChanged` as:

```rust
pub struct TabClosedStateChanged {
    pub tab: TabId,
    pub closed: bool,
    pub exit_code: Option<i32>,
}
```

Emitted via Tauri event whenever `closed` flips true *or* false (M1 emits true on exit; restart will emit false).

### 8. Implement the closed-state restart flow

The restart flow has both backend and frontend components.

**Backend** — add a Tauri command `restart_shell_tab(tab_id)`:

```rust
#[tauri::command]
async fn restart_shell_tab(tab_id: String, state: tauri::State<'_, AppState>) -> Result<(), String> { ... }
```

Behavior:
1. Look up the tab. Reject if not Shell or not closed.
2. Look up the tab's spawn config. In M1, the only Shell tab is hardcoded, so the config is the `default_shell()` result captured at launch. Store this on the `TabState` or in a side table indexed by `TabId`. (Side table is cleaner; M3 will move config into settings, but the lookup interface stays the same.)
3. Spawn a new PTY with the same config.
4. Wire it into the same processing layer instance (which is per-tab, retained across restarts) and the same xterm.js instance on the frontend.
5. Set `state.closed = false`, broadcast `TabClosedStateChanged { closed: false, ... }`.

**Frontend** — add `ClosedShellOverlay.svelte`:
- Renders a centered message "Shell exited (code {N}). Press Enter to restart." Or "Shell exited. Press Enter to restart." if exit code is None.
- Non-zero exit codes render in the error color.
- Subscribes to `TabClosedStateChanged` to know when to show/hide.
- Mounted as a sibling of xterm.js, absolutely positioned to cover it, only visible when the tab's `closed` flag is true.

In `Tab.svelte` (or wherever the active-tab keyboard handling lives), add: when the active tab is a closed Shell tab, intercept Enter keypress, suppress propagation to xterm.js, and invoke the `restart_shell_tab` Tauri command.

### 9. Avatar state machine for Shell tabs

The state machine sits in `src/state/manager.rs`. M1 adds: when a state-transition signal arrives for a Shell tab, only allow transitions to `Idle` or `Error`. Specifically:

- `UserInput` signal for a Shell tab: ignore (no Listening transition).
- `OutputActivity` signal for a Shell tab: ignore (no Thinking transition).
- `TtsStarted` / `TtsEnded`: cannot occur (TTS is bypassed).
- `SubprocessExited`: handled per step 7 — does not change avatar state.
- Hard error path (e.g., processing layer panic relayed as a signal): transition to `Error`.

The transition asset still plays on any avatar state change, but for Shell tabs that's only Idle ↔ Error, which is rare.

### 10. Notification system additions

In `src/notifications/mod.rs`:

- Add a new notification trigger variant: `Exited`.
- Define a per-kind allowlist of triggers:
  - AI tabs: `Idle`, `AwaitingPermission`, `Error`
  - Shell tabs: `Error`, `Exited`
- When a notification is queued, drop it silently if it's not in the allowlist for the originating tab's kind. This is a defense-in-depth check; in practice the upstream code already won't generate disallowed triggers for a given kind, but the allowlist makes the rule explicit.

The notification text for `Exited` is read from a per-tab setting field (see step 12). For M1, the literal text from settings is used as-is — `{code}` interpolation comes in M4.

### 11. Wire `Ctrl+3` shortcut

The shortcut store already handles `switch_to_tab_1` and `switch_to_tab_2`. Add `switch_to_tab_3` with default `Ctrl+3`. The shortcut is bound to *position* (1-indexed) in the tab order, not a tab ID — see `DESIGN-V3.md`. Since M1 has a fixed three-tab order, this collapses to "switch to the Shell tab" but the position-based design is what M2/M4 generalize.

### 12. Settings schema additions (interim)

M3 reshapes the entire `tabs` schema. M1 has to live with the v1.1 shape (`tabs.claude` / `tabs.aider`) plus a temporary key for the Shell tab's defaults:

```json
"tabs": {
  "claude": { ... },
  "aider": { ... }
},
"_shell_1_tmp": {
  "name": "Shell 1",
  "notifications": {
    "error": "Shell encountered an error",
    "exited": "Shell exited (code {code})"
  }
}
```

The `_shell_1_tmp` key is explicitly temporary (the leading underscore signals "this is going away"). M3's migration removes it. Keeping it out of the main `tabs` object avoids polluting v1.1's shape with a hybrid that M3 then has to migrate twice.

Alternative considered: skip persistence entirely in M1, hardcode the notification strings in source. Rejected because M2 needs *some* mechanism to surface notification text in UI, and putting it in settings now (even under a temporary key) keeps the data flow consistent across milestones.

## Files Touched / Added

**Added:**
- `src/shell/mod.rs`
- `src/shell/detect.rs`
- Frontend `ClosedShellOverlay.svelte`

**Modified:**
- `src/state/mod.rs` (TabKind, AiToolKind, TabState fields)
- `src/state/manager.rs` (per-kind signal routing, new events)
- `src/processing/mod.rs` (kind-aware factory)
- `src/pty/mod.rs` (generic spawn function; existing AI-tool spawns become thin wrappers)
- `src/notifications/mod.rs` (Exited trigger, per-kind allowlist)
- `src/main.rs` (third tab spawn, default-shell detection call)
- `src/settings/schema.rs` (`_shell_1_tmp` interim key)
- `src/ipc/mod.rs` (register `restart_shell_tab` command)
- Frontend `TabBar.svelte` (render three tabs)
- Frontend `Tab.svelte` or equivalent (Enter-to-restart key handler when closed)
- Frontend shortcut store (add `switch_to_tab_3`)

**Dependencies possibly added:**
- `winreg` (Windows-only, for Git Bash registry probe)
- `which` (cross-platform PATH resolution)

## Edge Cases and Gotchas

- **PowerShell fallback on Windows when Git Bash is missing**: M1 returns `powershell.exe` with no banner UI yet (banner comes in M2's dialog). The Shell tab still works; the user just gets PowerShell. Verify a fresh Windows VM without Git installed lands on PowerShell cleanly.
- **`$SHELL` unset on Linux**: rare but possible (some minimal containers). The fallback chain handles it. Test by running `unset SHELL && ./cctts`.
- **`$SHELL` set to a binary that doesn't exist**: e.g., user changed shell paths. Treat as "not found" and fall through to `/bin/bash`.
- **Restart while audio is playing on a different tab**: shouldn't matter — audio belongs to the AI tab, restarting the Shell tab doesn't touch it. But verify.
- **PTY reader task lifecycle on restart**: when restarting, the old PTY reader task has already exited (that's what triggered `SubprocessExited`). Make sure the new spawn creates a fresh task; do not try to reuse the old one.
- **Closed overlay vs. xterm.js focus**: when the closed overlay is showing, xterm.js should not receive keystrokes. Ensure focus management transfers to the overlay (or, simpler: the overlay's keydown handler stops propagation). Otherwise typing while the overlay is up could write garbage to a defunct PTY handle.
- **The `name: "Shell 1"`** is hardcoded in M1. M3 makes it configurable. Don't add UI for renaming in M1 — that's M2's right-click flow.
- **Notification text without `{code}` placeholder**: in M1, if the user sets the `exited` notification text to `"Shell exited (code {code})"`, it will literally include `{code}` (no interpolation yet). This is intentional — M4 implements interpolation. Document this in the M1 ship note.

## Manual Verification Checklist

On both Windows and Linux:

- [ ] App launches with three tabs: Claude, aider, Shell 1.
- [ ] Clicking each tab activates it; the active tab gets keyboard focus.
- [ ] `Ctrl+1`, `Ctrl+2`, `Ctrl+3` switch tabs correctly.
- [ ] Typing in Shell 1 writes to the shell; output renders in xterm.js.
- [ ] Standard Linux tools (`grep`, `cat`, `nano`, `history`) work in Shell 1 (Windows: depends on Git Bash being installed).
- [ ] Avatar in Shell 1 stays at `Idle` regardless of typing or shell output.
- [ ] No TTS audio plays from Shell 1 output, even if the shell echoes back text.
- [ ] Compose overlay (`Ctrl+Shift+E`) submits to Shell 1 when Shell 1 is active. Submitted text appears as if pasted.
- [ ] Type `exit` in Shell 1: the closed overlay appears with "Shell exited (code 0). Press Enter to restart..."
- [ ] Press Enter on the closed overlay: a fresh shell prompt appears.
- [ ] Switch to Claude or aider, then exit Shell 1 from a separate kill (e.g., `kill -9` from another terminal): the `exited` notification fires audibly. Switch back to Shell 1 — the overlay is showing.
- [ ] Run a non-zero-exit command in Shell 1 like `bash -c "exit 7"` then `exit`: the closed overlay shows code `7` in error color.

Windows-specific:

- [ ] On a machine with Git for Windows: Shell 1 spawns Git Bash (`bash.exe`) — verify with `echo $SHELL` or `which bash`.
- [ ] On a clean Windows machine without Git: Shell 1 spawns PowerShell. (Test in a clean VM or by temporarily renaming Git's install dir.)

Linux-specific:

- [ ] `$SHELL` is honored: change shell, relaunch, verify.

## Done Criteria

- All ten "What This Milestone Delivers" items work end-to-end.
- All three tabs are functional in their respective modes.
- The closed-state restart flow works without relaunching the app.
- All "Manual Verification Checklist" items pass on at least one Windows machine and one Linux machine.
- No regression in v1.1 behavior for the Claude and aider tabs (TTS, permission detection, status indicators, notifications).
- `cargo test` passes; manual smoke tests pass.
