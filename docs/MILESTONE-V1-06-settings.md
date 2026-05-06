# Milestone 6: Settings

## Goal

Bring the settings window to life. Implement the settings schema, JSON persistence, the broadcast mechanism for live updates, and the actual settings window UI. Wire every previously-hardcoded value to the settings store so it can be changed at runtime — TTS voice/speed/volume/mute, avatar images and transition, avatar position/size/margin/opacity/visibility, waveform appearance, terminal font, processing layer timings, behavior toggles, and keyboard shortcuts.

## Why This Milestone Now

By this point, all functional pieces except the compose overlay are in place but most parameters are hardcoded. This milestone makes the application configurable. It comes before the compose overlay (Milestone 7) because the compose overlay needs the shortcuts infrastructure that this milestone establishes.

## Scope

### In Scope

- A `settings` module in the Rust backend implementing:
  - The settings schema as a Rust struct with serde
  - JSON load on app startup from the OS-appropriate config directory
  - Debounced JSON save when changes occur (~500ms after last change)
  - A broadcast channel that propagates changes to subscribers
- All previously-hardcoded values bound to the settings store, with subscribers updating their behavior on change
- A separate Tauri window for settings, opened when the gear icon in the avatar pane is clicked OR when the `open_settings` shortcut is pressed
- A form-based settings UI grouped by category (TTS, Avatar, Waveform, Display, Behavior, Compose, Shortcuts, Claude Code, Processing)
- Live updates for everything except Claude Code subprocess parameters (which require restart and are flagged in the UI)
- Color picker for waveform color
- File picker for avatar image paths (per state)
- File picker for the shared transition asset path, with a duration field; leaving the path empty disables transitions
- Avatar layout controls: width, height (independent), position (4-corner dropdown), margin, opacity slider (range 30%–100%), visibility toggle
- Voice dropdown populated from available Kokoro voices
- Shortcuts capture UI for `open_compose`, `submit_compose`, `cancel_compose`, `open_settings`
- A "Restart Required" notice for changes that require relaunching Claude Code, with a button to do so
- The `open_settings` keyboard shortcut works at any time when the main window has focus, opening the settings window

### Out of Scope

- Migration logic for settings schema changes (the schema is v1)
- Per-user profiles or multiple settings sets
- Importing/exporting settings (could be a polish task in Milestone 8)
- Audio device selection in settings (deliberately deferred to post-v1)
- Per-state transition configuration (only a single shared transition is supported)
- Compose overlay UI itself (Milestone 7)
- The `submit_compose` and `cancel_compose` shortcuts only become functional in Milestone 7, but the settings UI for them is built here

## Acceptance Criteria

### General settings infrastructure

1. Clicking the gear icon in the avatar pane opens a settings window
2. Pressing the configured `open_settings` shortcut also opens the settings window
3. The settings window has organized sections for TTS, Avatar, Waveform, Display, Behavior, Compose, Shortcuts, Claude Code, and Processing
4. Settings persist across app restarts (saved to JSON, loaded at startup)
5. If the settings file is missing or corrupt at startup, defaults are used and a clean settings file is written
6. Closing the settings window leaves all changes applied and saved

### TTS

7. Changing the TTS voice immediately uses the new voice for the next synthesis
8. Changing TTS speed immediately affects subsequent synthesis
9. Changing the volume slider immediately affects audio output
10. Toggling mute immediately silences/unsilences audio

### Avatar

