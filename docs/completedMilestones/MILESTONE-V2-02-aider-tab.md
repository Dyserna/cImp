# Milestone V2-02: Aider Tab

## Goal

Polish the aider tab specifically: add settings window UI for per-tab configuration, improve error handling when aider isn't installed or fails to launch, document the TTS limitation clearly in user-facing materials, and ensure the per-tab settings schema is fully wired through the UI. The functional plumbing for aider already exists in V2-01; this milestone makes it production-ready and discoverable.

## Why This Milestone Now

V2-01 made multi-tab work end-to-end but kept settings-window UI minimal — only the schema was added, with no controls in the settings window for the new per-tab fields. This milestone closes that gap, plus addresses the user-facing edge cases (aider not installed, surprising TTS silence) so users have a smooth experience.

## Scope

### In Scope

- A new "Tabs" section in the settings window with sub-sections for each tab (Claude Code, Aider)
- Per-tab settings UI: command (read-only display showing what binary will be spawned), persistent CLI flags, TTS injection toggle and instructions text, per-event notification text fields (the notification logic itself comes in V2-04, but the configuration UI exists now)
- A "Restart Tab" button per tab to relaunch that tab's subprocess after settings changes that require restart (CLI flags, TTS injection settings)
- Improved aider tab error handling: when aider fails to spawn (binary not found, version mismatch, etc.), display a clear in-tab error message with diagnostic info and a Retry button
- README updates documenting the aider tab, the TTS limitation, and where to find FUTURE-FEATURES.md
- A small in-app first-launch notice for the aider tab if it has never been activated, briefly explaining its limitations and pointing to documentation

### Out of Scope

- The notification system itself (V2-04) — only the configuration UI for it is in this milestone
- Permission detection (V2-03)
- Tab status indicators (V2-03)
- TTS markup injection for aider (deferred per FUTURE-FEATURES.md)
- Aider permission patterns (V2-03 will start with Claude only; aider patterns are a later effort)
- A proper tutorial or onboarding flow (the in-app notice is a one-time tip, not a full tutorial)

## Acceptance Criteria

### Settings window — Tabs section

1. The settings window has a new "Tabs" section listed in its navigation (alongside TTS, Avatar, Waveform, Display, Behavior, Compose, Shortcuts, Claude Code [removed], Processing). Note that the v1 "Claude Code" section is removed since per-tab settings now live in "Tabs".
2. The Tabs section contains two sub-sections: "Claude Code" and "Aider"
3. Each tab sub-section shows:
   - **Command** (read-only display): "claude" for the Claude tab, "aider" for the aider tab. Not editable in v2.
   - **Persistent CLI flags**: a text-list editor (one flag per line, or a dynamic add/remove array UI). User can add or remove flags that always pass to the tab's subprocess.
   - **TTS injection enabled** (toggle): on/off. For Claude, toggling this controls whether `--append-system-prompt` is passed. For Aider, toggling this is currently a no-op (aider has no CLI mechanism), but the toggle is visible so users can understand what's happening and the toggle is ready for the day aider gets the feature.
   - **TTS injection instructions** (multi-line text area): the content that gets injected when injection is enabled. Pre-populated with sensible defaults for each tab. User can edit. Disabled when the injection toggle is off.
   - **Notification texts** (form with one row per event): three rows, one each for "When tab becomes idle while you're on another tab", "When tab requests permission while you're on another tab", "When tab encounters an error while you're on another tab". Each row has a text input with default text and a "Reset to default" button per row.
4. A "Restart Tab" button at the bottom of each sub-section restarts that tab's subprocess immediately. This is needed because changes to CLI flags, command, or TTS injection require relaunching the subprocess to take effect.
5. Each tab sub-section has a "Restart Required" indicator that appears next to fields whose changes require restart (CLI flags, TTS injection settings). The indicator clears when the user clicks Restart Tab and the relaunch completes.
6. Notification text changes apply live (no restart needed) — they affect the next notification event.
7. All changes persist to the JSON settings file (debounced) and reload correctly across app restarts.

