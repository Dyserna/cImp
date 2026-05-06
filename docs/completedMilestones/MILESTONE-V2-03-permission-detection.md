# Milestone V2-03: Permission Detection and Tab Status

## Goal

Add detection of Claude Code's permission prompts via exact-string matching, the new `AwaitingPermission` flag in tab state, and the visual rendering of tab status indicators on the tab bar (Working continuous, AwaitingPermission, Error, DoneWhileAway). Aider permission patterns are out of scope for v2 — this milestone focuses on Claude Code only and lays the groundwork (the detection module accepts patterns as a configurable list, so aider patterns can be added later as a small data-only change).

## Why This Milestone Now

Multi-tab is functional after V2-01 and polished after V2-02. The next layer of value is making cross-tab status visible — knowing what's happening in the tab you're not currently looking at. Tab status indicators provide that visibility, and permission detection is the most actionable status to surface.

## Scope

### In Scope

- A permission pattern detector in the processing layer
- Exact-string matching against a hardcoded list of Claude Code's permission prompt patterns
- A new `AwaitingPermission` boolean flag on each tab's state (independent of `avatar_state`)
- Detection of permission prompt resolution (when the user provides input that resolves the prompt)
- A new `DoneWhileAway` boolean flag (UI-derived) on each tab's state
- Tab status indicator rendering in the tab bar:
  - Working: continuous subtle indicator (small dot or pulse) when tab's avatar state is Thinking or Speaking
  - AwaitingPermission: prominent indicator (orange/yellow, possibly pulsing) when the flag is set
  - Error: prominent indicator (red) when avatar state is Error
  - DoneWhileAway: subtle indicator (green dot) when the flag is set; clears on tab activation
- Indicator priority logic: highest-severity flag is shown when multiple flags are set
- The `done_while_away` flag is set on Idle transitions for inactive tabs and cleared on tab activation
- Pattern list is structured for easy updates (single config constant or JSON file in the repo)

### Out of Scope

