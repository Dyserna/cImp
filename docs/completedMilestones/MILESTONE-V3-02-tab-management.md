# Milestone V3-02: Tab Creation, Close, and Rename UI

## Purpose

Add the user-facing UI for managing Shell tabs: a `+` button on the tab bar that opens a New Shell Tab dialog, a close button on user-created tabs, a right-click context menu with rename/configure/close, and the corresponding keyboard shortcuts (`Ctrl+T`, `Ctrl+W`).

Persistence is *not* part of this milestone. Tabs created via this UI live in memory only and are lost when the app is restarted. M3 layers persistence on top. This split is intentional — proving the UI flow first, without the complications of schema migration, makes both milestones easier to reason about and easier to ship.

Read `DESIGN.md` and `MILESTONE-V3-01-tab-kinds.md` first.

## What This Milestone Delivers

1. A `+` button at the right end of the tab bar that opens the New Shell Tab dialog.
2. A New Shell Tab dialog with name, shell command (with file picker), arguments, and working directory (with directory picker) fields, validation, and Create / Cancel actions.
3. A close button (`×`) on user-created Shell tabs only. Builtin tabs (Claude, aider) have no close button.
4. A close confirmation UI (inline) that prevents accidental misclicks on a running shell.
5. A right-click context menu on tabs with kind-appropriate entries:
   - Builtin tabs: `Rename` only.
   - User Shell tabs: `Rename`, `Configure...`, `Close`.