### Aider tab error handling

8. When aider fails to spawn at app launch (binary not found, executable error, etc.), the aider tab's terminal area displays a clear error message. The message includes:
   - "Aider failed to start." headline
   - The actual error from the spawn attempt (e.g., "Error: aider: command not found")
   - A hint: "Make sure aider is installed and on your PATH. Visit https://aider.chat for installation instructions."
   - A "Retry" button that re-attempts the spawn
9. The Claude tab continues to work normally even if aider fails to spawn
10. The Retry button, when clicked, re-attempts the spawn. If successful, the error message is replaced by aider's normal output. If it fails again, the error message is updated with the new error.
11. If aider's process exits unexpectedly during a session (rather than at startup), the error state is similar: a message in the terminal explaining the exit, with a Retry button.
12. The avatar's Error state correctly fires for the aider tab when its subprocess fails or exits unexpectedly.

### First-launch notice for aider tab

13. The very first time the user activates the aider tab in the app's lifetime (tracked persistently — not per-session), a one-time notice is shown overlaid on the aider tab's terminal area. The notice contains:
    - "About the Aider Tab" headline
    - A brief paragraph: aider runs as a separate AI assistant in this tab. Spoken TTS output is currently limited because aider does not yet support system prompt injection via CLI. Tab status indicators, notifications, and visual feedback all work normally.
    - A link/reference to FUTURE-FEATURES.md or the project's README for more details
    - A "Got it" button to dismiss
14. The notice is shown only once. After dismissal, a flag is persisted (in settings or a separate state file) so the notice doesn't reappear on subsequent launches.
15. The notice doesn't block aider's underlying terminal — once dismissed, the user is in a normal aider session.

### README

16. The project README is updated to reflect v2:
    - Mention the multi-tab architecture (Claude Code + aider)
    - Mention the aider TTS limitation explicitly with a brief explanation
    - Reference FUTURE-FEATURES.md for the action plan if/when aider adds the relevant flag
    - Update setup instructions to mention aider as an additional dependency for v2 (clarifying it's optional — Claude tab works without it; aider tab will show an error if aider isn't installed but the rest of the app is unaffected)
    - Document the new shortcuts (Ctrl+1 / Ctrl+2)

### Cross-platform

17. All of the above works on both Windows and Linux. The aider error message and Retry behavior should work identically on both platforms.

## Implementation Approach

### Settings Window UI

#### Tabs section navigation

The settings window's section list (sidebar or top tabs, depending on the v1 implementation) gets a new "Tabs" entry. Inside, a sub-navigation distinguishes Claude vs Aider sub-sections. Two ways to lay this out:

- **Inline collapsible**: both Claude and Aider sub-sections rendered in a single Tabs panel, each as a collapsible group with a header. User can expand/collapse each.
- **Sub-tabs**: a row of buttons inside the Tabs section ("Claude" / "Aider") that switches which sub-section's form is visible.

For two tabs, inline collapsible is simpler and more discoverable (you can see both at once, scroll between them). Use inline collapsible. For v3 with potentially more tabs, sub-tabs scale better — revisit then.

#### Per-tab form

Each tab sub-section is a Svelte component that takes the tab's settings slice as a prop and renders the form:

