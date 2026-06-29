# Milestone V2-01: Multi-Tab Foundation

## Goal

Refactor the v1 single-tab architecture into a multi-tab architecture. Add a tab bar UI, generalize the PTY manager and processing layer to support multiple instances, refactor the state manager to be per-tab with an active-tab pointer, implement tab-switching logic with proper TTS handoff, migrate the settings schema, and add `Ctrl+1`/`Ctrl+2` shortcuts. v2 ships with two tabs (Claude Code and aider) — this milestone gets both functionally rendering and switchable, with the rest of v2's features (permission detection, notifications, status bar) deferred to later milestones.

## Why This Milestone First

Multi-tab support is the foundational change for v2. Every other v2 feature (permission detection, tab status indicators, notifications, the bottom status bar) depends on having a working multi-tab architecture. Doing the refactor first, ensuring both tabs work the same way they did in v1 individually, validates that the architectural change is sound before layering more complexity on top.

## Scope

### In Scope

- A tab bar UI at the top of the application window with two tabs: "Claude Code" and "Aider"
- Click-to-switch between tabs
- `Ctrl+1` / `Ctrl+2` keyboard shortcuts to switch tabs (configurable via the existing shortcuts system from v1)
- Per-tab PTY, processing layer, and xterm.js instance — both spawn at app launch (eager spawn)
- Both tabs functional independently: Claude Code in tab 1 (existing behavior preserved), aider in tab 2 (new, no TTS injection — aider runs with its default behavior)
- Active tab routing: only the active tab's xterm.js renders, only the active tab's PTY receives keyboard input, the avatar reflects only the active tab's state
- Tab switch behavior: stops audio playback, discards previously-active tab's pending TTS synthesis queue, activates new tab's terminal and state
- State manager refactor: per-tab `TabState`, an `active: TabId` pointer, signals tagged with their source tab
- Settings schema migration: v1's `claude_code` section becomes `tabs.claude`; new `tabs.aider` section added; notification text fields exist with defaults but are not yet acted on (no notification system in this milestone)
- The compose overlay continues to work; submitting from the compose sheet sends to whichever tab is currently active

### Out of Scope

- Permission detection and AwaitingPermission state (Milestone V2-03)
- Tab status indicators (Milestone V2-03)
- DoneWhileAway flag (Milestone V2-03)
- Notification system (Milestone V2-04)
- Bottom status bar (Milestone V2-04)
- TTS markup injection for aider (deferred per FUTURE-FEATURES.md; aider tab runs without injection in v2)
- Per-tab settings UI in the settings window (the schema is in place; the settings window UI for the new tab fields comes in Milestone V2-02)
- Lazy tab spawning (eager only)
- User-managed tabs / drag-to-reorder / closing tabs (v2 is fixed at two tabs)

## Acceptance Criteria

### Tab bar and switching