11. Selecting a new avatar image for any state immediately updates the displayed image when that state is active
12. Selecting a new transition asset path takes effect on the next state change; clearing the path causes subsequent state changes to snap directly with no transition
13. Changing the transition duration is applied to the next time the transition plays
14. Changing the avatar width or height immediately resizes the avatar overlay
15. Changing the avatar position (corner) immediately repositions the avatar overlay
16. Changing the avatar margin immediately adjusts the spacing from the corner edges
17. Changing the avatar opacity slider immediately updates the rendered opacity (and the toggle button's opacity)
18. Toggling avatar visibility immediately hides/shows the avatar (matching the toggle button behavior)
19. Avatar visibility persists across app restarts — if hidden when the app closes, it stays hidden on next launch

### Waveform

20. Changing the waveform color updates the visualizer in real time during Speaking state
21. Changing the waveform line width, glow intensity, or opacity updates the visualizer immediately
22. The waveform's opacity remains independent of the avatar's opacity (changing one does not affect the other)

### Display

23. Changing the terminal font family or size immediately updates the terminal rendering

### Behavior

24. Toggling "interrupt on input" changes whether typing during TTS playback stops the audio

### Processing

25. Changing the stability timeout or max hold values updates the processing layer's flush timing on the fly

### Shortcuts

26. The Shortcuts section displays each configurable shortcut with a "click to capture" field
27. Clicking a shortcut field, then pressing a key combination, captures and saves that shortcut
28. The captured shortcut format displays clearly (e.g., `Ctrl+Shift+E`)
29. The new shortcut takes effect immediately (the next time the trigger combination is pressed, the action fires)
30. An empty/cleared shortcut disables that action — pressing keys does nothing for that action
31. The `open_settings` shortcut, once configured, opens the settings window from anywhere in the main app

### Claude Code

32. Changes to Claude Code CLI flags or CLAUDE.md path show a "Restart Required" indicator with a "Restart Claude Code" button that recreates the subprocess (without restarting the whole app)

## Implementation Approach

### Backend: Settings Module

```
src-tauri/src/
  settings/
    mod.rs           # public API
    schema.rs        # the Settings struct definition
    persistence.rs   # load/save JSON
    broadcaster.rs   # change broadcasting
  ipc/
    shortcuts.rs     # parse and dispatch shortcut events from frontend
```

#### Schema (`schema.rs`)

```rust
use serde::{Serialize, Deserialize};
use std::path::PathBuf;

#[derive(Clone, Serialize, Deserialize, Debug)]
#[serde(default)]
pub struct Settings {
    pub tts: TtsSettings,
    pub segmentation: SegmentationSettings,
    pub avatar: AvatarSettings,
    pub display: DisplaySettings,
    pub behavior: BehaviorSettings,
    pub compose: ComposeSettings,
    pub shortcuts: ShortcutSettings,
    pub claude_code: ClaudeCodeSettings,
    pub processing: ProcessingSettings,
}

#[derive(Clone, Serialize, Deserialize, Debug)]
#[serde(default)]
pub struct AvatarSettings {
    pub visible: bool,
    pub size: AvatarSize,
    pub position: AvatarPosition,
    pub margin_px: u32,
    pub opacity: f32,
    pub images: AvatarImages,
    pub transition: TransitionSettings,
    pub waveform: WaveformSettings,
}

#[derive(Clone, Serialize, Deserialize, Debug)]
#[serde(default)]
pub struct AvatarSize {
    pub width_px: u32,
    pub height_px: u32,
}

#[derive(Clone, Copy, Serialize, Deserialize, Debug)]
#[serde(rename_all = "kebab-case")]
pub enum AvatarPosition {
    TopRight,
    TopLeft,
    BottomRight,
    BottomLeft,
}

impl Default for AvatarPosition {
    fn default() -> Self { Self::TopRight }
}

#[derive(Clone, Serialize, Deserialize, Debug)]
#[serde(default)]
pub struct AvatarImages {
    pub idle: Option<PathBuf>,
    pub listening: Option<PathBuf>,
    pub thinking: Option<PathBuf>,
    pub speaking: Option<PathBuf>,
    pub error: Option<PathBuf>,
}

#[derive(Clone, Serialize, Deserialize, Debug)]
#[serde(default)]
pub struct TransitionSettings {
    pub path: Option<PathBuf>,
    pub duration_ms: u32,
}

#[derive(Clone, Serialize, Deserialize, Debug)]
#[serde(default)]
pub struct ComposeSettings {
    pub min_height_px: u32,
    pub max_height_px: u32,
}

#[derive(Clone, Serialize, Deserialize, Debug)]
#[serde(default)]
pub struct ShortcutSettings {
    pub open_compose: Option<String>,
    pub submit_compose: Option<String>,
    pub cancel_compose: Option<String>,
    pub open_settings: Option<String>,
}

impl Default for AvatarSettings {
    fn default() -> Self {
        Self {
            visible: true,
            size: AvatarSize { width_px: 400, height_px: 400 },
            position: AvatarPosition::TopRight,
            margin_px: 16,
            opacity: 0.8,
            images: AvatarImages::default(),
            transition: TransitionSettings::default(),
            waveform: WaveformSettings::default(),
        }
    }
}

impl Default for ShortcutSettings {
    fn default() -> Self {
        Self {
            open_compose: Some("Ctrl+Shift+E".to_string()),
            submit_compose: Some("Ctrl+Enter".to_string()),
            cancel_compose: Some("Escape".to_string()),
            open_settings: Some("Ctrl+,".to_string()),
        }
    }
}

impl Default for ComposeSettings {
    fn default() -> Self {
        Self { min_height_px: 80, max_height_px: 300 }
    }
}
```

`#[serde(default)]` on each struct ensures forward compatibility.

The `pane_split_ratio` field is removed entirely.

#### Persistence (`persistence.rs`)

Standard load/save with corruption recovery (as previously specified):

```rust
pub fn config_path() -> Result<PathBuf, AppError>;
pub fn load() -> Result<Settings, AppError>;
pub fn save(settings: &Settings) -> Result<(), AppError>;
```

If the file is missing, defaults are written. If the file is corrupt (parse error), defaults are loaded and the corrupt file is overwritten with valid defaults.

#### Broadcasting

Same as previously: `tokio::sync::broadcast` with the full `Settings` struct on each change. Components subscribe and update on changes that affect their state.

### Wiring Components to Settings

Each component subscribes on startup. Examples:

- TTS engine reacts to voice/speed changes
- Audio output reacts to volume/mute changes
- Processing layer reacts to stability/max-hold changes
- Avatar overlay (frontend) reacts to size/position/margin/opacity/visibility/images/transition changes
- Waveform overlay (frontend) reacts to color/line-width/glow/opacity changes
- Shortcut dispatcher (frontend) re-parses shortcut strings on change

### Avatar Overlay Frontend Changes

The avatar overlay component from Milestone 4 reads layout values from `avatarConfig`. In this milestone, replace `avatarConfig` with a Svelte store that mirrors the avatar settings slice. The store updates on settings-changed events; the component reactivity automatically propagates layout changes.

Visibility is now driven by the persisted setting plus runtime toggle. Combine them: when the toggle button is clicked, update the setting via the IPC pathway. This way the persistence and the toggle are unified — there's only one source of truth (the settings store).

### Waveform Overlay Frontend Changes

Same pattern — read waveform settings from a store rather than `avatarConfig`. Update reactively.

### Shortcuts Implementation

#### Backend role

The backend's role for shortcuts is minimal: it stores the shortcut strings in settings and broadcasts changes. The actual key handling happens entirely on the frontend (in the webview), which has direct access to keyboard events.

#### Frontend dispatcher

```
src/lib/
  shortcuts/
    parser.ts        # parse shortcut strings into key event predicates
    dispatcher.ts    # window-level keydown listener that matches and fires
```

#### `parser.ts`

```typescript
export interface ShortcutPredicate {
    key: string;
    ctrl: boolean;
    shift: boolean;
    alt: boolean;
    meta: boolean;
}

export function parseShortcut(s: string | null | undefined): ShortcutPredicate | null {
    if (!s) return null;
    const parts = s.split('+').map(p => p.trim().toLowerCase());
    const key = parts[parts.length - 1];
    const modifiers = new Set(parts.slice(0, -1));
    return {
        key,
        ctrl: modifiers.has('ctrl') || modifiers.has('control'),
        shift: modifiers.has('shift'),
        alt: modifiers.has('alt'),
        meta: modifiers.has('meta') || modifiers.has('cmd') || modifiers.has('command'),
    };
}

export function matches(event: KeyboardEvent, p: ShortcutPredicate): boolean {
    return (
        event.key.toLowerCase() === p.key &&
        event.ctrlKey === p.ctrl &&
        event.shiftKey === p.shift &&
        event.altKey === p.alt &&
        event.metaKey === p.meta
    );
}
```

Special-case the key `'enter'` to also match `event.key === 'Enter'`, and `'escape'` to match `event.key === 'Escape'`. Special characters like `,` need their own normalization.

#### `dispatcher.ts`

A module-level keydown listener that runs in capture phase, so it receives events before xterm.js does:

```typescript
import { parseShortcut, matches } from './parser';
import type { ShortcutPredicate } from './parser';

interface ShortcutHandlers {
    open_compose?: () => void;
    submit_compose?: () => void;
    cancel_compose?: () => void;
    open_settings?: () => void;
}

let predicates: Record<string, ShortcutPredicate | null> = {};
let handlers: ShortcutHandlers = {};

export function configureShortcuts(
    config: { open_compose?: string; submit_compose?: string; cancel_compose?: string; open_settings?: string },
    h: ShortcutHandlers
) {
    predicates = {
        open_compose: parseShortcut(config.open_compose),
        submit_compose: parseShortcut(config.submit_compose),
        cancel_compose: parseShortcut(config.cancel_compose),
        open_settings: parseShortcut(config.open_settings),
    };
    handlers = h;
}

window.addEventListener('keydown', (event) => {
    for (const [name, pred] of Object.entries(predicates)) {
        if (pred && matches(event, pred)) {
            const handler = handlers[name as keyof ShortcutHandlers];
            if (handler) {
                event.preventDefault();
                event.stopPropagation();
                handler();
            }
            return;
        }
    }
}, true); // capture phase
```

This listener runs *before* xterm.js sees the key event, so configured shortcuts intercept correctly. When no shortcut matches, the event continues to xterm.js as normal.

For the compose milestone, `submit_compose` will only invoke its handler when the textarea has focus (the handler checks focus state). `cancel_compose` will only invoke its handler when the compose sheet is open.

### Settings Window Frontend

A separate Tauri window with its own URL pointing to `settings.html`. Open via `WebviewWindow` API as previously documented.

#### Sections

For this milestone, the settings window has the following sections in order:

1. **TTS** — voice dropdown, speed slider, volume slider, mute toggle
2. **Avatar** — visibility toggle, position dropdown, width and height numeric inputs, margin numeric input, opacity slider (30–100%), per-state image pickers, transition path picker + duration input
3. **Waveform** — color picker, line width slider, glow intensity slider, opacity slider
4. **Display** — terminal font family, font size, theme dropdown, TTS markup visibility (hidden in v1, but expose for future)
5. **Behavior** — interrupt on input toggle, auto speak toggle, fallback silent toggle (disabled, always true in v1)
6. **Compose** — min height, max height (numeric inputs)
7. **Shortcuts** — capture fields for open_compose, submit_compose, cancel_compose, open_settings
8. **Claude Code** — extra CLI flags (text array editor), CLAUDE.md path override, "Restart Required" indicator, "Restart Claude Code" button
9. **Processing** — stability timeout (ms), max hold (ms)

#### Shortcut capture UI

A common pattern: a button that says "Click to set" or shows the current shortcut. Clicking enters "capture mode" — the next keypress is captured as the new shortcut. A modifier-only press is rejected (must include a non-modifier key). Escape during capture cancels (returns to previous binding). A "Clear" button next to each shortcut empties the binding.

```svelte
<!-- ShortcutCapture.svelte (sketch) -->
<script lang="ts">
  export let value: string | null;
  let capturing = false;

  function startCapture() {
    capturing = true;
  }

  function handleKeydown(event: KeyboardEvent) {
    if (!capturing) return;
    event.preventDefault();
    event.stopPropagation();
    if (event.key === 'Escape') {
      capturing = false;
      return;
    }
    // Reject pure modifier presses
    if (['Control', 'Shift', 'Alt', 'Meta'].includes(event.key)) return;
    const parts = [];
    if (event.ctrlKey) parts.push('Ctrl');
    if (event.shiftKey) parts.push('Shift');
    if (event.altKey) parts.push('Alt');
    if (event.metaKey) parts.push('Meta');
    parts.push(event.key);
    value = parts.join('+');
    capturing = false;
  }

  function clear() {
    value = null;
  }
</script>

<svelte:window on:keydown={handleKeydown} />

<button on:click={startCapture}>
  {capturing ? 'Press a key combination...' : (value ?? 'Not set')}
</button>
<button on:click={clear} disabled={!value}>Clear</button>
```

Note: while capturing, the `dispatcher.ts` global listener could conflict. Suppress the dispatcher temporarily (e.g., a "capturing" flag the dispatcher checks), or accept that the dispatcher might fire one of its own shortcuts during capture and just live with it (rare in practice). The simplest fix is the suppression flag.

### Restart Claude Code

Backend command and frontend button as previously specified. No change.

## Validation Steps

1. **First launch with no config**: delete the config file, launch the app. Verify defaults are used and a config file is created.
2. **Persistence**: change settings, close and relaunch. Verify changes persist.
3. **Live TTS voice/speed/volume/mute**: change while running, verify each takes effect immediately.
4. **Live avatar image swap**: change a state's image; trigger that state; verify the new image displays.
5. **Live transition asset / duration**: change path and duration; verify next state change uses the new asset for the new duration. Clear path; verify subsequent transitions snap directly.
6. **Live avatar size**: change width and height; verify the avatar resizes immediately.
7. **Live avatar position**: change position dropdown through all four corners; verify the avatar relocates correctly each time.
8. **Live avatar margin**: change margin; verify spacing from the corner adjusts immediately.
9. **Live avatar opacity**: drag opacity slider from 30% to 100%; verify avatar (and toggle button) opacity updates in real time.
10. **Avatar visibility persistence**: hide the avatar via toggle button. Close the app. Relaunch. Verify the avatar is still hidden. Show via toggle button. Close. Relaunch. Verify still visible.
11. **Live waveform**: change color, line width, glow intensity, opacity while audio is playing. Verify each updates immediately. Verify the waveform's opacity is independent of the avatar's opacity (set avatar to 30% and waveform to 90% and visually confirm).
12. **Live terminal font**: change font size; verify the terminal reflows immediately.
13. **Restart-required setting**: change a Claude Code CLI flag. Verify "Restart Required" indicator. Click "Restart Claude Code". Verify subprocess restarts and new flag is in effect.
14. **Shortcut capture**: click each shortcut's capture field, press a key combination. Verify the captured combination is displayed correctly. Verify the new shortcut works (open_settings, especially — try pressing the configured combination).
15. **Shortcut clearing**: clear a shortcut. Verify pressing the previously-bound combination no longer triggers the action.
16. **Open settings via shortcut**: configure `open_settings`; close the settings window; press the shortcut from the main window. Verify the settings window opens.
17. **Corrupt config recovery**: corrupt the config file. Launch. Verify defaults load and a clean file is written.
18. **Cross-platform**: verify all of the above on the second platform.

## Known Risks and Mitigation

- **Settings change races**: rapid changes mitigated by frontend debounce and full-struct broadcast.
- **Schema evolution**: `#[serde(default)]` handles missing fields. Removed fields are silently ignored (no `deny_unknown_fields`). Document changes in DESIGN.md.
- **Shortcut conflicts with terminal input**: if the user binds something that conflicts with a Claude Code key combination (e.g., `Ctrl+C`), Claude Code never sees the keypress because the dispatcher captures it first. Show a soft warning if the user binds a known-problematic combination, but don't block.
- **Path serialization**: paths returned by the dialog should be absolute. Verify they roundtrip correctly through JSON, especially Windows paths.
- **Capture mode race with dispatcher**: shortcut capture must temporarily suppress the dispatcher to avoid the user accidentally firing an existing shortcut while binding a new one. The suppression flag handles this.
- **Position transition jank**: when changing avatar position, the avatar may abruptly jump from one corner to another. Acceptable for v1; a smooth animated reposition could be a polish task.

## What "Done" Looks Like

The user can open settings, change anything, and see the change applied immediately. Persistence works across restarts. The avatar can be repositioned, resized, made transparent, hidden, all from the settings window or via the toggle button. Shortcuts are user-configurable. The only thing requiring restart is Claude Code subprocess configuration, and that's a one-button operation. The infrastructure is in place for the compose overlay (Milestone 7) which depends on the shortcut system established here.

---

## Next Milestone

Milestone 7: Compose Overlay. Adds a slide-up bottom sheet with a spell-checking textarea for composing longer messages, using the shortcut system from this milestone. Submits in append mode to the PTY.