```svelte
<!-- TabSettingsSection.svelte -->
<script lang="ts">
  import { createEventDispatcher } from 'svelte';
  import type { TabSettings } from '../types';
  
  export let tabId: 'claude' | 'aider';
  export let displayName: string;
  export let settings: TabSettings;
  export let restartRequired: boolean = false;
  
  const dispatch = createEventDispatcher();
  
  function update() {
    dispatch('change', settings);
  }
  
  function restart() {
    dispatch('restart');
  }
</script>

<div class="tab-settings">
  <h3>{displayName}</h3>
  
  <div class="field">
    <label>Command</label>
    <input type="text" value={settings.command} disabled readonly />
  </div>
  
  <div class="field">
    <label>Persistent CLI flags <span class="restart-flag" class:visible={restartRequired}>(restart required)</span></label>
    <ArrayEditor bind:items={settings.extra_cli_flags} on:change={update} />
  </div>
  
  <div class="field">
    <label>
      <input type="checkbox" bind:checked={settings.tts_injection.enabled} on:change={update} />
      TTS markup injection enabled <span class="restart-flag" class:visible={restartRequired}>(restart required)</span>
    </label>
    {#if tabId === 'aider' && settings.tts_injection.enabled}
      <p class="warning">Note: aider does not currently support system prompt injection via CLI. This setting is preserved for future use; injection has no effect today. See FUTURE-FEATURES.md.</p>
    {/if}
  </div>
  
  <div class="field">
    <label>TTS markup instructions</label>
    <textarea
      bind:value={settings.tts_injection.instructions}
      disabled={!settings.tts_injection.enabled}
      on:input={update}
      rows="6"
    />
    <button on:click={() => { settings.tts_injection.instructions = defaultInstructions(tabId); update(); }}>
      Reset to default
    </button>
  </div>
  
  <div class="field">
    <label>Notifications</label>
    <NotificationEditor bind:notifications={settings.notifications} tabId={tabId} on:change={update} />
  </div>
  
  <div class="field">
    <button on:click={restart} disabled={!restartRequired}>Restart {displayName}</button>
  </div>
</div>
```

`ArrayEditor` is a small reusable component for editing a list of strings (one flag per line, with add/remove buttons).

`NotificationEditor` is a small component rendering three labeled text inputs for the three notification events, with a "Reset" button per input.

#### Restart-required tracking

Some fields require the tab's subprocess to be restarted to take effect:

- `command` (not editable in v2 anyway)
- `extra_cli_flags`
- `tts_injection.enabled`
- `tts_injection.instructions`

Other fields apply live:

- `notifications.*`

When a "restart required" field changes, the parent settings component sets a flag for that tab. The restart-required indicator shows next to the relevant fields and the "Restart Tab" button becomes enabled. Clicking it invokes a backend Tauri command to restart that tab's subprocess and clears the flag.

```rust
#[tauri::command]
async fn restart_tab(state: State<'_, AppState>, tab: TabId) -> Result<(), String> {
    let settings = state.settings.read().await;
    state.tab_registry.lock().await.restart_tab(tab, &settings).await
        .map_err(|e| e.to_string())
}
```

