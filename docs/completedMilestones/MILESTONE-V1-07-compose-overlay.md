# Milestone 7: Compose Overlay

## Goal

Add a spell-checking compose overlay — a slide-up bottom sheet with a textarea — for composing longer messages with browser-native spell-check support. The sheet does not block the terminal underneath; the user can still interact with the terminal (selecting, copying, even typing) while the sheet is open. Submits text to the PTY in append mode (without modifying any existing input in Claude Code's input box). Uses the shortcut system from Milestone 6 for trigger, submit, and cancel actions.

## Why This Milestone Now

The settings window with shortcut configuration (Milestone 6) is in place, providing the configurable shortcuts this milestone consumes. The compose overlay is the last functional addition before polish.

## Scope

### In Scope

- A bottom-sheet compose overlay component:
  - Slides up from the bottom of the application window when triggered
  - Spans the full window width
  - Auto-grow height: starts at the configured `min_height_px` (default 80), grows up to `max_height_px` (default 300) as content is added, scrolls internally beyond max
  - Visually distinct from the terminal (different background tone, top border, slight elevation/shadow)
  - Slides up/down with a brief animation (~200ms ease)
- A textarea inside the sheet with `spellcheck="true"` enabled — browser-native spell-check provides red squiggles and right-click corrections
- The terminal underneath remains fully interactive while the sheet is open (clickable, scrollable, keyboard-focusable)
- Triggered by the configured `open_compose` shortcut. Has no effect if the sheet is already open (no toggle).
- The configured `submit_compose` shortcut sends the textarea content to the PTY:
  - Append mode: writes the textarea bytes to the PTY followed by a newline (Enter). Does not clear or modify Claude Code's existing input line.
  - Closes the sheet on submit
  - Submit shortcut only fires when the textarea has focus
- The configured `cancel_compose` shortcut closes the sheet without submitting; draft is discarded
  - Cancel shortcut fires globally while the sheet is open, regardless of focus
- State manager integration: while the compose sheet is open AND the textarea is non-empty, treat as user input activity (transitions avatar to Listening). Empty textarea contributes no signal.
- Sheet style:
  - The textarea uses a comfortable text font (system sans-serif), not the terminal's monospace
  - Reasonable padding around the textarea (not flush against the sheet edges)

### Out of Scope

- Replace mode for submissions (append mode only)
- Draft preservation across cancellations
- Separate keyboard shortcut to toggle the sheet closed (open shortcut is one-way; use cancel shortcut to close)
- Custom spell-check engine (relies entirely on browser/WebView spell-check)
- User-configurable spell-check language (defaults to whatever the WebView's locale provides)
- Multiple compose sheets or compose history

## Acceptance Criteria

### Trigger and dismiss

1. Pressing the configured `open_compose` shortcut opens the compose sheet (default `Ctrl+Shift+E`)
2. The sheet slides up from the bottom with a smooth animation
3. The textarea receives focus when the sheet opens
4. Pressing the `open_compose` shortcut while the sheet is already open has no effect (no toggle)
5. Pressing the configured `cancel_compose` shortcut (default `Escape`) closes the sheet
6. Closing the sheet via cancel discards any text in the textarea — re-opening shows an empty textarea
7. Cancel works regardless of whether the textarea or the terminal has focus

### Submission

8. Pressing the configured `submit_compose` shortcut (default `Ctrl+Enter`) sends the textarea content to the PTY:
   - The text is sent as bytes followed by a newline character
   - Claude Code receives this as if pasted and submitted
9. The submit shortcut only fires when the textarea has focus — pressing it while focus is in the terminal does not trigger submit (the terminal handles the keypress instead, or it's a no-op)
10. After submit, the sheet closes (slides down) and the textarea is cleared
11. Append mode is verified: if Claude Code has partial text in its own input box, submitting from the compose overlay appends to that partial text rather than replacing it

### Sheet behavior

12. The sheet starts at the configured minimum height (default 80px) when opened with empty content
13. As the user types, the textarea grows in height (and the sheet grows with it) until reaching the configured maximum (default 300px)
14. Beyond the maximum, the textarea scrolls internally; the sheet does not grow further
15. The sheet's height changes are smooth (not jumpy) as content is added or removed

### Terminal interaction underneath

16. The terminal pane underneath the sheet remains visible (the sheet does not cover the entire terminal)
17. The user can click into the terminal to select and copy text while the sheet is open
18. The user can type directly into the terminal (Claude Code's native input) while the sheet is open
19. Clicking back into the textarea returns focus there for continued composition

### Spell-check

20. Misspelled words in the textarea show red squiggles (browser-native spell-check)
21. Right-clicking a misspelled word shows correction suggestions (browser-native context menu)
22. Selecting a correction replaces the word in the textarea

### State manager integration

23. While the compose sheet is open and the textarea has non-empty content, the avatar transitions to Listening (just as if the user were typing in the terminal)
24. When the textarea becomes empty (deleted, submitted, or canceled), the Listening signal from the compose sheet stops; if no other input activity is happening, the avatar transitions back according to the normal state machine rules

### Cross-platform

25. Spell-check works on both Windows (WebView2) and Linux (WebKitGTK) — both provide built-in spell-check via their respective WebView engines

## Implementation Approach

### Frontend: Compose Overlay Component

```
src/lib/
  ComposeOverlay.svelte
  composeState.ts     # store for sheet open/closed state
```

#### `composeState.ts`

```typescript
import { writable } from 'svelte/store';

export const composeOpen = writable<boolean>(false);
export const composeContent = writable<string>('');

export function openCompose() {
    composeOpen.set(true);
}

export function closeCompose() {
    composeOpen.set(false);
    composeContent.set(''); // discard on close
}
```

#### `ComposeOverlay.svelte`

```svelte
<script lang="ts">
  import { tick } from 'svelte';
  import { composeOpen, composeContent, closeCompose } from './composeState';
  import { invoke } from '@tauri-apps/api/core';
  import { settingsStore } from './settings/store';

  let textareaEl: HTMLTextAreaElement;
  let sheetEl: HTMLDivElement;

  // Settings-driven heights
  $: minHeight = $settingsStore.compose.min_height_px;
  $: maxHeight = $settingsStore.compose.max_height_px;

  // Track focus for submit shortcut decisions
  let textareaFocused = false;

  // When the sheet opens, focus the textarea
  $: if ($composeOpen) {
    tick().then(() => textareaEl?.focus());
  }

  // Auto-grow logic
  function adjustHeight() {
    if (!textareaEl) return;
    textareaEl.style.height = 'auto';
    const desired = Math.min(Math.max(textareaEl.scrollHeight, minHeight), maxHeight);
    textareaEl.style.height = `${desired}px`;
  }

  // Submit handler — invoked by the shortcut dispatcher when textarea has focus
  export async function submit() {
    const content = $composeContent;
    if (!content) {
      closeCompose();
      return;
    }
    // Send to PTY: text + newline
    await invoke('pty_write', { input: content + '\n' });
    closeCompose();
  }

  // Cancel handler — invoked by the shortcut dispatcher
  export function cancel() {
    closeCompose();
  }

  function handleInput() {
    adjustHeight();
  }

  // Detect whether textarea has focus, for the dispatcher's submit gate
  function handleFocus() { textareaFocused = true; }
  function handleBlur() { textareaFocused = false; }

  export function isTextareaFocused() {
    return textareaFocused;
  }
</script>

{#if $composeOpen}
  <div class="compose-sheet" bind:this={sheetEl}>
    <textarea
      bind:this={textareaEl}
      bind:value={$composeContent}
      on:input={handleInput}
      on:focus={handleFocus}
      on:blur={handleBlur}
      spellcheck="true"
      placeholder="Compose message... ({@html keyHint('submit')} to send, {@html keyHint('cancel')} to cancel)"
      style="min-height: {minHeight}px; max-height: {maxHeight}px;"
    ></textarea>
  </div>
{/if}

<style>
  .compose-sheet {
    position: absolute;
    bottom: 0;
    left: 0;
    right: 0;
    background: var(--compose-bg, #1e1e1e);
    border-top: 1px solid var(--compose-border, #444);
    box-shadow: 0 -4px 12px rgba(0, 0, 0, 0.3);
    padding: 12px;
    animation: slide-up 200ms ease;
    z-index: 100; /* above terminal but below settings window */
  }

  @keyframes slide-up {
    from { transform: translateY(100%); }
    to { transform: translateY(0); }
  }

  textarea {
    width: 100%;
    box-sizing: border-box;
    font-family: system-ui, -apple-system, sans-serif;
    font-size: 14px;
    color: #e0e0e0;
    background: #2a2a2a;
    border: 1px solid #555;
    border-radius: 4px;
    padding: 10px;
    resize: none;
    outline: none;
  }

  textarea:focus {
    border-color: #6699cc;
  }
</style>
```

Note on the slide-down animation when closing: the example above only animates slide-up. To animate slide-down on close, use a Svelte `transition:` directive instead of a CSS keyframe, which lets Svelte handle both directions and the eventual unmount. Implementation detail; either approach works.

### Wiring the Shortcut Dispatcher

The shortcut dispatcher from Milestone 6 needs to know about compose actions. In the app's main initialization (after settings load):

```typescript
import { configureShortcuts } from './lib/shortcuts/dispatcher';
import { openCompose, composeOpen } from './lib/composeState';
import { openSettings } from './lib/settings/window';
import { composeOverlayRef } from './lib/composeOverlayRef'; // a way to access the component instance

settingsStore.subscribe((settings) => {
    configureShortcuts(settings.shortcuts, {
        open_compose: () => openCompose(),
        submit_compose: () => {
            // Only submit if the textarea has focus
            if (composeOverlayRef.current?.isTextareaFocused()) {
                composeOverlayRef.current.submit();
            }
            // else: do nothing, let the keypress flow normally
        },
        cancel_compose: () => {
            // Only fires while sheet is open
            if (get(composeOpen)) {
                composeOverlayRef.current?.cancel();
            }
        },
        open_settings: () => openSettings(),
    });
});
```

The dispatcher doesn't decide whether to fire — it always invokes the handler when the shortcut matches. The handler decides whether to act based on context (textarea focus, sheet open state). This keeps the dispatcher dumb and the handlers smart.

There's a subtle point about preventDefault: the dispatcher calls `event.preventDefault()` and `stopPropagation()` when a shortcut matches. For `submit_compose` when the textarea doesn't have focus, the handler does nothing — but the dispatcher has already prevented default. This means `Ctrl+Enter` in the terminal would be silently swallowed by the dispatcher rather than reaching xterm.js.

Two solutions:

1. **Conditional dispatch**: the dispatcher checks a "should this shortcut fire right now" predicate before preventing default. The submit predicate is "textarea has focus."
2. **Always dispatch, accept the swallow**: `Ctrl+Enter` is rarely meaningful inside Claude Code anyway, so swallowing it is fine.

Option 1 is cleaner. Implement the dispatcher with optional per-shortcut "active" predicates:

```typescript
configureShortcuts(settings.shortcuts, {
    open_compose: { handler: openCompose, active: () => true },
    submit_compose: { handler: submitFn, active: () => textareaHasFocus() },
    cancel_compose: { handler: cancelFn, active: () => composeIsOpen() },
    open_settings: { handler: openSettings, active: () => true },
});
```

The dispatcher only calls `preventDefault` when the active predicate is true. If false, the event flows normally.

### State Manager Integration

The state manager needs a new signal source: "compose textarea has non-empty content."

Add a new signal:

```rust
pub enum StateSignal {
    // ... existing variants
    ComposeContentChanged { non_empty: bool },
}
```

The frontend emits this whenever the textarea content changes from empty to non-empty or vice versa:

```typescript
let lastNonEmpty = false;
composeContent.subscribe((content) => {
    const nonEmpty = content.length > 0;
    if (nonEmpty !== lastNonEmpty) {
        lastNonEmpty = nonEmpty;
        invoke('compose_content_changed', { nonEmpty });
    }
});
```

The Tauri command forwards to the state manager:

```rust
#[tauri::command]
async fn compose_content_changed(state: State<'_, AppState>, non_empty: bool) -> Result<(), String> {
    state.signal_tx.send(StateSignal::ComposeContentChanged { non_empty }).await.ok();
    Ok(())
}
```

The state manager's logic:

- `ComposeContentChanged { non_empty: true }` from any state → Listening (similar to UserInput)
- `ComposeContentChanged { non_empty: false }` doesn't immediately transition; it just removes the "compose-active" condition. If no other active conditions hold, the existing rules drive the transition.

This requires a small refactor of the state manager to track "user is currently composing" as a flag rather than computing it from a single signal — similar to the existing `claude_still_generating` flag.

### Compose-related Settings

The compose section in the settings window:

```
Compose
  Min height (px):  [80]
  Max height (px):  [300]
```

These flow through the settings store as previously specified. The textarea reads them reactively for its `style` attribute.

## Validation Steps

### Basic functionality

1. **Trigger**: press `Ctrl+Shift+E` (or configured shortcut). Verify the sheet slides up from the bottom of the window.
2. **Focus**: verify the textarea has focus immediately after opening; typing produces text in the textarea.
3. **Cancel via Escape**: press Escape. Verify the sheet closes and the draft is discarded (re-opening shows empty).
4. **Cancel from terminal focus**: open the sheet, click into the terminal, then press Escape. Verify the sheet closes.
5. **Submit**: type a message, press `Ctrl+Enter`. Verify the message is sent to Claude Code (appears in Claude Code's history) and the sheet closes.
6. **Submit only with textarea focus**: open the sheet, click into the terminal, press `Ctrl+Enter`. Verify the sheet does NOT submit (the keypress flows to the terminal/Claude Code as normal).

### Auto-grow

7. **Initial size**: open the sheet with no text. Verify the textarea is at the minimum configured height.
8. **Growth**: type several lines. Verify the textarea grows up to the maximum height.
9. **Internal scroll beyond max**: keep typing past the maximum height. Verify the textarea scrolls internally and the sheet doesn't grow further.

### Spell-check

10. **Squiggles**: type a misspelled word. Verify red squiggles appear under it.
11. **Corrections**: right-click a misspelled word. Verify correction suggestions appear in a context menu.
12. **Apply correction**: select a correction. Verify the word is replaced.

### Terminal interaction

13. **Terminal still visible**: open the sheet. Verify the terminal pane is still visible above the sheet.
14. **Terminal selection**: open the sheet. Click and drag in the terminal to select text. Verify the selection works and the text can be copied via right-click or Ctrl+C-equivalent.
15. **Paste from terminal**: select text in the terminal, copy it, click into the textarea, paste. Verify the text appears in the textarea.
16. **Terminal typing while sheet is open**: open the sheet, click into the terminal, type a message there directly. Verify Claude Code receives it normally. Then click back into the textarea, finish composing, submit. Verify both messages went through.

### Append mode

17. **Append behavior**: type partial text directly into Claude Code's input. Open the compose sheet. Type more text. Submit. Verify the submitted message contains both the partial direct text AND the composed text concatenated (this is append mode behavior).

### State integration

18. **Listening on compose**: open the sheet. Type in the textarea (non-empty content). Verify the avatar transitions to Listening.
19. **Idle on empty**: clear the textarea (delete all content). Verify (after the typing-stopped timeout) the avatar returns to Idle if Claude isn't responding.
20. **Submit transitions**: type in the textarea, submit. Verify the avatar progresses through Thinking → Speaking based on Claude's response, just like a normal terminal-driven message.

### Configuration

21. **Custom shortcuts**: change the `open_compose` shortcut in settings to something else (e.g., `Ctrl+Alt+C`). Verify the new shortcut opens the sheet and the old one no longer does.
22. **Custom heights**: change min and max heights in settings. Verify the textarea respects the new bounds.
23. **Cleared shortcut**: clear the `open_compose` shortcut in settings. Verify there's no way to open the sheet via keyboard (since there's no other UI for it). This is acceptable — the user explicitly disabled the feature.

### Cross-platform

24. **Windows**: validate spell-check, animation smoothness, and terminal interaction underneath on Windows (WebView2).
25. **Linux**: validate the same on Linux (WebKitGTK). Spell-check dictionary may differ; both should provide some level of spell-check.

## Known Risks and Mitigation

- **Submit shortcut conflict with terminal**: as discussed, `Ctrl+Enter` while focus is in the terminal could be relevant for some terminal use cases (rare). The active-predicate dispatcher avoids swallowing keys when the textarea isn't focused.
- **Spell-check dictionary differences**: Windows and Linux WebViews use different spell-check engines and dictionaries. Quality may vary. If a user finds the default spell-check unhelpful, we'd need to investigate adding a custom engine — out of scope for v1.
- **Animation jank**: slide-up animation is brief (200ms). Should be smooth. If WebKitGTK shows jank here, simplify or remove the animation on Linux.
- **Compose sheet covering terminal output**: the sheet is positioned absolutely at the bottom and overlays terminal content. Important terminal output might be obscured while the sheet is open. The user can resize the window or close the sheet to see; this is acceptable.
- **Focus switching feels awkward**: clicking between terminal and textarea is a bit of context-switching. The auto-focus on open and the ability to click anywhere should make it work. If users find it annoying, consider Tab to switch focus between terminal and textarea — defer to polish.
- **Append mode + accidental Claude Code input**: if a user has typed `gibberish` directly into Claude Code's input and forgotten about it, then composes a real message in the sheet and submits, the result is `gibberishreal message` going to Claude Code. The user has to clear the direct input themselves before submitting. We chose append mode over replace mode deliberately to avoid the wrapper interfering with Claude Code's input state, but this is the consequence. Document in the README.

## What "Done" Looks Like

The user has a fast, reliable way to compose longer messages with spell-check assistance, without losing access to the terminal underneath. Short messages, slash commands, and quick replies still flow through Claude Code's native input. Longer prose-heavy messages get composed in the sheet. Both pathways coexist cleanly. Shortcuts can be customized to whatever the user prefers.

---

## Next Milestone

Milestone 8: Polish. Error states, edge cases, cross-platform validation, performance tuning, and any remaining quality-of-life improvements. The application should feel finished after this milestone.