1. The application window shows a tab bar at the top of the window above the terminal area
2. Two tabs are visible in the bar: "Claude Code" (or just "Claude") and "Aider", in that order
3. The Claude Code tab is active by default at app launch
4. Clicking a tab switches to it: that tab's terminal becomes visible, that tab's avatar state drives the displayed avatar, keyboard input flows to that tab's PTY
5. Pressing `Ctrl+1` activates the Claude Code tab; `Ctrl+2` activates the aider tab. Both shortcuts work regardless of where focus is in the application.
6. The active tab's tab-bar entry is visually distinct from inactive tabs (e.g., highlighted background, brighter text, an indicator line)
7. Switching tabs is instant — no perceptible delay, no flicker
8. The terminal pane below the tab bar shrinks vertically by the tab bar's height; the avatar overlay's positioning is relative to the visible terminal area below the tab bar (not the window's outer edge)

### PTY and processing per tab

9. At app launch, both `claude` and `aider` are spawned as subprocesses, each in its own PTY, each in the launch directory
10. If `aider` is not installed or fails to launch, the aider tab displays an error message in its terminal area but the app continues to function for the Claude tab
11. Each tab's PTY processes output through its own processing layer (vte parser, flush state, tag detection)
12. Switching tabs does not interrupt either subprocess — they continue running in the background
13. Background tabs accumulate terminal output normally; switching to a background tab shows the up-to-date terminal state immediately

### State manager and avatar

14. Each tab has its own avatar state (Idle, Listening, Thinking, Speaking, Error)
15. The displayed avatar reflects only the active tab's state
16. Switching tabs immediately updates the avatar to reflect the newly-active tab's state
17. State signals from each tab affect only that tab's state (Claude generating output transitions only the Claude tab to Thinking, not the aider tab)

### TTS handoff on tab switch

18. When a tab is active and TTS is playing, switching to another tab immediately stops the audio
19. Pending TTS synthesis from the previously-active tab is discarded, not held for resumption
20. After switching, only the newly-active tab's TTS pipeline produces audio
21. Background tabs do not synthesize TTS — incoming `[[TTS]]` content from a background tab is detected by its processing layer but not sent for synthesis (or is sent and discarded; implementation can choose)

### Settings migration

22. On first launch with a v1 settings file: the v1 `claude_code` section is migrated to `tabs.claude` (specifically: `extra_cli_flags` is preserved, `claude_md_override` is dropped). New `tabs.aider` section is created with defaults.
23. The migrated settings file is written back, so subsequent launches see the v2 schema directly
24. On first launch with no settings file: defaults for both tabs are used and saved
25. The new `behavior.announcements_enabled` field defaults to `true` (will be acted on in Milestone V2-04, but the field exists in the schema now)

### Compose overlay compatibility

26. The compose overlay opens via the same shortcut as in v1
27. Submitting from the compose overlay sends the text to whichever tab is currently active (not always the Claude tab)
28. The compose overlay's "Listening" signal to the avatar state is correctly routed to the active tab's state

### Cross-platform

29. All of the above works on both Windows and Linux

## Implementation Approach

### Backend Refactor

The v1 backend was structured around a single `PtyManager`, `ProcessingLayer`, and `StateManager`. The v2 refactor:

```
src-tauri/src/
  pty/
    mod.rs           # public API
    manager.rs       # PtyManager — now multiple instances, one per tab
  processing/
    mod.rs
    layer.rs         # ProcessingLayer — multiple instances, one per tab
    parser.rs
    tag_detector.rs
    flush.rs
    segmenter.rs
  state/
    mod.rs
    manager.rs       # StateManager — single instance, tracks per-tab state
    types.rs         # TabId, TabState, AvatarState, StateSignal, StateEvent
  tabs/                  # NEW
    mod.rs
    registry.rs      # TabRegistry — owns the set of tabs and their components
    config.rs        # tab launch configuration
  audio/
    ...              # unchanged
  tts/
    ...              # unchanged
  ...
```

#### `TabId`

A simple identifier:

```rust
#[derive(Clone, Copy, Hash, Eq, PartialEq, Debug, Serialize, Deserialize)]
pub enum TabId {
    Claude,
    Aider,
}
```

For v2 this is a closed enum (only two values). For v3, this could become a `String` or a numeric ID for arbitrary tabs.

#### `TabRegistry`

A new module that owns the set of tabs and their per-tab components.

```rust
pub struct TabRegistry {
    tabs: HashMap<TabId, TabComponents>,
    active: TabId,
    state_manager: Arc<RwLock<StateManager>>,
    // shared resources (audio output, tts engine, settings store, signal channels)
}

pub struct TabComponents {
    pub pty: PtyManager,
    pub processing: ProcessingLayerHandle,  // a handle since the layer runs in its own task
    pub xterm_id: String,  // identifier for the frontend's xterm.js instance
}

impl TabRegistry {
    pub async fn new(
        settings: Settings,
        launch_cwd: PathBuf,
        invocation_args: Vec<String>,
        state_manager: Arc<RwLock<StateManager>>,
        // ... other shared resources
    ) -> Result<Self, AppError>;
    
    pub async fn activate(&mut self, tab: TabId) -> Result<(), AppError>;
    pub fn active(&self) -> TabId;
    pub async fn write_to_active(&self, bytes: &[u8]) -> Result<(), AppError>;
    pub async fn resize_active(&self, rows: u16, cols: u16) -> Result<(), AppError>;
    pub async fn shutdown(self) -> Result<(), AppError>;
}
```

`activate(tab)`:
1. Stops current audio playback (`audio_output.stop_all()`)
2. Drains and discards the previously-active tab's TTS text queue
3. Updates `self.active` to the new tab
4. Emits a `tab-activated` event to the frontend so it can swap the visible xterm.js instance
5. Emits a `StateSignal::TabActivated { tab }` to the state manager

#### Per-tab launch configuration

Each tab is launched based on its settings entry:

```rust
fn build_tab_command(tab_id: TabId, settings: &Settings, launch_cwd: &Path, invocation_args: &[String]) -> CommandBuilder {
    let tab_settings = match tab_id {
        TabId::Claude => &settings.tabs.claude,
        TabId::Aider => &settings.tabs.aider,
    };
    
    let mut cmd = CommandBuilder::new(&tab_settings.command);
    cmd.cwd(launch_cwd);
    
    // Persistent flags from settings
    for flag in &tab_settings.extra_cli_flags {
        cmd.arg(flag);
    }
    
    // TTS injection (if enabled and supported for this tab)
    if tab_settings.tts_injection.enabled {
        match tab_id {
            TabId::Claude => {
                // Claude supports --append-system-prompt
                cmd.arg("--append-system-prompt");
                cmd.arg(&tab_settings.tts_injection.instructions);
            }
            TabId::Aider => {
                // Aider has no equivalent flag; injection is a no-op for now
                // See FUTURE-FEATURES.md
                tracing::info!("aider tab: TTS injection enabled in settings but no aider CLI mechanism available; skipping injection");
            }
        }
    }
    
    // Invocation arguments (only for Claude in v2 — aider is its own command and v2 doesn't accept aider-specific
    // invocation args; user-provided arguments to cimp go to claude. This keeps backward compatibility
    // with v1's "cimp is a drop-in replacement for claude" promise.)
    if tab_id == TabId::Claude {
        for arg in invocation_args {
            cmd.arg(arg);
        }
    }
    
    cmd
}
```

The `invocation_args` decision: cimp continues to accept CLI arguments and forward them to the Claude subprocess only, preserving v1's drop-in-replacement behavior. The aider tab gets only its persistent settings flags, no invocation passthrough. This keeps the user's expectation stable: `cimp --resume <id>` still resumes a Claude session, just like v1.

#### State Manager refactor

```rust
pub struct StateManager {
    tabs: HashMap<TabId, TabState>,
    active: TabId,
    state_tx: broadcast::Sender<StateEvent>,
}

#[derive(Clone, Debug)]
pub struct TabState {
    pub avatar_state: AvatarState,
    pub awaiting_permission: bool,        // unused in this milestone, populated in V2-03
    pub done_while_away: bool,             // unused in this milestone, populated in V2-03
    pub claude_still_generating: bool,
}

impl StateManager {
    pub fn new(initial_active: TabId, state_tx: broadcast::Sender<StateEvent>) -> Self;
    pub async fn handle_signal(&mut self, signal: StateSignal);
    pub fn active_state(&self) -> AvatarState {
        self.tabs[&self.active].avatar_state
    }
    pub fn tab_state(&self, tab: TabId) -> Option<&TabState>;
}
```

The signal-handling logic is the same as v1's per-state-machine logic, just keyed by `signal.tab()`:

```rust
pub async fn handle_signal(&mut self, signal: StateSignal) {
    let tab_id = signal.tab();
    let tab_state = self.tabs.get_mut(&tab_id).expect("tab exists");
    let old_state = tab_state.avatar_state;
    let new_state = compute_transition(old_state, &signal, tab_state);  // existing v1 logic
    
    if new_state != old_state {
        tab_state.avatar_state = new_state;
        let _ = self.state_tx.send(StateEvent::StateChanged { tab: tab_id, state: new_state });
        tracing::info!("tab {:?} state: {:?} -> {:?}", tab_id, old_state, new_state);
    }
    
    // Special case: TabActivated signal
    if let StateSignal::TabActivated { tab } = signal {
        let prev = self.active;
        self.active = tab;
        let _ = self.state_tx.send(StateEvent::ActiveTabChanged { tab });
        // The frontend will swap avatar visuals based on the new active tab's state
    }
}
```

The frontend listens for state events and updates the avatar to match the active tab's state. When `ActiveTabChanged` is broadcast, the frontend re-renders with the new active tab's state.

### Audio and TTS coordination

The audio output module is shared across tabs (single output stream, single playback queue). The TTS synthesis pipeline becomes active-tab-aware:

```rust
pub struct TtsCoordinator {
    engine: TtsEngine,
    audio: Arc<AudioOutput>,
    per_tab_queues: HashMap<TabId, mpsc::Receiver<String>>,  // text to synthesize, per tab
    active: Arc<RwLock<TabId>>,
}

impl TtsCoordinator {
    pub async fn run(self) {
        // Loop: select on the active tab's queue
        // When the active tab changes, switch to draining a different queue
        // When the active tab changes, also clear the audio output queue
        loop {
            let active = *self.active.read().await;
            // Select-based wait on the active tab's queue
            // ... implementation detail
        }
    }
}
```

Implementation detail: tokio's `select!` with dynamic per-tab queue selection requires some care. One approach: each tab's processing layer sends TTS segments into a single shared channel tagged with `(tab, segment)`, and the coordinator filters to active-tab segments at the receiver side. Inactive-tab segments are dropped immediately. This is simpler than per-tab channels and avoids the dynamic selection problem.

```rust
pub enum TtsRequest {
    Synthesize { tab: TabId, text: String },
}

// Single shared channel; coordinator filters at receive time
loop {
    if let Some(req) = tts_rx.recv().await {
        match req {
            TtsRequest::Synthesize { tab, text } => {
                if tab == *self.active.read().await {
                    // Synthesize and play
                } else {
                    // Drop silently
                }
            }
        }
    }
}
```

This is cleaner. Use this pattern.

When a tab activation happens, additionally clear the audio queue:

```rust
pub async fn on_tab_activated(&self, new_tab: TabId) {
    *self.active.write().await = new_tab;
    self.audio.stop_all();  // stops current playback, clears queue
}
```

### Frontend Refactor

#### Tab bar component

```
src/lib/
  TabBar.svelte
  Tab.svelte
  tabs/
    state.ts        # tab-related Svelte stores
```

`TabBar.svelte` renders the list of tabs from a Svelte store, dispatches click events to switch tabs.

```svelte
<script lang="ts">
  import { activeTab, switchTab } from './tabs/state';
  import Tab from './Tab.svelte';

  const tabs = [
    { id: 'claude', label: 'Claude Code' },
    { id: 'aider', label: 'Aider' },
  ];
</script>

<div class="tab-bar">
  {#each tabs as tab}
    <Tab
      label={tab.label}
      active={$activeTab === tab.id}
      on:click={() => switchTab(tab.id)}
    />
  {/each}
</div>

<style>
  .tab-bar {
    display: flex;
    flex-direction: row;
    height: 32px;
    background: var(--tab-bar-bg, #2a2a2a);
    border-bottom: 1px solid var(--tab-bar-border, #444);
  }
</style>
```

`Tab.svelte` renders a single tab with active/inactive styling. Status indicators (working pulse, awaiting permission, etc.) come in Milestone V2-03.

#### Multiple xterm.js instances

The frontend now has multiple xterm.js instances, one per tab. Approach: render all of them, but only the active tab's container is visible (`display: block`); the rest are `display: none`.

```svelte
<!-- Terminal area in App.svelte -->
{#each tabs as tab}
  <div class="terminal-pane" class:hidden={$activeTab !== tab.id}>
    <Terminal tabId={tab.id} />
  </div>
{/each}

<style>
  .terminal-pane {
    width: 100%;
    height: 100%;
  }
  .hidden {
    display: none;
  }
</style>
```

`Terminal.svelte` is parameterized on `tabId` so each instance subscribes to events tagged with its tab and writes input back tagged correspondingly.

xterm.js instances retain their state (scrollback, cursor position) when hidden via `display: none`, so switching back to a tab shows the up-to-date terminal contents immediately.

#### Tab-aware IPC

The `pty_write` and `pty_resize` Tauri commands now take a tab ID parameter:

```rust
#[tauri::command]
async fn pty_write(state: State<'_, AppState>, tab: TabId, input: String) -> Result<(), String>;

#[tauri::command]
async fn pty_resize(state: State<'_, AppState>, tab: TabId, rows: u16, cols: u16) -> Result<(), String>;
```

Output events are tagged with their tab:

```rust
app.emit("pty-output", PtyOutputEvent { tab: TabId::Claude, bytes: ... });
```

Frontend xterm.js instances filter by the tab tag.

#### Switching shortcut handlers

The shortcut dispatcher gets two new handlers:

```typescript
configureShortcuts(settings.shortcuts, {
    open_compose: () => openCompose(),
    submit_compose: () => { /* ... */ },
    cancel_compose: () => { /* ... */ },
    open_settings: () => openSettings(),
    switch_to_tab_1: () => switchTab('claude'),
    switch_to_tab_2: () => switchTab('aider'),
});
```

`switchTab` emits a Tauri command to the backend to activate the tab, which goes through the `TabRegistry::activate()` path described above.

### Settings Migration

```rust
pub fn load_with_migration() -> Result<Settings, AppError> {
    let path = config_path()?;
    if !path.exists() {
        let defaults = Settings::default();
        save(&defaults)?;
        return Ok(defaults);
    }
    
    let contents = std::fs::read_to_string(&path)?;
    
    // Try parsing as v2 first
    if let Ok(s) = serde_json::from_str::<Settings>(&contents) {
        return Ok(s);
    }
    
    // Try parsing as v1 (with the old `claude_code` field)
    if let Ok(v1) = serde_json::from_str::<V1Settings>(&contents) {
        let migrated = migrate_v1_to_v2(v1);
        save(&migrated)?;  // overwrite with v2 schema
        tracing::info!("migrated settings from v1 to v2 schema");
        return Ok(migrated);
    }
    
    // Both failed: corrupt file, use defaults
    tracing::warn!("settings file unparseable; using defaults");
    let defaults = Settings::default();
    save(&defaults)?;
    Ok(defaults)
}

fn migrate_v1_to_v2(v1: V1Settings) -> Settings {
    let mut v2 = Settings::default();
    // Copy fields that exist in both
    v2.tts = v1.tts;
    v2.segmentation = v1.segmentation;
    v2.avatar = v1.avatar;
    v2.display = v1.display;
    v2.behavior.interrupt_on_input = v1.behavior.interrupt_on_input;
    v2.behavior.auto_speak = v1.behavior.auto_speak;
    v2.behavior.fallback_silent = v1.behavior.fallback_silent;
    // announcements_enabled gets the default (true)
    v2.compose = v1.compose;
    v2.shortcuts.open_compose = v1.shortcuts.open_compose;
    v2.shortcuts.submit_compose = v1.shortcuts.submit_compose;
    v2.shortcuts.cancel_compose = v1.shortcuts.cancel_compose;
    v2.shortcuts.open_settings = v1.shortcuts.open_settings;
    // switch_to_tab_1 and switch_to_tab_2 get defaults
    v2.processing = v1.processing;
    
    // Migrate claude_code -> tabs.claude
    v2.tabs.claude.extra_cli_flags = v1.claude_code.extra_cli_flags;
    // claude_md_override is dropped intentionally (no longer the injection mechanism)
    
    // tabs.aider gets all defaults
    
    v2
}
```

`V1Settings` is a Rust type matching the v1 schema, used only for migration. After migration, all subsequent loads use the v2 schema directly.

## Validation Steps

### Tab bar and switching

1. **Both tabs visible**: launch the app. Verify the tab bar shows two tabs ("Claude Code" and "Aider") at the top of the window.
2. **Default active**: verify Claude Code tab is highlighted as active and its terminal is visible.
3. **Click to switch**: click the Aider tab. Verify the terminal area swaps to show aider's content; the avatar updates to reflect aider's state (likely Idle initially).
4. **Click back**: click the Claude Code tab. Verify the terminal swaps back to Claude's content with all of its scrollback intact.
5. **Shortcut switching**: press `Ctrl+1` and `Ctrl+2`. Verify each switches to the corresponding tab. Verify the shortcuts work regardless of where focus is (terminal, compose overlay, etc.).
6. **Tab bar visual feedback**: hover over inactive tabs; verify a hover effect. Verify active tab is distinct from hovered-but-not-active.

### PTY behavior

7. **Both subprocesses spawned**: at app launch, check the OS process list. Verify both `claude` and `aider` are running.
8. **Both in launch directory**: in each tab, verify the working directory matches where cimp was launched.
9. **Aider not installed**: temporarily rename the `aider` binary (or set the path to something invalid). Launch cimp. Verify the Claude tab still works and the aider tab shows an error or empty terminal without crashing the app.
10. **Background output**: send a long-running command to the Claude tab, then switch to aider, wait for Claude to finish in the background, then switch back. Verify Claude's full output is visible (not truncated or lost).

### TTS handoff

11. **Mid-speech tab switch**: in the Claude tab, trigger a long TTS response. While audio is playing, switch to the Aider tab. Verify the audio stops immediately (no audible tail).
12. **No queue resumption**: switch back to Claude. Verify no audio resumes from the previously-cut response (only new content from Claude after the switch back would speak).
13. **Aider tab silent**: in the Aider tab, send a message and trigger aider's response. Verify no TTS plays (since aider doesn't emit `[[TTS]]` tags). The avatar should still move through Listening / Thinking / Idle states based on aider's activity, just without Speaking.

