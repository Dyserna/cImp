# Milestone 4: Avatar Overlay

## Goal

Restructure the layout so the terminal occupies the full window, and add a floating avatar overlay positioned in a configurable corner with configurable size and opacity. Implement the avatar state machine in the Rust backend, broadcast state changes to the frontend, and render the configured image or animation for each state on the avatar overlay. Add a thin vertical toggle button adjacent to the avatar that hides/shows it. Support an optional shared transition animation that plays between every state change at runtime (but not at app launch). No waveform overlay yet — that comes in Milestone 5.

## Why This Milestone Now

The TTS pipeline (Milestone 3) is functional. The avatar adds visual feedback that mirrors the audio behavior and the application's general state. Doing the avatar before the visualizer means the layout, image rendering, state machine, and transition logic are validated independently from the more complex Canvas-based waveform work.

## Scope

### In Scope

- Terminal occupies the full window (no two-pane split, no splitter)
- Avatar overlay floating on top of the terminal:
  - Position: hardcoded `top-right` for this milestone (configurable in Milestone 6)
  - Size: hardcoded 400×400 pixels for this milestone (configurable in Milestone 6)
  - Margin from corner: hardcoded 16px (configurable in Milestone 6)
  - Opacity: hardcoded 80% (configurable in Milestone 6)
  - Image fitting: `contain` (preserve aspect ratio, letterbox if needed)
- A thin vertical toggle button adjacent to the avatar (on the side facing the screen edge — right side for top-right position) that:
  - Spans the full vertical height of the avatar
  - Toggles avatar visibility on click
  - Remains visible when the avatar is hidden, so the user can re-show it
- Avatar visibility persists across the app session (settings persistence is Milestone 6, so for this milestone use an in-memory toggle)
- Per-state image configuration in code (hardcoded paths for this milestone, settings UI in Milestone 6)
- A single shared transition asset and duration in code (also hardcoded for this milestone)
- Transition behavior:
  - Plays once for its configured duration on every state change at runtime
  - Does **not** play at app launch
  - If no transition is configured, all state changes snap directly to the new state image
- Transition interruption: state change during a transition cancels it and starts fresh
- Source images with alpha channels (transparent backgrounds) render with their transparency intact
- Global opacity multiplier applied to the avatar overlay container (affects avatar image AND toggle button via CSS opacity inheritance)
- A `state` module in the Rust backend that:
  - Defines the five-state enum: `Idle`, `Listening`, `Thinking`, `Speaking`, `Error`
  - Receives signals from PTY (user input), processing layer (output flow), TTS engine (synthesis activity), audio output (playback activity)
  - Computes state transitions based on observed events
  - Broadcasts state changes to the frontend via Tauri events
- A gear icon button in the top-right corner of the avatar (for now, a placeholder that does nothing — settings window is Milestone 6)
- Hidden avatar = hidden gear button (no settings access from the UI when hidden; this is fine since the keyboard shortcut for settings is also a Milestone 6 feature)

### Out of Scope

- Waveform overlay (Milestone 5)
- Settings window (Milestone 6) — gear button is a placeholder
- User-configurable image paths, transition asset, position, size, margin, opacity (all Milestone 6)
- Settings persistence to disk (Milestone 6) — the visibility toggle resets on app restart in this milestone
- Compose overlay (Milestone 7)
- Per-state transition animations (deliberately deferred per design)
- Transition animation at app launch (deliberately deferred per design)
- Configurable image fitting modes other than `contain`

## Acceptance Criteria

### Layout