The `TabRegistry::restart_tab` method:
1. Stops the existing subprocess for that tab
2. Kills its processing layer task
3. Clears its xterm.js instance content (sends a `clear-terminal` event to the frontend for that tab's xterm.js)
4. Spawns a fresh subprocess with current settings
5. Connects the new subprocess to the same xterm.js instance

The user's session in that tab is reset (history is gone). Worth flagging in the UI so the user understands clicking Restart loses their conversation. A confirmation dialog ("Restart will reset the session. Continue?") is reasonable; for v2 keep it simple — show a tooltip or hint near the button rather than a modal.

### Aider error handling

#### Spawn failure at app launch

The PTY manager's `spawn` method already returns a `Result`. When it fails for the aider tab, instead of bubbling the error up and crashing the app, the `TabRegistry` catches it and creates a "stub" tab that displays the error in xterm.js:

```rust
match PtyManager::spawn(launch_cwd.clone(), tab_id, &settings).await {
    Ok(pty) => { /* normal setup */ }
    Err(e) => {
        tracing::warn!("failed to spawn {:?}: {}", tab_id, e);
        // Create a stub: no PTY, but an xterm.js instance that shows an error
        let error_msg = format_aider_error(&e);
        app_handle.emit(&format!("tab-error-{:?}", tab_id), error_msg).ok();
        state_manager.handle_signal(StateSignal::SubprocessExited { tab: tab_id }).await;
    }
}
```

The frontend's xterm.js instance for that tab listens for `tab-error-{tabid}` events and writes the error message to its terminal:

```typescript
listen(`tab-error-${tabId}`, (event) => {
    const message = event.payload as string;
    term.clear();
    term.writeln('\x1b[31m' + message + '\x1b[0m');  // red error text
    term.writeln('');
    term.writeln('Press the Retry button below or restart cimp after fixing the issue.');
    showRetryButton.set(true);  // trigger UI to render retry button
});
```

Below the xterm.js terminal area for that tab, when `showRetryButton` is true, a retry button is rendered:

```svelte
{#if $showRetryButton}
  <div class="retry-overlay">
    <button on:click={retryTabSpawn}>Retry</button>
  </div>
{/if}
```

`retryTabSpawn` invokes a backend command:

```rust
#[tauri::command]
async fn retry_tab_spawn(state: State<'_, AppState>, tab: TabId) -> Result<(), String> {
    state.tab_registry.lock().await.retry_spawn(tab).await
        .map_err(|e| e.to_string())
}
```

The retry path is essentially the same as the initial spawn: try, on success connect everything, on failure write the new error to the terminal and keep the retry button visible.

#### Mid-session subprocess exit

If aider exits unexpectedly mid-session (e.g., crashes), the existing PTY exit handling kicks in: `SubprocessExited` signal fires, the tab transitions to Error state. The frontend sees this and writes a similar error message to the terminal with a retry option.

#### Error message formatting

```rust
fn format_aider_error(e: &AppError) -> String {
    let mut msg = String::from("Aider failed to start.\n\n");
    msg.push_str(&format!("Error: {}\n\n", e));
    msg.push_str("Make sure aider is installed and on your PATH.\n");
    msg.push_str("Installation instructions: https://aider.chat\n");
    msg
}
```

Different error categories may warrant different hints:

- "command not found": "Aider does not appear to be installed."
- Permission errors: "cimp could not execute aider — check file permissions."
- Other: generic "Failed to start aider:" with the raw error

For v2, just one general format with the error included is fine. Refine later if specific errors come up frequently.

### First-launch notice

Track first activation in settings:

```json
"tabs": {
  "aider": {
    ...
    "first_launch_notice_dismissed": false
  }
}
```

When the user activates the aider tab and `first_launch_notice_dismissed` is false, the frontend shows a modal-style notice over the tab's content area:

```svelte
<!-- AiderFirstLaunchNotice.svelte -->
<script lang="ts">
  import { settingsStore, updateSettings } from '../settings/store';
  
  export let onDismiss: () => void;
  
  function dismiss() {
    const settings = $settingsStore;
    settings.tabs.aider.first_launch_notice_dismissed = true;
    updateSettings(settings);
    onDismiss();
  }
</script>

<div class="notice-overlay">
  <div class="notice-card">
    <h3>About the Aider Tab</h3>
    <p>
      This tab runs aider, an alternative AI coding assistant. The tab's status indicators,
      notifications, and visual feedback all work normally.
    </p>
    <p>
      Spoken TTS output is limited in this tab because aider does not yet support
      system prompt injection via CLI. When that feature lands upstream, cimp will
      automatically use it. See FUTURE-FEATURES.md for details.
    </p>
    <button on:click={dismiss}>Got it</button>
  </div>
</div>
```

The overlay has a subtle backdrop and a centered card. It dismisses on button click and never reappears.

### README updates

Update the project README to include:

- A "v2: Multi-tab support" section noting the addition of the aider tab
- A subsection on the aider TTS limitation, with the same explanation as the first-launch notice
- A link to FUTURE-FEATURES.md
- Setup instructions: aider is now an optional additional dependency
- Updated shortcut documentation including Ctrl+1 / Ctrl+2

The README content should match the FUTURE-FEATURES.md framing — be honest about the limitation, explain why, and point to the action plan.

## Validation Steps

### Settings UI

1. **Tabs section visible**: open the settings window. Verify a Tabs section is in the navigation. Click it.
2. **Both sub-sections rendered**: verify both Claude and Aider sub-sections are visible, each with their own form.
3. **Editing CLI flags**: add a flag to Claude's persistent flags. Verify "Restart Required" indicator appears. Click "Restart Tab". Verify Claude's terminal clears and the new subprocess starts (with the flag in effect — verify by querying it inside Claude).
4. **TTS injection toggle (Claude)**: turn off Claude's TTS injection, restart the tab. Trigger a response. Verify no `[[TTS]]` tags appear (since the system prompt no longer instructs Claude to use them) and no audio plays.
5. **TTS injection toggle (Aider)**: turn the aider TTS injection toggle on. Verify the warning message appears. Toggle off. Verify the warning disappears.
6. **Editing notification text**: change the Claude tab's idle notification text. Save. Verify the text persists in the settings JSON file. (Notification firing is V2-04; this validates only that the configuration UI works.)
7. **Reset to default**: click "Reset to default" on a notification field. Verify the default text is restored.
8. **Restart Tab button enables on relevant changes only**: change a notification text. Verify Restart Tab is NOT enabled. Change a CLI flag. Verify it IS enabled.

### Aider error handling

9. **Aider not installed**: rename or remove the `aider` binary so it's not on PATH. Launch cimp. Verify the Claude tab works normally and the aider tab shows the error message with installation hint and Retry button.
10. **Retry**: install aider (or restore the binary). Click Retry on the aider tab. Verify aider starts normally and the error message is replaced by aider's startup output.
11. **Mid-session exit**: while aider is running, kill its process via task manager / `kill`. Verify the aider tab transitions to Error state, terminal shows an exit message, Retry button appears. Click Retry. Verify aider restarts.
12. **Claude tab unaffected**: throughout aider failures, verify the Claude tab continues to work without disruption.

### First-launch notice

13. **Notice on first activation**: with a fresh settings file (delete it), launch the app. Stay on the Claude tab and use it for a while. Verify no notice appears on the Claude tab. Click the Aider tab for the first time. Verify the notice overlay appears.
14. **Dismiss**: click "Got it" on the notice. Verify the overlay disappears. The aider terminal is now visible underneath.
15. **Persistence**: close the app. Relaunch. Click the Aider tab. Verify the notice does NOT reappear.

### README

16. **README content**: read through the README. Verify it covers the new architecture, the aider limitation, the new shortcuts, and links to FUTURE-FEATURES.md.

### Cross-platform

17. Verify all of the above on the second platform.

## Known Risks and Mitigation

- **Tab restart loses session**: clicking Restart Tab kills aider mid-conversation. Users may not realize this. Mitigation: a tooltip or hint near the Restart button. If users complain, add a confirmation dialog later.
- **First-launch notice timing**: if the user activates the aider tab very briefly (clicks accidentally), they might dismiss the notice without reading it. Acceptable — it's not critical information, just helpful context. The README has the same content.
- **Notice persistence across reinstalls**: the dismissal flag is in settings, which is in the user's config directory. A clean reinstall won't show the notice again as long as the config persists. If the user fully wipes settings, they'll see it again — fine.
- **TTS injection toggle confusion (aider)**: a user might toggle the aider injection on, expect TTS to start working, then be confused when nothing changes. Mitigation: the warning message under the toggle. If users are still confused, consider hiding the toggle entirely for aider until the upstream feature lands.
- **Error message verbosity**: showing the raw error text could be confusing for non-technical users. Acceptable for the target user (technical, comfortable in terminals). If broader audience is ever a goal, format errors more friendly.

## What "Done" Looks Like

The settings window has a clean Tabs section where users can configure both tabs. Aider failures are handled gracefully with informative messages and easy retry. Users discovering the aider tab for the first time get a helpful one-time explanation of what to expect. The README accurately describes v2 and the aider limitation. The architecture is fully ready for permission detection (V2-03) and notifications (V2-04) to be added.

---

## Next Milestone

Milestone V2-03: Permission Detection and Tab Status. Adds exact-string matching for Claude Code's permission prompts, the AwaitingPermission state, and tab status indicator rendering (Working, AwaitingPermission, Error, DoneWhileAway).