### State and avatar

14. **Per-tab states**: type in the Claude tab; verify Claude's avatar state changes to Listening. Switch to aider without affecting state. Verify aider's tab independently has its own state.
15. **Avatar reflects active tab**: when switching tabs, verify the avatar visual updates to reflect the newly-active tab's state.

### Settings migration

16. **From v1 settings file**: place a v1 settings file (with `claude_code` section) at the config path. Launch the app. Verify the file is rewritten in v2 schema, with `tabs.claude.extra_cli_flags` populated from the old `claude_code.extra_cli_flags`. Verify the app uses the migrated settings correctly (e.g., if the v1 file had a custom font size, it's still applied).
17. **Fresh install**: delete the settings file. Launch. Verify a v2 schema file is created with defaults.
18. **Defaults**: verify `behavior.announcements_enabled` defaults to true, `tabs.claude.tts_injection.enabled` defaults to true, `tabs.aider.tts_injection.enabled` defaults to false, `shortcuts.switch_to_tab_1` defaults to `Ctrl+1` and `switch_to_tab_2` to `Ctrl+2`.

### Compose overlay

19. **Compose targets active tab**: open the compose overlay (Ctrl+Shift+E). Type a message. Switch tabs. Submit the compose overlay. Verify the message went to the *currently active* tab, not the one that was active when the overlay was opened.
20. **Compose listening signal goes to active tab**: open the compose overlay and type. Verify the active tab's avatar transitions to Listening. Switch tabs while compose is still open with content. Verify the previously-active tab returns to its prior state, and the newly-active tab transitions to Listening (or accept that this case is messy and document as a known small quirk; either is fine).

### Cross-platform

21. Verify all of the above on the second target platform.

## Known Risks and Mitigation

- **xterm.js memory growth**: keeping multiple hidden xterm.js instances alive consumes memory. Each instance buffers scrollback. With two tabs this is fine; for v3 with more tabs it's worth profiling. v2 doesn't need to address this.
- **Audio stop-all latency**: rodio's `Sink::clear()` is fast but not instantaneous; there might be a brief audible artifact (a "click" or partial buffer playthrough) on tab switch during speech. Acceptable; if intrusive, look at the audio buffer size or add a small fade-out.
- **TTS dropping inactive tabs**: the design says to drop inactive-tab synthesis requests. Verify this doesn't cause a memory leak (synthesis requests piling up before they're filtered). The single-shared-channel approach with filter-at-receive avoids this.
- **Settings migration edge cases**: a v1 file might be missing fields, have extra fields, or be partially corrupt. The migration logic should handle "v1 fields that exist" without requiring "all v1 fields exist." `serde(default)` should handle this, but verify with a few test files.
- **Aider failing to spawn**: handle gracefully — show an error in the aider tab's terminal, don't crash the app. The user can install aider and restart later. Consider adding a "Retry" option in the aider tab when spawn fails (out of scope for this milestone, but flag if it comes up).
- **xterm.js focus management with hidden instances**: when a tab is hidden via `display: none`, it can't receive focus or input naturally. When the user clicks back to it, focus needs to be programmatically restored. xterm.js has a `focus()` method; call it on tab activation.
- **Tab bar squeezing the terminal**: 32px of vertical space is consumed by the tab bar. The avatar overlay needs to be positioned relative to the visible terminal area, not the window outer edge, or it will overlap the tab bar. The avatar's CSS positioning should be relative to the terminal container, not the window root.

## What "Done" Looks Like

The app launches with two tabs visible. Both tabs work — Claude Code in tab 1 behaves identically to v1, aider in tab 2 lets you have a normal aider session (just without spoken TTS for its output). Switching tabs is instant and clean. Avatar reflects the active tab. Settings persist correctly with the migrated schema. The architecture is in place for permission detection (V2-03) and notifications (V2-04) to layer on top.

The app should not feel different from v1 when using the Claude tab — same TTS, same avatar, same compose overlay. The new dimension is that you can also switch to aider when you want to use it.

---

## Next Milestone

Milestone V2-02: Aider Tab. Adds aider-specific polish — settings UI for the per-tab fields, error handling for aider not being installed, documenting the TTS limitation in the README, and per-tab settings sections in the settings window.
