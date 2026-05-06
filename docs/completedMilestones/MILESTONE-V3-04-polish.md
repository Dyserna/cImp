# Milestone V3-04: Polish and Edge Cases

## Purpose

Round out v1.2 with the remaining items deferred from M1–M3: notification text interpolation, the restart-shell context menu action, notification-text editing UI for Shell tabs, cross-platform validation of shell auto-detection, and a final pass over the bottom status bar and shortcut wiring with N tabs in play.

This is the smallest of the four milestones in scope but the most validation-heavy. The work here ensures v1.2 ships in a state where a user with a fresh install on either Windows or Linux can productively use Shell tabs without surprises.

Read `DESIGN.md`, M1, M2, M3 first.

## What This Milestone Delivers

1. `{code}` placeholder interpolation in the `exited` notification text. The configured text is rendered with `{code}` replaced by the actual exit code at notification-fire time.
2. Notification text editing UI for Shell tabs in both the Configure Tab dialog and the Settings window's Tabs section. The fields are: `error` text and `exited` text, with a small help line documenting the `{code}` placeholder.
3. The right-click → `Restart shell` context menu action. Kills the running shell and respawns with the current configuration. Useful when a Shell tab's config has been changed and the user wants the change to take effect immediately without typing `exit`.
4. End-to-end cross-platform validation of shell auto-detection. Documented test results across the supported configurations.
5. Validation that the bottom status bar (mute, announcements, volume) continues to work correctly with N tabs.
6. Validation that the `Ctrl+1`..`Ctrl+9` shortcuts behave correctly with various tab counts and reorderings (closing tabs in the middle, etc.).
7. README updates documenting Shell tabs, default shells per platform, and how to configure an alternative shell.
8. A short troubleshooting section in the README for the most common Shell-tab issues (Git Bash not detected on Windows; `$SHELL` quirks on Linux).

## What This Milestone Does NOT Do

- No drag-to-reorder (out of scope for v1.2).
- No tab groups, splits, or panes (out of scope).
- No env var UI (out of scope; the schema field is preserved by M3 and remains editable via hand-editing settings.json).
- No profiles/templates for shell tabs (deferred to v1.3).
- No tab-bar overflow polish — when more tabs exist than fit, tabs become narrower; explicit overflow handling (scroll, dropdown) deferred.
- No automatic shell restart on crash — the closed overlay still requires user input.
- No additional notification placeholders beyond `{code}` (e.g., `{name}`, `{tab_position}`).

## Implementation Steps

### 1. `{code}` placeholder interpolation

In `src/notifications/mod.rs`, the function that resolves notification text for an event currently returns the configured string verbatim. Add interpolation at the resolution point:

```rust
fn resolve_text(template: &str, ctx: &NotificationContext) -> String {
    template.replace("{code}", &ctx.exit_code.map(|c| c.to_string()).unwrap_or_else(|| "?".to_string()))
}
```

The `NotificationContext` is constructed at the trigger site. For `exited` triggers, it carries the exit code from the `SubprocessExited` signal. For other triggers (which don't use `{code}`), the field is `None` and the placeholder is left as `?` if accidentally used in a non-exited notification (defensive — users shouldn't put `{code}` in `error` text but if they do, the result is graceful).

If the user's configured text contains no `{code}`, the replace is a no-op. No regression for existing AI-tab notifications (which don't use placeholders).

### 2. Notification text fields in the Configure dialog

Update `frontend/src/components/ShellTabFields.svelte` (the shared field component from M2) to also include:

- **Error notification text**: `<input type="text">` with default `"Shell encountered an error"`.
- **Exited notification text**: `<input type="text">` with default `"Shell exited (code {code})"`. Include a small help line under the field: "Use `{code}` to insert the exit code."

These fields populate from / write to the `notifications.error` and `notifications.exited` fields of the tab's `ShellTabConfig`. The Configure Tab dialog now passes these through `reconfigure_shell_tab` (extend the command's signature).

For the New Shell Tab dialog, both fields should be pre-filled with the platform-default text. If the user doesn't change them, defaults are used.

### 3. Notification text fields in the Settings window

Update `SettingsTabsSection.svelte` (from M3) to surface the same notification text fields when editing a Shell tab. Use the same `ShellTabFields.svelte` component as the Configure dialog.

For the AI-tab rows (Claude, aider), the v1.1 notification fields (`idle`, `awaiting_permission`, `error`) are already exposed under M3's relocated layout. M4 doesn't change those.

### 4. Restart-shell context menu action

In `frontend/src/components/TabContextMenu.svelte` (from M2), add a new entry to the user-Shell-tab menu: `Restart shell`. Order: `Rename`, `Configure...`, `Restart shell`, separator, `Close`.