- Aider permission patterns (deferred — Claude only in this milestone)
- Permission detection for prompts other than tool-use approval (e.g., aider's own confirm-changes prompts come later)
- Notification system (V2-04) — flags are set in this milestone but the audible notifications layer is added in V2-04
- Smart Working indicator that hides when the user is on the active tab (per design discussion, ship continuous; revisit if it feels noisy)
- Animated/pulsing indicators with sophisticated timing (use simple CSS animations only)

## Acceptance Criteria

### Permission detection (Claude Code)

1. When Claude Code displays a permission prompt (e.g., asking to use a tool), the processing layer detects it within the standard flush window
2. On detection, the Claude tab's `awaiting_permission` flag is set to true
3. When the user provides input to the Claude tab (any keystroke), the flag is cleared
4. The flag clearing logic is robust to false positives — keystrokes typed while no prompt is active should not affect anything (the flag is already false; no-op)
5. The detection works for at least the most common Claude Code permission prompt patterns:
   - Tool-use approval prompts (the typical "Do you want to allow Claude to use the X tool?" form)
   - Other recognizable prompt patterns characterized at implementation time
6. Pattern matching uses the rendered (ANSI-stripped) view of the recent terminal output, not the raw byte stream

### `DoneWhileAway` flag

7. When a tab's avatar state transitions to Idle while the tab is inactive (i.e., `state.active != tab` at the moment of transition), the tab's `done_while_away` flag is set to true
8. When the user activates a tab, that tab's `done_while_away` flag is cleared (whether or not it was set)
9. The active tab never has `done_while_away` set — transitions to Idle on the active tab don't set the flag
10. The flag persists across multiple state changes if the tab remains inactive (e.g., if the tab transitions Idle → Working → Idle while inactive, the flag stays set)

### Tab status indicator rendering

11. Each tab's tab-bar entry renders an indicator based on its current flags. Priority order (highest wins):
    1. Error (red)
    2. AwaitingPermission (orange/yellow, with subtle pulse animation)
    3. DoneWhileAway (green dot)
    4. Working (subtle small dot in tab text color, gentle pulse)
    5. None (no indicator)
12. The active tab's indicator: Error, AwaitingPermission, and Working still show. DoneWhileAway is hidden (cleared on activation).
13. Indicators are visually distinct enough to identify at a glance (color, position, animation)
14. Indicators are positioned consistently within each tab entry (e.g., a small dot to the left of the tab label, or a colored border)
15. State changes update the indicator immediately (within one render frame)

### Cross-platform

16. Indicator rendering is consistent on both Windows (WebView2) and Linux (WebKitGTK)
17. Pulse animations don't cause performance issues on either platform

## Implementation Approach

### Permission Pattern Detection

#### Pattern characterization (one-time research task)

Before implementation, do a small research task:

1. Run Claude Code interactively and trigger several permission prompts (use the bash tool, edit a file, etc., to provoke the prompt UI)
2. Capture the rendered text of each prompt (e.g., by running cctts and logging the rendered-view content from the processing layer)
3. Identify distinctive substrings that uniquely identify a permission prompt
4. Document these patterns in a constants file with a comment noting which Claude Code version was tested

Likely pattern characteristics to look for:

- The phrase asking for permission (specific wording)
- The presence of numbered or labeled choice options
- Specific Unicode box-drawing or other special characters Claude Code uses in its prompt UI

The patterns should be specific enough to avoid false positives in normal output but general enough to handle minor cosmetic variations.

#### Pattern data structure

```rust
pub struct PermissionPattern {
    pub name: &'static str,
    pub substring: &'static str,
    pub description: &'static str,
}

pub const CLAUDE_PERMISSION_PATTERNS: &[PermissionPattern] = &[
    PermissionPattern {
        name: "tool_use_approval",
        substring: "Do you want to proceed?",  // example placeholder
        description: "Standard tool-use approval prompt",
    },
    // ... more patterns characterized at implementation time
];
```

A const array is fine for v2. If the patterns ever need to be data-driven (e.g., loaded from a JSON file so users can update without recompiling), refactor at that point.

#### Detector implementation

Add to the processing layer:

```rust
pub struct PermissionDetector {
    patterns: &'static [PermissionPattern],
    last_detected: Option<&'static str>,  // name of the pattern most recently detected
}

impl PermissionDetector {
    pub fn new(patterns: &'static [PermissionPattern]) -> Self;
    
    pub fn check_for_permission_prompt(&mut self, rendered_text: &str) -> PermissionDetectorResult;
    
    pub fn clear_on_input(&mut self) -> bool;  // returns true if the flag was set
}

pub enum PermissionDetectorResult {
    None,
    Detected { pattern_name: &'static str },
    Resolved,  // a previously-detected pattern is no longer in the rendered text
}
```

The detector is invoked after each flush from the processing layer. It scans the recently-rendered text for any of its configured patterns. If a pattern is matched and `last_detected` was None, returns `Detected`. If `last_detected` was Some and no patterns are now matched, returns `Resolved`. Otherwise returns `None`.

The state manager listens for these and updates the tab's `awaiting_permission` flag accordingly.

For input-driven clearing: the IPC `pty_write` handler also informs the state manager. The state manager clears `awaiting_permission` for the tab whose input was received, regardless of whether the detector also reports Resolved. The two clearing mechanisms (input-driven and detector Resolved) are both safe — clearing an already-false flag is a no-op.

#### Pattern matching specifics

The patterns use simple `String::contains` checks for v2 — exact substring matching against the rendered text. No regex, no fuzzy matching. This is brittle (formatting changes break it) but predictable.

Worth flagging in code comments: if Claude Code changes its prompt text in a future release, the patterns may need updating. The README should note this as a known limitation.

#### Where the rendered text comes from

The processing layer's vte parser already maintains a rendered view (per the v1 design). The permission detector consumes a window of recent rendered text — say, the last 1000 characters. This avoids scanning the entire scrollback on every check while ensuring multi-line prompts are still captured.

### State Manager Updates

Add the new flag to `TabState`:

```rust
pub struct TabState {
    pub avatar_state: AvatarState,
    pub awaiting_permission: bool,  // NEW: set/cleared by permission detection
    pub done_while_away: bool,      // NEW: UI-derived flag
    pub claude_still_generating: bool,
}
```

Add new signals:

```rust
pub enum StateSignal {
    // ... existing variants
    PermissionPromptDetected { tab: TabId },
    PermissionPromptResolved { tab: TabId },
    // (UserInput signal already exists; reuse it for input-driven clearing)
}
```

Update signal handling:

```rust
pub async fn handle_signal(&mut self, signal: StateSignal) {
    let tab_id = signal.tab();
    
    match &signal {
        StateSignal::PermissionPromptDetected { tab } => {
            let tab_state = self.tabs.get_mut(tab).unwrap();
            if !tab_state.awaiting_permission {
                tab_state.awaiting_permission = true;
                let _ = self.state_tx.send(StateEvent::AwaitingPermissionChanged {
                    tab: *tab, awaiting: true,
                });
            }
        }
        StateSignal::PermissionPromptResolved { tab } | StateSignal::UserInput { tab } => {
            let tab_state = self.tabs.get_mut(tab).unwrap();
            if tab_state.awaiting_permission {
                tab_state.awaiting_permission = false;
                let _ = self.state_tx.send(StateEvent::AwaitingPermissionChanged {
                    tab: *tab, awaiting: false,
                });
            }
        }
        _ => { /* fall through to standard avatar state machine */ }
    }
    
    // ... existing per-tab avatar state transition logic
    
    // Handle DoneWhileAway flag updates
    self.update_done_while_away(&signal);
}

fn update_done_while_away(&mut self, signal: &StateSignal) {
    // Set: when a tab transitions to Idle and is not the active tab
    // Clear: when the user activates a tab
    
    match signal {
        StateSignal::TabActivated { tab } => {
            let tab_state = self.tabs.get_mut(tab).unwrap();
            if tab_state.done_while_away {
                tab_state.done_while_away = false;
                let _ = self.state_tx.send(StateEvent::DoneWhileAwayChanged {
                    tab: *tab, done: false,
                });
            }
        }
        _ => {
            // Check if any tab transitioned to Idle and is inactive
            for (tab_id, tab_state) in &mut self.tabs {
                if tab_state.avatar_state == AvatarState::Idle && *tab_id != self.active && !tab_state.done_while_away {
                    // Was this transition fresh (i.e., did it just happen)?
                    // We need to know whether this signal caused the Idle transition.
                    // Tracking: compare avatar_state before/after the standard signal handling.
                    // Simpler: detect the transition in the avatar state machine handler itself,
                    // and call this from there.
                }
            }
        }
    }
}
```

The cleanest implementation: emit the `DoneWhileAwayChanged` event from inside the per-tab avatar state transition logic, when the transition is to Idle and the tab is not active. This avoids separate logic chasing the same condition.

### Frontend: Tab Status Indicators

#### Tab component update

```svelte
<!-- Tab.svelte -->
<script lang="ts">
  export let label: string;
  export let active: boolean;
  export let avatarState: AvatarState;       // 'Idle' | 'Listening' | 'Thinking' | 'Speaking' | 'Error'
  export let awaitingPermission: boolean;
  export let doneWhileAway: boolean;
  
  $: indicator = computeIndicator(avatarState, awaitingPermission, doneWhileAway, active);
  
  function computeIndicator(
    avatarState: AvatarState,
    awaitingPermission: boolean,
    doneWhileAway: boolean,
    active: boolean
  ): { type: string; class: string } | null {
    // Priority: error > awaiting_permission > done_while_away > working > none
    if (avatarState === 'Error') {
      return { type: 'error', class: 'indicator-error' };
    }
    if (awaitingPermission) {
      return { type: 'awaiting_permission', class: 'indicator-awaiting' };
    }
    if (doneWhileAway && !active) {
      return { type: 'done_while_away', class: 'indicator-done' };
    }
    if (avatarState === 'Thinking' || avatarState === 'Speaking') {
      return { type: 'working', class: 'indicator-working' };
    }
    return null;
  }
</script>

<button class="tab" class:active on:click>
  {#if indicator}
    <span class="indicator {indicator.class}"></span>
  {/if}
  <span class="label">{label}</span>
</button>

<style>
  .tab {
    display: inline-flex;
    align-items: center;
    gap: 8px;
    padding: 6px 16px;
    background: transparent;
    border: none;
    color: var(--tab-text, #ccc);
    cursor: pointer;
    border-bottom: 2px solid transparent;
  }
  .tab.active {
    background: var(--tab-active-bg, #1e1e1e);
    color: var(--tab-active-text, #fff);
    border-bottom-color: var(--tab-active-border, #6699cc);
  }
  .indicator {
    display: inline-block;
    width: 8px;
    height: 8px;
    border-radius: 50%;
  }
  .indicator-working {
    background: var(--tab-text, #ccc);
    animation: pulse-subtle 1.5s ease-in-out infinite;
  }
  .indicator-awaiting {
    background: #f0a020;
    animation: pulse-strong 1s ease-in-out infinite;
  }
  .indicator-done {
    background: #4caf50;
  }
  .indicator-error {
    background: #e74c3c;
  }
  
  @keyframes pulse-subtle {
    0%, 100% { opacity: 0.4; }
    50% { opacity: 0.9; }
  }
  @keyframes pulse-strong {
    0%, 100% { opacity: 0.6; transform: scale(1); }
    50% { opacity: 1; transform: scale(1.2); }
  }
</style>
```

#### Subscribing to state events

The frontend listens for the new state events:

```typescript
// In tabs/state.ts
import { writable } from 'svelte/store';
import { listen } from '@tauri-apps/api/event';

export const tabStates = writable<Record<string, TabUIState>>({
  claude: { avatarState: 'Idle', awaitingPermission: false, doneWhileAway: false },
  aider: { avatarState: 'Idle', awaitingPermission: false, doneWhileAway: false },
});

listen<{ tab: string; state: AvatarState }>('state-changed', (event) => {
  tabStates.update(s => ({
    ...s,
    [event.payload.tab]: { ...s[event.payload.tab], avatarState: event.payload.state },
  }));
});

listen<{ tab: string; awaiting: boolean }>('awaiting-permission-changed', (event) => {
  tabStates.update(s => ({
    ...s,
    [event.payload.tab]: { ...s[event.payload.tab], awaitingPermission: event.payload.awaiting },
  }));
});

listen<{ tab: string; done: boolean }>('done-while-away-changed', (event) => {
  tabStates.update(s => ({
    ...s,
    [event.payload.tab]: { ...s[event.payload.tab], doneWhileAway: event.payload.done },
  }));
});
```

The TabBar passes the relevant state to each Tab:

```svelte
{#each tabs as tab}
  <Tab
    label={tab.label}
    active={$activeTab === tab.id}
    avatarState={$tabStates[tab.id].avatarState}
    awaitingPermission={$tabStates[tab.id].awaitingPermission}
    doneWhileAway={$tabStates[tab.id].doneWhileAway}
    on:click={() => switchTab(tab.id)}
  />
{/each}
```

### Pattern Update Mechanism

To make pattern updates easy as Claude Code evolves:

- Patterns live in `src-tauri/src/processing/permission_patterns.rs` as a documented `const`
- A comment block at the top notes which Claude Code version was tested
- When patterns need updating (Claude Code changes its prompts), update the const, recompile, ship a new version of cctts

For v2 this is sufficient. If pattern updates become frequent, consider loading from a JSON file the user can edit, but that's not needed yet.

## Validation Steps

### Permission detection

1. **Pattern characterization first**: before testing, do the research task — run Claude Code, trigger permission prompts, capture distinctive text. Update the patterns const.
2. **Detection on the active tab**: in the Claude tab, ask Claude to do something that triggers a permission prompt (e.g., "edit the file at /tmp/test.txt with random content" or use a tool that requires approval). Verify the tab's `awaiting_permission` flag becomes true (visible via the orange/yellow indicator).
3. **Resolution on input**: when the prompt is showing, type a response (yes or no). Verify the flag clears immediately.
4. **Detection on the inactive tab**: trigger a permission prompt on the Claude tab, then quickly switch to the aider tab. Verify the Claude tab's indicator (visible in the tab bar) shows AwaitingPermission while you're on the aider tab.
5. **No false positives**: have Claude generate a long prose response that doesn't include any permission-prompt-like text. Verify the flag does NOT get set spuriously. (If it does, the patterns are too loose; refine.)
6. **Repeated prompts**: trigger several permission prompts in a row. Verify the flag toggles correctly each time.

### DoneWhileAway flag

7. **Set on inactive idle**: while on the aider tab, send a message to the Claude tab beforehand. Wait for Claude to finish responding (Idle transition). Verify the Claude tab's indicator shows the green DoneWhileAway dot.
8. **Cleared on activation**: switch to the Claude tab. Verify the green dot disappears immediately.
9. **Not set on active idle**: stay on the Claude tab, send a message, wait for completion. Verify no green dot appears (the tab is active throughout).
10. **Persists across multiple inactive transitions**: while on the aider tab, send a Claude message. Wait for completion (DoneWhileAway sets). Send another message to the Claude tab via... actually, this is hard to test because you have to be on Claude to send to it. Skip this case; the simpler flag-set-once-and-cleared-on-activation behavior covers the realistic use case.

### Indicator rendering

11. **Working indicator (continuous)**: while on the Claude tab and Claude is generating, verify a subtle pulse indicator on the Claude tab. Switch to aider while Claude is still working — verify the indicator is still visible from the aider perspective.
12. **AwaitingPermission visual**: when set, verify the orange/yellow indicator with a stronger pulse than Working.
13. **Error visual**: kill the Claude subprocess. Verify the red indicator on the Claude tab.
14. **Priority ordering**: somehow get a tab into both Error and AwaitingPermission states (kill aider while it had a permission prompt — hypothetical edge case). Verify Error wins over AwaitingPermission.
15. **No indicator when idle**: with Claude in the Idle state and active, verify no indicator on the Claude tab.

### Cross-platform

16. Verify all of the above on the second platform. Animations should be smooth on both.
17. Verify the indicator colors are visible and distinct on both platforms (color rendering can vary slightly).

## Known Risks and Mitigation

- **Pattern brittleness**: Claude Code may change its prompts. Mitigation: comment-documented patterns, easy to update. README acknowledges the limitation.
- **Detection false positives**: if patterns are too generic, normal Claude output might match. Mitigation: characterize patterns carefully during the research task; pick text distinctive enough to be unambiguous. If false positives occur in practice, refine.
- **Detection false negatives**: subtle prompt variations might not match. Same mitigation as false positives — refine patterns based on observed behavior.
- **Animation performance**: pulse animations on multiple tabs simultaneously could be a small CPU cost on lower-end systems. Negligible on the target user's 5090. If it ever matters, simplify the animations.
- **Resolved-via-no-longer-visible logic**: the detector can use "the prompt text is no longer in the rendered window" as a Resolved trigger, but this depends on the rendered window size and how Claude Code clears prompts. Input-driven clearing is more reliable; consider input-driven primary, detector-driven secondary.
- **DoneWhileAway timing edge case**: very fast Idle → not-Idle → Idle transitions while inactive might leave the flag set briefly even if the user wasn't really "away during a finish" in any meaningful sense. Acceptable; the flag has the right semantics in normal cases.

## What "Done" Looks Like

When you're on the aider tab and Claude needs your attention (asking permission to use a tool, finished a task, hit an error), you can see at a glance from the tab bar that something's going on with the Claude tab — without having to switch over to check. When you switch to a tab with a notification-worthy state, the indicator clears appropriately. The tab bar becomes a real status indicator for the system, not just a navigation tool.

---

## Next Milestone

Milestone V2-04: Notifications and Status Bar. Adds the audible notification system (per-tab dedup at play-time, configurable text), the notification queue logic, and the bottom status bar with mute / announcements / volume controls.