1. The application window shows the terminal at full window width and height
2. The avatar overlay is rendered floating in the top-right corner of the window, 400×400 pixels in size, with a 16px margin from the top and right edges
3. The avatar image is sized to fit the 400×400 area using `contain` (preserves aspect ratio; letterboxes if the image isn't square)
4. The terminal underneath the avatar remains fully visible and interactive (clickable, scrollable, accepts input). Terminal text behind the avatar is obscured by the avatar's pixels but is not "blocked" — the user can scroll the terminal to bring obscured text into view

### Toggle button

5. A thin vertical button is rendered to the right of the avatar (between the avatar's right edge and the screen's right margin), spanning the full 400px vertical height
6. Clicking the toggle button hides the avatar; the toggle button itself remains visible
7. Clicking the toggle button again shows the avatar
8. The toggle button has clear visual affordance (e.g., a subtle background, hover effect, and an icon or chevron indicating its function)

### State and visuals

9. At app launch, the avatar shows the configured Idle image directly with no transition animation
10. When the user starts typing in the terminal, the avatar transitions to the Listening state
11. When Claude Code starts generating output, the avatar transitions to Thinking
12. When TTS audio starts playing, the avatar transitions to Speaking
13. When TTS audio stops and Claude Code is no longer generating, the avatar transitions back to Idle
14. If the Claude Code subprocess exits unexpectedly, the avatar transitions to Error and stays there
15. Animated formats (GIF, animated WebP) play their animation correctly while displayed
16. State transitions are visible in logs at INFO level

### Opacity and transparency

17. The avatar overlay renders at 80% opacity by default. The toggle button, being part of the same overlay container, also renders at 80% opacity
18. Source images with alpha channels (e.g., transparent PNG, animated WebP with alpha) render with their transparency preserved — the terminal shows through wherever the image is transparent. The 80% global opacity composes multiplicatively with the source image's alpha
19. The visual result is that terminal text is faintly visible through the avatar even when the source image is opaque

### Transitions

20. With a transition asset configured, the asset plays for its configured duration on every state change at runtime, then is replaced by the new state's looping image
21. With no transition asset configured, state changes snap directly to the new state image
22. The transition does not play at app launch (Idle image shown directly)
23. State changes during a running transition cancel the transition and start fresh
24. Transitions play even when transitioning back into Idle

### Other

25. A gear icon is visible in the top-right corner of the avatar (clicking does nothing for this milestone)
26. When the avatar is hidden, the gear button is hidden along with it
27. The terminal experience from previous milestones is unchanged — same responsiveness, same TTS behavior

## Implementation Approach

### Backend: State Manager

Add to the Rust backend:

```
src-tauri/src/
  state/
    mod.rs
    manager.rs       # state machine, signal handling, broadcasting
```

#### Public API

```rust
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub enum AvatarState {
    Idle,
    Listening,
    Thinking,
    Speaking,
    Error,
}

pub enum StateSignal {
    UserInput,                  // user typed something
    UserInputStopped,           // typing timeout reached
    ClaudeOutputStarted,        // first non-input byte from PTY after user input
    ClaudeOutputStopped,        // Claude appears done generating
    TtsPlaybackStarted,         // first audio buffer for a sentence began playing
    TtsPlaybackStopped,         // audio queue empty
    SubprocessExited,           // claude died
    AudioError,                 // audio device problem
    TtsError,                   // synthesis problem
    ErrorAcknowledged,          // user acknowledged or recovered from error
}

pub struct StateManager { /* current state, broadcast channel */ }

impl StateManager {
    pub fn new(broadcast_tx: broadcast::Sender<AvatarState>) -> Self;
    pub async fn handle_signal(&mut self, signal: StateSignal);
    pub fn current(&self) -> AvatarState;
}
```

The state manager is unaware of transitions, position, opacity, or any visual concern — it just emits state changes.

#### State Transition Logic

```
match (current_state, signal) {
    (Idle, UserInput) => Listening,
    (Idle, ClaudeOutputStarted) => Thinking,
    (Idle, SubprocessExited | AudioError | TtsError) => Error,

    (Listening, ClaudeOutputStarted) => Thinking,
    (Listening, UserInputStopped) => Idle,
    (Listening, SubprocessExited | AudioError | TtsError) => Error,

    (Thinking, TtsPlaybackStarted) => Speaking,
    (Thinking, ClaudeOutputStopped) => Idle,
    (Thinking, SubprocessExited | AudioError | TtsError) => Error,

    (Speaking, TtsPlaybackStopped) => {
        if claude_still_generating { Thinking } else { Idle }
    },
    (Speaking, SubprocessExited | AudioError | TtsError) => Error,

    (Error, ErrorAcknowledged) => Idle,
    (Error, _) => Error,

    (current, _) => current,
}
```

#### Signal Sources

Same as previously documented:

- **`UserInput`**: emitted by `pty_write` IPC handler on every keystroke
- **`UserInputStopped`**: timer task, fires after ~2s of no input
- **`ClaudeOutputStarted`**: processing layer, on first byte after quiet period
- **`ClaudeOutputStopped`**: processing layer, after stability window of no output
- **`TtsPlaybackStarted`** / **`TtsPlaybackStopped`**: audio output module
- **`SubprocessExited`**: PTY manager
- **`AudioError`** / **`TtsError`**: respective error paths

### Frontend: Layout Restructure

`App.svelte` becomes a single-pane layout with the terminal at full extent and the avatar overlay positioned absolutely over it.

```svelte
<script lang="ts">
  import Terminal from './lib/Terminal.svelte';
  import AvatarOverlay from './lib/AvatarOverlay.svelte';
</script>

<main>
  <Terminal />
  <AvatarOverlay />
</main>

<style>
  :global(html, body) {
    margin: 0;
    padding: 0;
    height: 100%;
    overflow: hidden;
  }
  main {
    position: relative;
    width: 100vw;
    height: 100vh;
  }
  /* Terminal fills the main container */
  /* AvatarOverlay is positioned absolutely within main */
</style>
```

The previous splitter component is removed. Any imports or references to pane-split state are removed.

### Frontend: Avatar Overlay Component

```
src/lib/
  AvatarOverlay.svelte
  avatarState.ts     # store for current state
  avatarConfig.ts    # hardcoded image, transition, layout config for this milestone
```

#### `avatarState.ts`

```typescript
import { writable } from 'svelte/store';
import { listen } from '@tauri-apps/api/event';

export type AvatarState = 'Idle' | 'Listening' | 'Thinking' | 'Speaking' | 'Error';

export const avatarState = writable<AvatarState>('Idle');
export const avatarVisible = writable<boolean>(true);

listen<AvatarState>('avatar-state', (event) => {
    avatarState.set(event.payload);
});
```

#### `avatarConfig.ts`

```typescript
export interface AvatarConfig {
    images: Record<AvatarState, string>;
    transition: { path: string | null; durationMs: number };
    layout: {
        widthPx: number;
        heightPx: number;
        position: 'top-right' | 'top-left' | 'bottom-right' | 'bottom-left';
        marginPx: number;
        opacity: number;
    };
}

export const avatarConfig: AvatarConfig = {
    images: {
        Idle:      '/path/to/idle.gif',
        Listening: '/path/to/listening.png',
        Thinking:  '/path/to/thinking.gif',
        Speaking:  '/path/to/speaking.gif',
        Error:     '/path/to/error.png',
    },
    transition: {
        path: '/path/to/transition.gif',
        durationMs: 400,
    },
    layout: {
        widthPx: 400,
        heightPx: 400,
        position: 'top-right',
        marginPx: 16,
        opacity: 0.8,
    },
};
```

#### `AvatarOverlay.svelte`

The avatar overlay is positioned absolutely within the main container. Its CSS variables drive the position and size; the opacity is applied to the container. The toggle button is a sibling element placed adjacent to the avatar.

```svelte
<script lang="ts">
  import { onDestroy } from 'svelte';
  import { avatarState, avatarVisible, type AvatarState } from './avatarState';
  import { avatarConfig } from './avatarConfig';

  let displayedSrc = avatarConfig.images.Idle;
  let displayedState: AvatarState = 'Idle';
  let transitionTimer: number | null = null;
  let isFirstRender = true;

  $: handleStateChange($avatarState);

  function handleStateChange(newState: AvatarState) {
    if (isFirstRender) {
      isFirstRender = false;
      displayedState = newState;
      displayedSrc = avatarConfig.images[newState] ?? avatarConfig.images.Idle;
      return;
    }

    if (newState === displayedState && transitionTimer === null) return;

    if (transitionTimer !== null) {
      clearTimeout(transitionTimer);
      transitionTimer = null;
    }

    const transition = avatarConfig.transition;
    const stateImage = avatarConfig.images[newState] ?? avatarConfig.images.Idle;

    if (transition.path && transition.durationMs > 0) {
      displayedSrc = `${transition.path}?t=${Date.now()}`;
      transitionTimer = window.setTimeout(() => {
        displayedSrc = stateImage;
        displayedState = newState;
        transitionTimer = null;
      }, transition.durationMs);
      displayedState = newState;
    } else {
      displayedSrc = stateImage;
      displayedState = newState;
    }
  }

  function toggleVisibility() {
    avatarVisible.update(v => !v);
  }

  function openSettings() {
    console.log('settings clicked');
  }

  onDestroy(() => {
    if (transitionTimer !== null) clearTimeout(transitionTimer);
  });

  // Position styling derived from config
  $: positionStyles = computePositionStyles(avatarConfig.layout);

  function computePositionStyles(layout: typeof avatarConfig.layout): string {
    const { widthPx, heightPx, position, marginPx, opacity } = layout;
    const styles: string[] = [
      `--avatar-width: ${widthPx}px`,
      `--avatar-height: ${heightPx}px`,
      `--avatar-margin: ${marginPx}px`,
      `--avatar-opacity: ${opacity}`,
    ];
    return styles.join(';');
  }

  $: positionClass = avatarConfig.layout.position;
</script>

<div class="avatar-container {positionClass}" style={positionStyles}>
  {#if $avatarVisible}
    <div class="avatar-overlay">
      <img src={displayedSrc} alt="Avatar" class="avatar-image" />
      <button class="settings-button" on:click={openSettings} aria-label="Settings">
        ⚙
      </button>
    </div>
  {/if}
  <button class="toggle-button" on:click={toggleVisibility} aria-label="Toggle avatar">
    <!-- chevron or other glyph -->
    {$avatarVisible ? '›' : '‹'}
  </button>
</div>

<style>
  .avatar-container {
    position: absolute;
    display: flex;
    align-items: stretch;
    pointer-events: none; /* let clicks pass through gaps; children re-enable */
  }
  .avatar-container.top-right {
    top: var(--avatar-margin);
    right: var(--avatar-margin);
    flex-direction: row;
  }
  .avatar-container.top-left {
    top: var(--avatar-margin);
    left: var(--avatar-margin);
    flex-direction: row-reverse;
  }
  .avatar-container.bottom-right {
    bottom: var(--avatar-margin);
    right: var(--avatar-margin);
    flex-direction: row;
  }
  .avatar-container.bottom-left {
    bottom: var(--avatar-margin);
    left: var(--avatar-margin);
    flex-direction: row-reverse;
  }

  .avatar-overlay {
    position: relative;
    width: var(--avatar-width);
    height: var(--avatar-height);
    opacity: var(--avatar-opacity);
    pointer-events: auto;
  }

  .avatar-image {
    width: 100%;
    height: 100%;
    object-fit: contain;
  }

  .settings-button {
    position: absolute;
    top: 8px;
    right: 8px;
    background: rgba(0, 0, 0, 0.5);
    border: none;
    color: #fff;
    width: 32px;
    height: 32px;
    border-radius: 4px;
    cursor: pointer;
    font-size: 18px;
  }

  .toggle-button {
    width: 16px;
    height: var(--avatar-height);
    background: rgba(0, 0, 0, 0.4);
    border: none;
    color: #fff;
    cursor: pointer;
    pointer-events: auto;
    /* The toggle button is OUTSIDE .avatar-overlay so the opacity rule
       above applies only to the avatar image and gear, not the toggle.
       However, the design calls for the toggle to share the avatar's
       opacity. So we apply the same opacity here explicitly. */
    opacity: var(--avatar-opacity);
  }

  .top-left .toggle-button,
  .bottom-left .toggle-button {
    /* For left-side positions, the toggle is mirrored visually */
  }
</style>
```

#### Layout note for the toggle button

The CSS structure places the toggle button as a sibling of the `.avatar-overlay` div, not a child. This is intentional: the waveform visualizer (Milestone 5) will need to be a sibling that doesn't inherit the avatar's opacity, and it's cleaner to establish the sibling pattern now.

The current implementation applies the same `var(--avatar-opacity)` to the toggle button so it visually matches the avatar's opacity, but if it's ever desired to make the toggle button independent (e.g., always full-opacity for visibility), it's a one-line change.

#### Layout note for hidden avatar

When `$avatarVisible` is false, the `.avatar-overlay` div is removed entirely (not just hidden via CSS). The toggle button stays. The container is still positioned at the same corner; only the avatar content is gone. The toggle button retains its position adjacent to where the avatar used to be.

If the avatar is hidden at startup (only relevant in later milestones with persistence), the toggle button is the only element rendered.

### Wiring State Signals

Same as previously planned; no change beyond the layout.

## Validation Steps

### Layout

1. **Full-window terminal**: launch the app, verify the terminal occupies the entire window
2. **Avatar position**: verify the avatar is visible in the top-right corner, 400×400 pixels, with 16px margin from the top and right edges
3. **Image fitting**: configure a non-square test image; verify it letterboxes within the 400×400 area without distortion
4. **Terminal visibility behind avatar**: verify the terminal's content under the avatar is partially visible (due to 80% opacity) and that scrolling the terminal moves text around behind the avatar correctly
5. **Terminal interactivity**: click on terminal content not under the avatar; verify selection works. Click on terminal content directly under the avatar; verify the click is captured by the avatar (this is expected and acceptable)

### Toggle button

6. **Visibility toggle**: click the toggle button; verify the avatar disappears and the toggle button remains
7. **Show again**: click the toggle button again; verify the avatar reappears
8. **Toggle button position**: verify the toggle button is to the right of where the avatar is, spans 400px vertical height, and is clearly distinguishable

### State and visuals

9. **App launch**: verify the Idle image is shown immediately at app launch with no transition flash
10. **Listening state**: type into the terminal; verify the avatar plays the transition then swaps to Listening
11. **Thinking state**: send a message to Claude; verify the avatar plays the transition then shows Thinking
12. **Speaking state**: when TTS plays, verify the avatar plays the transition then shows Speaking
13. **Idle return**: when Claude finishes, verify the transition plays and the avatar returns to Idle
14. **Error state**: kill the `claude` subprocess; verify the avatar transitions to Error
15. **Animated formats**: configure animated GIF and WebP for different states; verify both animate correctly

### Opacity

16. **Default opacity**: at the default 80% opacity, verify terminal text is faintly visible through opaque source images
17. **Transparent source images**: configure an image with alpha-channel transparency; verify the terminal shows through the transparent regions clearly (alpha + 80% opacity composes correctly)
18. **Toggle button opacity**: verify the toggle button is rendered at the same 80% opacity as the avatar

### Transitions

19. **Transition plays on state change**: with a transition configured, verify it plays on every state change after app launch
20. **No transition at launch**: verify the very first display (Idle on app launch) is NOT preceded by a transition
21. **Interruption**: trigger rapid state changes; verify only the latest transition plays
22. **No transition configured**: clear the transition path; verify state changes snap directly without intermediate animation

### Cross-platform

23. Verify all of the above on the second target platform; pay attention to any rendering differences between WebView2 and WebKitGTK, especially around opacity, animated WebP, and alpha-channel composition

## Known Risks and Mitigation

- **Click-through behavior over the terminal**: the avatar overlay captures clicks within its bounds. Terminal text directly under the avatar is not directly clickable. The toggle button is the escape valve — hide the avatar to access content under it. If this feels too restrictive in practice, consider adding a "ghost mode" later where the avatar passes clicks through. Out of scope for v1.
- **Animated WebP rendering on WebKitGTK**: very old WebKitGTK versions may not support animated WebP. Document a minimum version requirement if it comes up.
- **Opacity composition**: confirm in testing that the visual is what you want. If the multiplied opacity (source alpha × 80% global) ends up too washed out for fully-opaque source images, increase the default to 90% or rethink the composition.
- **Toggle button discoverability**: a 16px-wide button is small. If users miss it, increase the width or add a clearer affordance (icon, hover label). Defer to polish.
- **Image cache-bust on transition**: same as previously noted; the `?t=Date.now()` query param forces fresh playback each time.
- **First-render flag**: the `isFirstRender` boolean ensures the very first state assignment doesn't trigger a transition. Tests should explicitly verify this — it's the easiest thing to break with a refactor.
- **Position rendering**: the four corners are implemented via flex direction and absolute positioning. Tests should exercise all four corners (even though only top-right is used in this milestone) to make sure the layout code doesn't bake in assumptions.

## What "Done" Looks Like

The app is a single-pane terminal with a floating, semi-transparent avatar in the top-right corner. The avatar reacts to what's happening with state-driven images and an optional shared transition animation. The toggle button hides and shows the avatar on demand. The terminal experience is otherwise unchanged. Source images can have transparent backgrounds that integrate visually with the terminal underneath. The architecture is in place for the visualizer (Milestone 5) and configurable settings (Milestone 6).

---

## Next Milestone

Milestone 5: Visualizer. Adds the scrolling oscilloscope waveform overlay to the avatar area, reactive to TTS audio playback via the amplitude tap from Milestone 3. The waveform is rendered as a sibling of the avatar overlay (not a child) so its opacity is independent of the avatar's overall opacity.