Visibility: `Restart shell` is hidden when the tab is in closed state (the closed overlay's Enter affordance is the equivalent action) — alternatively, always show but no-op when closed. Hide-when-closed is the cleaner UX.

The menu entry calls a new (or existing — see step 5) Tauri command `restart_shell_tab(tab_id)`.

### 5. Backend `restart_shell_tab` (final form)

`restart_shell_tab` was introduced in M1 (closed-state Enter handler) and refined in M3 (reads config from settings). M4 extends it to also handle the case where the shell is *running*:

```rust
pub async fn restart_shell_tab(tab_id: TabId, state: ...) -> Result<(), RestartError> {
    let tab = state.find_tab(&tab_id)?;
    if !matches!(tab.kind, TabKind::Shell) {
        return Err(RestartError::WrongKind);
    }

    if !tab.closed {
        // Tab is running; kill the subprocess. The PTY reader task will then
        // emit SubprocessExited, which transitions the tab to closed state.
        tab_handle.kill_subprocess().await?;
        // Wait briefly for the closed transition (with a timeout).
        wait_for_closed(&tab_id, Duration::from_secs(2)).await?;
    }

    // Now the tab is closed; respawn.
    spawn_from_config(&settings.tabs.find_by_id(&tab_id)?, ...).await?;
    Ok(())
}
```

The two-phase approach (kill, wait, respawn) reuses M1's closed-state restart machinery. Don't try to do an in-place swap of the PTY — kill cleanly first.

### 6. Cross-platform shell auto-detection validation

Run the test matrix and document results in a comment in `src/shell/detect.rs`:

| Platform | Configuration | Expected default | Verified? |
|----------|---------------|------------------|-----------|
| Windows  | Git for Windows installed in `C:\Program Files\Git` | `C:\Program Files\Git\bin\bash.exe` | |
| Windows  | Git for Windows installed in `C:\Program Files (x86)\Git` (32-bit, rare) | `C:\Program Files (x86)\Git\bin\bash.exe` | |
| Windows  | Git for Windows installed elsewhere (e.g., `D:\dev\Git`) | Path read from registry | |
| Windows  | No Git for Windows; `bash.exe` on PATH from MSYS2 | The PATH-resolved bash | |
| Windows  | No Git, no MSYS2, no bash on PATH | `powershell.exe` with banner shown in dialog | |
| Linux    | `$SHELL=/bin/bash` (typical) | `/bin/bash -i` | |
| Linux    | `$SHELL=/usr/bin/zsh` | `/usr/bin/zsh -i` | |
| Linux    | `$SHELL` unset | `/bin/bash -i` | |
| Linux    | `$SHELL` set to non-existent path | `/bin/bash -i` (fallback) | |

Run each case manually. For the "elsewhere" Windows case, install Git in a non-default location and verify the registry probe finds it. For the "no Git" case, use a clean Windows VM or temporarily rename the install directory.

Fix any quirks discovered. Common likely issues:

- The registry value might be a `REG_SZ` with a trailing backslash; sanitize.
- WSL on Windows: not part of the *default* probe, but verify a user can manually configure a tab to use `C:\Windows\System32\wsl.exe` and have it work cleanly.

Document the matrix in a top-of-file comment in `src/shell/detect.rs` so future maintainers see what was tested.

### 7. Bottom status bar with N tabs

The bottom status bar is global (mute, announcements toggle, volume) and is not per-tab. It should work the same regardless of how many tabs exist. Verify:

- [ ] With 2 tabs (just builtins): status bar layout normal.
- [ ] With 5 tabs: layout normal, controls function.
- [ ] With 10 tabs: layout normal, tab bar may scroll/narrow but status bar is unaffected.
- [ ] When the active tab is a Shell tab in closed state: mute / announcements / volume still respond.

No code change expected unless a regression is found.

### 8. `Ctrl+1`..`Ctrl+9` validation

The shortcut layer binds to position 1..9. M2 wired the shortcuts in-session; M3 persisted them. M4 validates the behavior across the trickier scenarios:

- [ ] Switch via `Ctrl+1`..`Ctrl+9` with various tab counts (2, 5, 9, 10).
- [ ] Close a tab in the middle of the order (e.g., position 4 of 6): subsequent positions shift down. `Ctrl+5` after close should land on what was position 6.
- [ ] Create a new tab while a `Ctrl+N` shortcut targets a non-existent position: the shortcut starts working when the tab count grows to N.
- [ ] `Ctrl+9` with only 5 tabs: no-op, no error, no toast (silent).
- [ ] After M2's `Ctrl+W` closes the active tab: focus moves correctly; subsequent `Ctrl+N` shortcuts still target the right tabs.

Fix any rough edges found.

### 9. README updates

In the project README, add a new section: "Shell Tabs (v1.2+)". Cover:

- What Shell tabs are: configurable terminal sessions running alongside Claude and aider tabs.
- How to create one: `+` button or `Ctrl+T`.
- How to configure: right-click → Configure, or Settings → Tabs.
- Default shell behavior on each platform:
  - Windows: Git Bash (auto-detected). Fallback to PowerShell.
  - Linux: `$SHELL` env var.
- How to use an alternative shell: list common configurations with concrete command/args:
  - WSL (Windows): `wsl.exe`, args `-d Ubuntu` (or whichever distro)
  - PowerShell Core: `pwsh.exe`, args `-NoLogo`
  - cmd: `cmd.exe`, args `/K`
  - zsh on Linux: `/usr/bin/zsh`, args `-i`

### 10. Troubleshooting section in README

Brief section with:

- **"My Shell tab on Windows opens PowerShell, not Git Bash."** Cause: Git for Windows is not installed at a standard location and not on PATH. Solution: install Git for Windows from gitforwindows.org, or manually configure the Shell tab's command in the Configure dialog.
- **"The shell exits immediately when I create a new tab."** Cause: usually a quoting issue in the args field or a missing dependency. Solution: check the args for unquoted spaces; verify the command runs from a normal terminal first.
- **"Linux tools (grep, nano) aren't found in my Shell tab."** Cause on Windows: shell is PowerShell or cmd, which doesn't ship with these. Solution: switch to Git Bash or WSL via the Configure dialog.
- **"My Shell tab's config changes don't take effect."** Cause: changes apply on next shell restart. Solution: right-click → Restart shell, or type `exit` and press Enter on the closed overlay.
- **"How do I delete a Shell tab?"** Hover the tab and click the `×`, or `Ctrl+W`. (Mention that builtin tabs can't be closed.)
- **"My settings.json got corrupted."** v1.2 backs up the v1.1 file on first migration as `config.json.v1.1.bak`. For other corruption, delete settings.json and the app will write a fresh default.

## Files Touched / Added

**Modified:**
- `src/notifications/mod.rs` (`{code}` interpolation)
- `src/state/manager.rs` (`restart_shell_tab` extension for running shells)
- `src/shell/detect.rs` (registry quirks if found, top-of-file documentation comment with verified matrix)
- `src/ipc/mod.rs` (`reconfigure_shell_tab` extended signature for notification text)
- Frontend `ShellTabFields.svelte` (notification text fields)
- Frontend `TabContextMenu.svelte` (`Restart shell` entry)
- Frontend `ConfigureTabDialog.svelte` (passes notification text through)
- Frontend `NewShellTabDialog.svelte` (default notification text)
- Frontend `SettingsTabsSection.svelte` (notification text editing for Shell rows)
- README

**No new files added.**

## Edge Cases and Gotchas

- **`{code}` in `error` text**: defensive default to `?`. Don't crash.
- **`{code}` appears multiple times in the template**: `String::replace` handles all occurrences. No special handling needed.
- **`Restart shell` while the shell is in the middle of writing to disk**: the kill is forceful (the user asked for it). Document that uncommitted work in the shell is lost on restart, same as if the user typed `kill -9` from outside.
- **`Restart shell` on a Shell tab that's in "command not found" state**: the closed overlay's Enter handler routes to Configure (per M3). The right-click `Restart shell` should also route to Configure rather than try to respawn — same logic. Or: hide the menu entry in this state. Pick one and document.
- **The `Restart shell` command and the closed-overlay Enter both invoke `restart_shell_tab`**: ensure they don't race. If both fire (e.g., user clicks Restart while the Enter handler is mid-flight), the second call sees the tab as already restarting; reject with a `AlreadyRestarting` error or no-op silently.
- **Notification text with very long strings**: no truncation in v1.2. The TTS engine reads them out as configured. Document a soft recommendation: keep notification text under one sentence.
- **Notification text containing characters Kokoro misreads**: out of scope — user can audit their text. v1.2 doesn't sanitize.
- **`Ctrl+9` collides with browser/webview shortcuts**: in Tauri's webview, `Ctrl+9` may have a default browser binding (last tab). Verify it's overridden cleanly. If not, document the collision and let the user remap.

## Manual Verification Checklist

- [ ] `{code}` in the `exited` notification text is replaced with the actual exit code when the shell exits with a non-zero code.
- [ ] `{code}` is replaced when the shell exits with code 0.
- [ ] Notification text without `{code}` is unchanged.
- [ ] Configure Tab dialog: error and exited notification text fields are present, editable, persist across restart.
- [ ] New Shell Tab dialog: notification text fields default to platform defaults.
- [ ] Settings window → Tabs → edit a Shell tab: notification text fields visible and editable.
- [ ] Right-click on a running user Shell tab → Restart shell: shell restarts cleanly, prompt re-appears within ~2 seconds.
- [ ] Right-click on a closed user Shell tab: Restart shell entry is hidden (or disabled).
- [ ] All entries in the cross-platform shell auto-detection matrix verified at least once.
- [ ] `Ctrl+1`..`Ctrl+9` works correctly with 2, 5, and 9 tabs.
- [ ] `Ctrl+9` with only 3 tabs is a silent no-op.
- [ ] Closing a middle tab shifts subsequent positions correctly.
- [ ] Bottom status bar (mute, announcements, volume) functions identically with 2 tabs and 9 tabs.
- [ ] README has "Shell Tabs" section with platform defaults documented.
- [ ] README has Troubleshooting section covering at least the six scenarios above.

## Done Criteria

- All eight "What This Milestone Delivers" items work end-to-end.
- The cross-platform shell auto-detection matrix has been verified manually on at least one Windows machine and one Linux machine, with results documented in source.
- README documents Shell tab usage and platform defaults.
- No regression in v1.1, M1, M2, or M3 behavior.
- v1.2 ships from this point: a single binary on Windows and a single binary on Linux, both feature-complete per the v1.2 design.