6. A Configure Tab dialog (sharing field UI with the New Shell Tab dialog) for editing existing user Shell tabs' configuration.
7. A rename inline UI (or modal — implementer's choice; inline preferred) for renaming any tab.
8. `Ctrl+T` keyboard shortcut to open the New Shell Tab dialog.
9. `Ctrl+W` keyboard shortcut to close the active tab (no-op with toast on builtins).
10. The Git Bash detection banner shown in the New Shell Tab dialog when Git Bash was not found at launch (Windows only).

## What This Milestone Does NOT Do

- No persistence — created/renamed/configured tabs revert to the v1.1 + Shell 1 state on app restart (M3).
- No `Ctrl+4`..`Ctrl+9` shortcuts — they have no use yet without persistence and could collide with shortcuts the user creates more than 3 tabs and then expects to switch with `Ctrl+4` *during the session*. Actually that case is real — see step 8 below; we wire `switch_to_tab_4..9` here as a session-only nicety. The full default settings entries land in M3.
- No restart-shell context menu action (M4).
- No env var UI (out of scope for v1.2).
- No drag-to-reorder (out of scope for v1.2).
- No tab-bar overflow handling — tabs become narrower as count grows; explicit overflow polish is deferred.
- No `{code}` placeholder interpolation (M4).
- No settings window Tabs section (M3).

## Implementation Steps

### 1. Backend Tauri commands

In `src/ipc/mod.rs`, add four new commands:

```rust
#[tauri::command]
async fn create_shell_tab(
    name: String,
    command: String,
    args: Vec<String>,
    cwd: Option<String>,
    env: HashMap<String, String>,
    state: tauri::State<'_, AppState>,
) -> Result<TabId, ShellTabError>;

#[tauri::command]
async fn close_tab(tab_id: String, state: tauri::State<'_, AppState>) -> Result<(), CloseTabError>;

#[tauri::command]
async fn rename_tab(tab_id: String, new_name: String, state: tauri::State<'_, AppState>) -> Result<(), RenameTabError>;

#[tauri::command]
async fn reconfigure_shell_tab(
    tab_id: String,
    name: String,
    command: String,
    args: Vec<String>,
    cwd: Option<String>,
    env: HashMap<String, String>,
    state: tauri::State<'_, AppState>,
) -> Result<(), ReconfigureError>;
```

Define error enums with explicit variants the frontend can handle (`CommandNotFound`, `CwdNotFound`, `EmptyName`, `BuiltinNotClosable`, `TabNotFound`, etc.). Serialize errors via `serde` with a discriminator field.

### 2. Implement `create_shell_tab`

1. Validate `name`: non-empty after trim. → `EmptyName`.
2. Validate `command` resolves: if it contains a path separator, check the file exists and is executable. Otherwise resolve via PATH using the `which` crate. → `CommandNotFound { tried: String }`.
3. Validate `cwd`: if `Some`, check the directory exists. → `CwdNotFound`.
4. Generate a fresh `TabId`. Use `format!("shell-{}", uuid::Uuid::new_v4())` — uuid is more robust than timestamp-based and avoids collisions if multiple tabs are created in the same second.
5. Construct a `TabState` with `kind: TabKind::Shell`, the provided name, `closed: false`, `closed_exit_code: None`.
6. Spawn a PTY via `pty::spawn(&command_path, &args, cwd.as_deref(), &env)`.
7. Construct a Shell-mode processing layer (the kind-aware factory from M1).
8. Wire PTY ↔ processing ↔ state-manager (same wiring as the M1 hardcoded shell tab — extract this into a helper function during this step if it isn't already, since it's now called from at least three places: app launch for Claude/aider, app launch for Shell 1, and create_shell_tab).
9. Append the tab to the state manager's ordered tabs list.
10. Store the spawn config in the per-tab side table (so restart-on-exit picks up the right command). M3 moves this to settings; M2 keeps it in-memory.
11. Broadcast `TabCreated { tab, kind, name, position }` via Tauri event.
12. Return the new `TabId`.

The frontend opens the dialog, collects fields, calls this command. On `Ok`, the frontend's tab bar receives the `TabCreated` event and updates accordingly. On `Err`, the dialog displays the error inline next to the offending field and stays open.

### 3. Implement `close_tab`

1. Look up the tab. → `TabNotFound`.
2. Reject if `tab.kind` is `AiTool` *or* if the tab id is `claude` or `aider`. → `BuiltinNotClosable`. (Belt-and-suspenders: the kind check covers AI tabs in general; the id check is explicit for the two specific builtins. M2 has no other AI tabs, but the kind check future-proofs.)
3. Send a kill signal to the subprocess (`PtyHandle::kill()` or equivalent — portable-pty exposes process kill via the `Child` returned from spawn). If the subprocess doesn't exit within 2 seconds, force-kill.
4. Drop the per-tab processing layer task (the `tokio::task::JoinHandle` should be tracked per tab so it can be dropped/aborted here).
5. Remove the tab from the state manager's tab list.
6. If this was the active tab, switch active to the previous tab in the order (or the first tab if the closed tab was already at position 1 — but since builtins can't be closed, the closed tab is always at position ≥ 3, so the previous tab always exists).
7. Broadcast `TabClosed { tab }`.

### 4. Implement `rename_tab`

1. Validate `new_name`: non-empty after trim. → `EmptyName`.
2. Look up tab. → `TabNotFound`.
3. Update the tab's `name` field.
4. Broadcast `TabRenamed { tab, name }`.

Allowed for both builtin and user tabs (renaming a builtin doesn't change its `command`, just the display name).

### 5. Implement `reconfigure_shell_tab`

1. Validate name, command, cwd as in `create_shell_tab`.
2. Reject if the target tab is not Shell kind. → `WrongKind`.
3. Update the tab's name and the per-tab spawn config in the side table.
4. Broadcast `TabReconfigured { tab, name }` (the rest of the config is internal).

Per `DESIGN.md`, this does *not* respawn the running shell. The new config takes effect on next restart (manual via the closed overlay, or automatic on subprocess exit).

### 6. New Shell Tab dialog component

Create `frontend/src/components/NewShellTabDialog.svelte`. The dialog is a modal overlay (full-screen translucent backdrop, centered card). Fields:

- **Name**: `<input type="text">`. Default value: `"Shell {N}"` where `N` is one greater than the current count of user Shell tabs. Computed from the tabs store at dialog-open time.
- **Shell command**: `<input type="text">` paired with a "Browse..." button. The button calls Tauri's `dialog.open` with `directory: false` and (on Windows) `filters: [{ name: "Executable", extensions: ["exe"] }]`. Default value: the platform default shell command. Backend exposes a query command `default_shell_spec() -> ShellSpec` that returns `{ command, args, args_default_string }` for the dialog to populate. (M1's `default_shell()` is internal; M2 wraps it in a Tauri command.)
- **Arguments**: `<input type="text">`. Default value: the platform default args joined with single spaces. On Create, parse via `shlex` (in Rust, server-side) — frontend just sends the raw string and the backend splits.
- **Working directory**: `<input type="text">` with a "Browse..." button calling Tauri's `dialog.open` with `directory: true`. Default value: empty (interpreted as cctts's launch directory by the backend).
- **Banner** (Windows only, shown if `was_default_git_bash_found()` returns false): "Git Bash not detected. Defaulting to PowerShell. Linux tools (grep, cat, nano) will not be available. Install Git for Windows to enable Git Bash by default, or set a custom shell below." Rendered above the form fields with a warning icon.

Buttons: `Create` and `Cancel`. Create triggers the backend call and shows inline errors per field. Cancel closes the dialog and discards entered values.

### 7. Configure Tab dialog component

Create `frontend/src/components/ConfigureTabDialog.svelte`. Mostly identical to NewShellTabDialog — extract a shared `ShellTabFields.svelte` component containing just the form fields (name, command, args, cwd) and use it in both. Differences:

- Button label: `Save` instead of `Create`.
- Pre-fills with the target tab's current configuration (passed as a prop).
- On Save, calls `reconfigure_shell_tab`.
- A small note at the bottom: "Changes apply on next shell restart."

### 8. Tab bar UI changes

Modify `TabBar.svelte`:

1. Render existing tabs from the tabs store, in order.
2. After the last tab, render a `+` button: small icon button styled to match tab heights, with `aria-label="New shell tab"` and `title="New shell tab (Ctrl+T)"`.
3. Inside each tab's render template:
   - For user Shell tabs (`!tab.builtin`): render an `×` button on the right side of the tab. Hidden by default; shown on tab hover or when the tab is active. Click triggers the close confirm UI.
   - For builtin tabs: no `×` button.
4. Bind a contextmenu handler to each tab to open the `TabContextMenu` (step 9).

The close confirm UI: when `×` is clicked, the tab's content briefly transforms to show a small inline confirm: "Close? [Yes] [No]" replacing the tab name. Click `Yes` → call `close_tab`. Click `No` (or click outside) → revert to normal. If the tab's shell is in the closed state already, skip the confirm and close immediately (the user has nothing to lose).

### 9. Tab context menu component

Create `frontend/src/components/TabContextMenu.svelte`. Triggered by right-click on a tab. Renders a small popover at the cursor position with kind-appropriate entries:

- For builtin tabs: `Rename`.
- For user Shell tabs: `Rename`, `Configure...`, `Close`.

`Rename` → enters the inline rename mode on the tab (the tab's name becomes a focused `<input>`, Enter submits, Escape cancels). The submit calls `rename_tab`.
`Configure...` → opens `ConfigureTabDialog` with the tab's current config pre-filled.
`Close` → behaves the same as the `×` button.

Suppress the browser's native context menu on the tab bar (`event.preventDefault()` on the contextmenu handler). Click outside the popover closes it.

### 10. Subscribe to backend events

In `frontend/src/stores/tabs.ts` (or equivalent), subscribe to:

- `TabCreated`: append the new tab to the local store, set it as active.
- `TabClosed`: remove the tab from the local store. If it was the active tab, the backend has already moved active; reflect that.
- `TabRenamed`: update the name in the local store.
- `TabReconfigured`: update the name in the local store (other fields are backend-only).

These are Tauri events (`event.listen()`). Run subscriptions at app init.

### 11. Keyboard shortcuts

Wire `Ctrl+T` and `Ctrl+W`:

- `Ctrl+T`: opens NewShellTabDialog. Identical to clicking the `+` button. Works regardless of which tab is active.
- `Ctrl+W`: invokes `close_tab` on the active tab. If the backend returns `BuiltinNotClosable`, show a transient toast: "This tab cannot be closed."

Also wire `switch_to_tab_4` through `switch_to_tab_9` as session-only shortcuts (defaults `Ctrl+4`..`Ctrl+9`). These switch by ordinal position. M3's settings migration adds the persisted defaults; for M2 it's enough that they work in-session. The shortcut layer should treat absent settings entries as "use the default keybinding", so wiring the defaults in the frontend's shortcut store handles this naturally.

### 12. Inline rename UI

When `Rename` is selected from the context menu (or a user double-clicks the tab name — optional, nice-to-have), the tab's name renders as an `<input>` with the current name pre-filled and selected. Behavior:

- `Enter`: submit the new name via `rename_tab`. Empty name shows an inline error (red border, transient text). Otherwise the rename succeeds and the tab returns to display mode.
- `Escape`: cancel and revert.
- Blur (click elsewhere): submit, same as Enter (with empty-name guard).

This avoids a modal for the common rename case while still supporting validation.

## Files Touched / Added

**Added:**
- Frontend `NewShellTabDialog.svelte`
- Frontend `ConfigureTabDialog.svelte`
- Frontend `ShellTabFields.svelte` (shared field component)
- Frontend `TabContextMenu.svelte`

**Modified:**
- Backend `src/ipc/mod.rs` (four new commands + error types)
- Backend `src/state/manager.rs` (lifecycle operations, event broadcasts, per-tab task tracking)
- Backend `src/main.rs` (extract the tab-spawn helper used by both launch and create_shell_tab)
- Frontend `TabBar.svelte` (+ button, × button, close confirm, contextmenu wiring)
- Frontend `Tab.svelte` (inline rename mode)
- Frontend `stores/tabs.ts` (event subscriptions, store mutations)
- Frontend shortcut store / handler (`Ctrl+T`, `Ctrl+W`, `Ctrl+4..9`)

**Dependencies possibly added:**
- `shlex` (Rust crate) — for argument splitting in `create_shell_tab`
- `uuid` (Rust crate) — for tab id generation
- `@tauri-apps/plugin-dialog` (frontend) — for file/directory picker, if not already used

## Edge Cases and Gotchas

- **Two dialogs at once**: keep dialog state in a single store with at most one open at a time. Opening a new dialog while one is open: replace the existing one (or no-op — implementer's choice; replacement is more forgiving). Document the choice.
- **`Ctrl+W` while a dialog is open**: do not close the active tab. The dialog should consume Escape; let `Ctrl+W` close the dialog too if the implementer prefers, otherwise route only after no dialog is active. Pick one and document.
- **Closing the active tab with `Ctrl+W`**: backend moves active to the previous tab. Frontend's active-tab indicator updates from the `TabClosed` event. Verify focus lands correctly in xterm.js after the switch.
- **Closing a Shell tab while its closed overlay is showing**: should work cleanly — the PTY is already dead, just remove the tab.
- **Rename to whitespace-only string**: trim and reject (treat as empty).
- **Rename to an exact duplicate of another tab's name**: allowed. Names are display-only; users may legitimately want two "Shell" tabs.
- **Browse… button on Linux for the executable picker**: no extension filter (Linux doesn't use extensions for executables). The picker should not restrict to executable bit either — Tauri's dialog plugin doesn't expose that filter; just open the picker and let the backend reject unresolvable commands.
- **Argument parsing edge case**: arguments containing spaces in filenames need quoting. `shlex` handles this. Document in the dialog field's help text: `Use double quotes for arguments containing spaces, e.g.: --config "C:\My Folder\config.toml"`.
- **Symlinks for shell binaries on Linux**: `which` follows symlinks; that's fine. Don't try to resolve to canonical paths — keep what the user specified.
- **`create_shell_tab` failure mid-flight**: e.g., command resolves but PTY spawn fails (rare, but possible if the binary is corrupted). Roll back: don't register the TabState, don't broadcast TabCreated, return an error. The frontend dialog stays open showing the error.
- **Closing a tab during its own subprocess startup**: theoretically possible if user spams. The close handler should be idempotent — killing an already-dead PTY is fine, removing an already-removed tab is fine.
- **The `+` button at the far right of the tab bar conflicting with the gear icon (settings)**: layout must accommodate. `+` button stays in the tab strip; gear icon is on the avatar (per v1 design). They don't conflict in layout terms.

## Manual Verification Checklist

- [ ] `+` button appears at the right end of the tab bar.
- [ ] Clicking `+` opens the New Shell Tab dialog with sensible defaults.
- [ ] On Windows without Git Bash: dialog shows the fallback banner.
- [ ] Browse… for command opens the OS file picker; selected path appears in the field.
- [ ] Browse… for cwd opens the OS directory picker.
- [ ] Submitting with empty name shows an inline error.
- [ ] Submitting with a non-existent command shows an inline error.
- [ ] Submitting with a non-existent cwd shows an inline error.
- [ ] Successful Create closes the dialog, the new tab appears at the right end, becomes active, and shows a working shell.
- [ ] `Ctrl+T` opens the dialog identically to clicking `+`.
- [ ] User Shell tabs show an `×` close button on hover or when active.
- [ ] Builtin tabs show no `×` button.
- [ ] Clicking `×` on a running shell shows the inline confirm; Yes closes, No reverts.
- [ ] Clicking `×` on a closed shell skips the confirm.
- [ ] `Ctrl+W` on a user tab closes it; the active tab moves to the previous one.
- [ ] `Ctrl+W` on a builtin tab shows a transient toast.
- [ ] Right-click on a builtin tab: only `Rename` appears.
- [ ] Right-click on a user Shell tab: `Rename`, `Configure...`, `Close` appear.
- [ ] `Rename` on a builtin: enters inline rename mode; Enter saves; the new name persists in the tab bar (lost on app restart in M2 — that's M3's job).
- [ ] `Configure...` on a user tab: pre-fills the dialog with current values; Save closes it.
- [ ] After Configure, the running shell does NOT restart (per design); restart manually via `exit` in the shell to verify the new config takes effect.
- [ ] Create a 4th tab: `Ctrl+4` switches to it.
- [ ] Create a 5th, 6th, 7th tab: `Ctrl+5`, `Ctrl+6`, `Ctrl+7` work.
- [ ] Close the 4th tab while it's active: focus moves to the 3rd. `Ctrl+5` now refers to the *new* 5th position (which was the 6th before).
- [ ] App restart: all user tabs disappear, only Claude / aider / Shell 1 remain. (This is correct — M3 adds persistence.)

## Done Criteria

- All ten "What This Milestone Delivers" items work end-to-end on Windows and Linux.
- All "Manual Verification Checklist" items pass on at least one machine per platform.
- No regression in M1 behavior (the hardcoded Shell 1 still works, restart-on-exit still works).
- No regression in v1.1 behavior for builtin tabs.
- `cargo test` passes.
