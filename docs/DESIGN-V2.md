# Design Document: cctts v2

## Purpose of This Document

This document captures the architecture and design decisions for cctts v2 — the multi-tab evolution of the v1 design. It supersedes `DESIGN.md` (v1) as the current architectural truth. The v1 document remains as a historical record of the v1 architecture as it existed at v1 ship.

When this document conflicts with `DESIGN.md` (v1), this document wins. Where v1 design elements are unchanged in v2, this document references them rather than restating them — read both together for the complete picture.

The audience is Claude Code working on v2 implementation, plus any human reviewer.

---

## What v2 Adds

v1 shipped as a single-tab Claude Code wrapper with TTS, an animated avatar overlay, a compose overlay, and configurable shortcuts. v2 expands this into a multi-tab architecture, adding:

1. **Tab bar** at the top of the window with multiple tabs
2. **Two tabs** in v2: Claude Code (existing, default active) and aider (new)
3. **Per-tab PTY and processing state**: each tab independently spawns a subprocess and routes output through its own processing layer
4. **Tab status indicators** with continuous Working state, AwaitingPermission, Error, and DoneWhileAway flags
5. **Permission prompt detection** for Claude Code via exact-string matching against known prompt patterns (extending later to aider once aider's patterns are characterized)
6. **Notification system**: when a tab's state changes and the user is on a different tab, an audible announcement plays. Notifications queue, dedupe per tab at play-time, and respect ongoing TTS.
7. **Bottom status bar** with mute TTS, disable announcements, and volume controls on the right side. Left side reserved for future text display.
8. **Active-tab-only TTS**: TTS plays only for the active tab; switching tabs stops the current speech and discards any queued segments from the previously-active tab.
9. **Active-tab-only avatar state**: the avatar reflects only the active tab's state.
10. **Tab keyboard shortcuts**: configurable shortcuts for switching directly to specific tabs.

The aider tab in v2 ships with reduced TTS coverage. See the "Aider TTS limitations" section below.

## What v2 Does NOT Change

The following v1 components are unchanged in v2:

- The PTY-based architecture for embedding interactive subprocesses
- The processing layer's vte-based parsing, hybrid flush trigger, and TTS tag detection
- The TTS pipeline (Kokoro via ONNX Runtime, sentence-boundary segmentation, audio queue via cpal+rodio)
- The avatar overlay (floating in a configurable corner with configurable size, position, margin, and opacity)
- The waveform visualizer (sibling of the avatar, independent opacity)
- The compose overlay (bottom sheet with browser-native spell-check, append-mode submission)
- The settings store (JSON persistence, debounced save, broadcast on change)
- The CLAUDE.md TTS markup convention itself (the `[[TTS]]...[[/TTS]]` tags)
- The cross-platform stack (Rust+Tauri+Svelte, Windows+Linux)

Refer to v1 `DESIGN.md` for details on these.

---

## Architecture Changes

### Multi-Tab Foundation

In v1, cctts owns a single PTY (running `claude`) and a single processing layer. v2 generalizes this so cctts owns N tabs, each with its own PTY and processing layer. Currently N=2 (Claude Code, aider) but the architecture supports more without rework — a future v3 might add tabs for general shells or other terminal-based tools.

#### Per-tab state

Each tab owns:

- A PTY pair and a child process (claude, aider, or whatever was configured)
- A processing layer instance (vte parser, flush state, tag detector, segmenter)
- An xterm.js instance in the frontend (one per tab, only the active one is rendered)
- A logical avatar state (Idle, Listening, Thinking, Speaking, Error) — each tab has its own
- A logical TTS queue (pending text segments to be synthesized)
- Audio-related queue state (synthesis-pending and playback-pending segments)
- Tab-level UI flags (status color, "DoneWhileAway" pending notification, etc.)

#### Active tab routing

Exactly one tab is active at any time. The active tab determines:

- Which xterm.js instance receives input from the user's keyboard
- Which xterm.js instance is visible in the terminal area
- Which tab's avatar state drives the displayed avatar
- Which tab's TTS gets played (background tabs' synthesis is skipped entirely — see below)

Switching tabs:

- Stops audio playback immediately. The currently-playing buffer is interrupted (rodio `Sink::clear()`).
- Discards the previously-active tab's pending synthesis queue. Segments that hadn't been synthesized yet are dropped, not held for later.
- Activates the new tab's xterm.js instance and routes keyboard input to its PTY.
- The avatar reflects the new tab's current state, with no transition animation triggered by the switch itself (transitions happen only on actual state changes, not on tab activation).

The reasoning: TTS reflects what's currently shown. Holding queued synthesis from a previous tab would mean spoken content played out-of-context after the user has switched away. Discarding is correct.

#### Background tabs

Background (non-active) tabs continue to:

- Run their PTY subprocess
- Process incoming bytes through the processing layer (so terminal state is up-to-date when you switch back)
- Track their avatar state via the state machine
- Trigger notifications when state changes meet the notification criteria (see below)

Background tabs do NOT:

- Synthesize TTS (synthesis is skipped to save GPU cycles)
- Play audio
- Render their xterm.js instance to the visible terminal area (they're hidden via CSS until activated)

When you switch to a background tab, its accumulated terminal state is immediately visible (xterm.js has been receiving and processing bytes the whole time). New TTS-marked content from that point forward is synthesized and played normally; older marked content from before the switch is not retroactively spoken.

### Per-Tab Eager PTY Spawn

Both tab subprocesses spawn at app launch, not on first activation. The Claude tab spawns `claude`, the aider tab spawns `aider`, both in the launch directory. This means:

- Switching to the aider tab the first time has no startup delay
- Both subprocesses consume resources from app launch onward
- If aider isn't installed or fails to launch, the error is surfaced at startup, not on first tab activation

The eager-spawn decision was made because v2's two tabs are both well-defined AI tools that are expected to be used. For v3 if user-managed tab profiles are added (where many tabs might exist but few get used per session), lazy spawn becomes more attractive. v2 stays eager for simplicity.

### State Manager Refactor

The v1 state manager tracked one current state. v2's state manager tracks per-tab state plus an active-tab pointer.

```rust
pub struct StateManager {
    tabs: HashMap<TabId, TabState>,
    active: TabId,
    state_tx: broadcast::Sender<StateEvent>,
}

pub struct TabState {
    avatar_state: AvatarState,  // Idle | Listening | Thinking | Speaking | Error
    awaiting_permission: bool,  // independent flag (see below)
    done_while_away: bool,      // UI flag (see below)
    claude_still_generating: bool,  // existing v1 helper flag, now per-tab
}

#[derive(Clone, Debug)]
pub enum StateEvent {
    StateChanged { tab: TabId, state: AvatarState },
    AwaitingPermissionChanged { tab: TabId, awaiting: bool },
    DoneWhileAwayChanged { tab: TabId, done: bool },
    ActiveTabChanged { tab: TabId },
    NotificationFired { tab: TabId, event: NotificationEvent },
}
```

Signals are tagged with the tab they originated from:

```rust
pub enum StateSignal {
    UserInput { tab: TabId },
    UserInputStopped { tab: TabId },
    ClaudeOutputStarted { tab: TabId },
    ClaudeOutputStopped { tab: TabId },
    TtsPlaybackStarted { tab: TabId },
    TtsPlaybackStopped { tab: TabId },
    PermissionPromptDetected { tab: TabId },
    PermissionPromptResolved { tab: TabId },
    SubprocessExited { tab: TabId },
    AudioError { tab: TabId },
    TtsError { tab: TabId },
    ErrorAcknowledged { tab: TabId },
    ComposeContentChanged { tab: TabId, non_empty: bool },
    TabActivated { tab: TabId },
}
```

The state machine logic for each tab is the same as v1's logic — `(current_state, signal) -> new_state`. The manager just runs that logic per tab.

The `claude_still_generating` flag (used to disambiguate Speaking → Thinking vs Speaking → Idle) is now per-tab.

The `awaiting_permission` flag is *independent* of the avatar state — a tab can be Thinking AND awaiting permission, or Idle AND awaiting permission, etc. It's tracked separately because it's used for tab status indicators and notifications, not the avatar's primary state.

### Tab Status Indicators

Each tab's tab-bar entry displays visual status based on its tab state plus active-tab-aware flags.

#### Status flags driving tab indicators

| Flag | Source | When set | When cleared |
|------|--------|----------|--------------|
| `working` | `TabState.avatar_state == Thinking \|\| Speaking` | When tab is generating output or speaking | When tab returns to Idle (or other non-active state) |
| `awaiting_permission` | `TabState.awaiting_permission` | When permission prompt detected | When prompt resolves (user makes choice in that tab) |
| `error` | `TabState.avatar_state == Error` | On error | When error acknowledged |
| `done_while_away` | UI-derived | When tab transitions to Idle from non-Idle while it was inactive | When user activates the tab |

The displayed indicator is the highest-severity flag currently set, with this priority:

1. Error (highest) — red
2. AwaitingPermission — orange/yellow, possibly pulsing
3. DoneWhileAway — green dot, fades after viewed
4. Working — subtle indicator (e.g., a small dot in tab text color)
5. None (idle, no special state) — default tab styling

The active tab's `done_while_away` flag is always cleared (it doesn't make sense for the currently-viewed tab to indicate "done while you weren't looking"). When you switch tabs, the newly-active tab's `done_while_away` is cleared on activation.

The active tab's `working` indicator still shows even though the user is on that tab. This is intentional per design discussion — the indicator's job includes "the system is alive and working" even when not surfacing cross-tab notification value. May be revisited in v2 polish if it feels noisy.

### Permission Prompt Detection

The processing layer is extended to detect Claude Code's permission prompts via exact-string matching against known prompt patterns.

Claude Code's permission prompts have recognizable text. The exact strings need to be characterized at implementation time (run Claude Code, observe the prompts that appear when it asks for tool permission, capture their distinctive text). Examples of patterns to look for include:

- Specific phrases like "Do you want to proceed?" appearing in a yes/no context
- Numbered choice prompts (e.g., "1. Yes" / "2. No")
- Specific Unicode box-drawing characters that appear in Claude Code's permission UI

The detector logic:

1. The processing layer maintains the rendered-view text as part of its existing operation
2. After each flush, the detector scans the recently-rendered region for known prompt patterns
3. On detection: emits `PermissionPromptDetected { tab: TabId }` to the state manager
4. On detection of input that resolves the prompt (user types a choice and it disappears): emits `PermissionPromptResolved`

Resolution detection is harder than detection. Two simpler approaches:

- **Track-and-wait**: when a prompt is detected, set the flag. When the prompt text is no longer in the rendered view (because the screen has scrolled or rewritten past it), assume it was resolved.
- **Input-driven**: when a permission prompt is active and the user provides input to the PTY, assume the input was the prompt response and clear the flag.

The second is simpler and probably correct in practice — the user's input either resolves the prompt or makes Claude Code reprompt anyway. Use the second.

For aider, permission patterns are different and need their own characterization. v2's first cut implements only Claude Code's patterns. Aider patterns get added once Claude Code's implementation is solid and the patterns can be observed.

#### Pattern brittleness

Exact-string matching is brittle to upstream changes. If Claude Code updates its prompt UI text, the detector breaks. Mitigations:

- Keep the patterns in a single, well-commented configuration constant in code (or a small data file) so updates are localized
- Document in code comments which Claude Code version was tested
- The README should mention that permission detection may lag if Claude Code changes its prompts; this is a known limitation

A future improvement would be a more structural detector (e.g., recognizing "the cursor is positioned over a yes/no choice with a highlighted option" rather than matching exact text), but that's significantly harder and deferred.

### Notification System

When a tab's state changes in a way that's interesting from the user's perspective AND the user is on a different tab, an audible notification announces the change.

#### Notification triggers

Triggers fire on these state transitions for inactive tabs:

| Transition | Notification event |
|-----------|---------------------|
| Anything → Idle | `idle` (task completion announcement) |
| Anything → AwaitingPermission | `awaiting_permission` |
| Anything → Error | `error` |

The `working` state does NOT trigger a notification. Working transitions are too common — every input the user submits transitions the tab to Working briefly. We don't want to announce that every time.

Notifications only fire when the user is on a *different* tab. If the user is currently on the tab whose state changed, no notification — they can see the state change directly.

#### Notification text

Each (tab, event) pair has a configurable notification text in settings. Defaults:

```
claude.notifications.idle:                "Claude is idle"
claude.notifications.awaiting_permission: "Claude is awaiting permission"
claude.notifications.error:               "Claude encountered an error"

aider.notifications.idle:                "Aider is idle"
aider.notifications.awaiting_permission: "Aider is awaiting permission"
aider.notifications.error:               "Aider encountered an error"
```

User can edit these freely (Structure B from design discussion: full text is configurable per (tab, event), no separate prefix field). An empty string disables that specific notification while leaving others active.

If multi-tab v3 adds more tabs, the user can prepend identifying text manually (e.g., "Tab 3: Claude is idle"). This is sufficient for the foreseeable scope.

#### Notification queue and playback

Notifications go through a dedicated queue, separate from the regular per-tab TTS queues. The notification queue's behavior:

1. **Append on trigger**: when a notification fires, append it to the queue with its tab ID, event type, and text.
2. **Filter at play-time**: just before playing notifications, filter the queue so that for each tab, only the most recent notification is retained. Older notifications from the same tab are dropped. Notifications from different tabs all survive (in their original arrival order, modulo the per-tab dedup).
3. **Playback timing**: notifications wait for any currently-playing TTS (regular tab TTS) to finish. Then notifications play, in arrival order after the dedup filter. Then regular tab TTS resumes if there's more queued.

Example: while user is on aider, the following accumulates:
- Claude tab: → Working (no notification — not a notification trigger)
- Claude tab: → Idle (notification queued: "Claude is idle")
- Claude tab: → Working (no notification)
- Claude tab: → AwaitingPermission (notification queued: "Claude is awaiting permission")
- (Hypothetical tab 3): → Idle (notification queued: "Tab 3 is idle")

When current TTS finishes, the queue is filtered to most-recent-per-tab: drops "Claude is idle" (older than the AwaitingPermission), keeps "Claude is awaiting permission" and "Tab 3 is idle". They play in arrival order: "Claude is awaiting permission", then "Tab 3 is idle".

This preserves the most useful information per tab without piling up redundant announcements.

#### Edge-case rules (V2-04 implementation)

Two refinements to the queue logic above, added to handle event orderings the simple "most-recent-per-tab" rule doesn't catch on its own:

- **Idle is suppressed while `awaiting_permission` is set on the same tab.** When Claude stops printing to ask for permission, the avatar state machine drops the tab to Idle (output-stopped) at roughly the same instant the permission detector fires. Without this rule the user hears both announcements ("X is awaiting permission" *and* "X is idle") for the same logical event. The check runs at enqueue time against the manager's most-recent-known `awaiting_permission` flag for the tab; the Idle notification is dropped silently if that flag is currently true.
- **Drain is debounced ~200 ms after the first enqueue.** When something gets queued and audio is currently idle, the manager waits a short window before draining. This gives closely-spaced related events (e.g. an Idle that arrives microseconds before the AwaitingPermission for the same logical edge, or vice versa) a chance to land in the queue together so dedup can collapse them. Audio idle-edges drain immediately on the next pulse; the debounce only applies to the cold-start case where no idle edge is forthcoming. If new events arrive during the window, the existing deadline stands — they ride the same drain.

#### Configuration toggles

- **Global "announcements enabled" toggle**: master on/off in settings. Default: ON. When OFF, no notifications fire regardless of state changes.
- **Quick toggle** in the bottom status bar: same effect as the settings toggle, accessible without opening settings.

There is no per-tab announcement toggle in v2. If users want different per-tab behavior, they can clear individual notification text fields (empty string disables that specific event for that tab).

### Bottom Status Bar

A thin horizontal bar at the bottom of the application window, below the terminal area.

Layout:

- **Left side**: empty in v2 (reserved for future text display, e.g., status messages, progress indicators, current model)
- **Right side**: three controls in a row:
  - Mute TTS button (icon: speaker with slash when muted)
  - Disable announcements button (icon: bell with slash when disabled)
  - Volume slider (small horizontal slider with a speaker icon)

Sizing:

- Bar height: ~28px (small, unobtrusive)
- Icons: ~16-20px
- Slider: ~80-100px wide

Visual style:

- Subtle background (slightly darker or lighter than the surrounding area)
- Thin top border to separate from terminal
- Hover effects on buttons
- Tooltips on hover ("Mute TTS", "Disable announcements", "Volume")

Behavior:

- Mute button: toggles `tts.muted` in settings (existing setting). Updates icon to reflect state.
- Announcements button: toggles `behavior.announcements_enabled` in settings (new setting in v2). Updates icon.
- Volume slider: bound to `tts.volume` in settings. Changes apply live to audio output.

These are convenience controls; the same settings remain accessible via the full settings window.

---

## Settings Schema Changes

Additions and changes from v1's schema. Existing v1 fields not listed here are unchanged.

### Top-level changes

- Add `tabs` section
- Add `behavior.announcements_enabled` field
- Add tab-switching shortcuts to `shortcuts`

### `tabs` section (new)

```json
"tabs": {
  "claude": {
    "command": "claude",
    "extra_cli_flags": [],
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
  "aider": {
    "command": "aider",
    "extra_cli_flags": [],
    "tts_injection": {
      "enabled": false,
      "instructions": ""
    },
    "notifications": {
      "idle": "Aider is idle",
      "awaiting_permission": "Aider is awaiting permission",
      "error": "Aider encountered an error"
    }
  }
}
```

Notes on the schema:

- `command` is the binary name to spawn. Hardcoded to `claude` and `aider` for v2; v3 might allow more flexibility.
- `extra_cli_flags` is per-tab persistent flags (analogous to v1's single `claude_code.extra_cli_flags`). The old `claude_code` settings section is migrated into `tabs.claude` for v2.
- `tts_injection.enabled` controls whether cctts adds system prompt content for the tab. For Claude, this uses `--append-system-prompt` and is on by default. For aider, it's off by default in v2 because aider lacks a CLI mechanism for system prompt injection (see FUTURE-FEATURES.md).
- `tts_injection.instructions` is the text content to inject. cctts ships with sensible defaults for Claude. The user can edit this to refine the markup convention.
- `notifications.<event>` are the configurable notification text strings. Empty string disables that event's notification.

### Replaced and removed v1 fields

- `claude_code.extra_cli_flags` → moved to `tabs.claude.extra_cli_flags`
- `claude_code.claude_md_override` → removed (CLAUDE.md is no longer the injection mechanism; system prompt injection via CLI flag is preferred)
- `claude_code` section as a whole → removed; per-tab settings live under `tabs`

For settings file migration: if a v1 `claude_code` section exists in a loaded settings file, copy its `extra_cli_flags` to `tabs.claude.extra_cli_flags` and discard the rest. Migration runs once on first v2 launch with a v1 settings file.

### `behavior.announcements_enabled` (new)

```json
"behavior": {
  "interrupt_on_input": true,
  "auto_speak": true,
  "fallback_silent": true,
  "announcements_enabled": true
}
```

Default: true.

### Shortcut additions

```json
"shortcuts": {
  "open_compose": "Ctrl+Shift+E",
  "submit_compose": "Ctrl+Enter",
  "cancel_compose": "Escape",
  "open_settings": "Ctrl+,",
  "switch_to_tab_1": "Ctrl+1",
  "switch_to_tab_2": "Ctrl+2"
}
```

The tab-switch shortcuts use the same configuration mechanism as v1's existing shortcuts (capture UI, parser, dispatcher).

For future tabs added in v3, additional `switch_to_tab_N` entries would be added.

### Compose section (unchanged from v1)

Compose settings remain at the application level, shared across tabs. Opening the compose sheet from any tab targets that tab's PTY. The compose sheet works the same way in both Claude and aider tabs — submitting sends the textarea content to whichever tab is currently active.

---

## Aider TTS Limitations

The aider tab in v2 ships without TTS markup injection. This is intentional and documented separately in `FUTURE-FEATURES.md`. Summary:

- Claude Code provides a CLI flag (`--append-system-prompt`) for injecting system prompt content. cctts uses this for the Claude tab to teach the model the `[[TTS]]...[[/TTS]]` convention.
- Aider does not currently provide an equivalent CLI flag. The closest is `--read <file>`, which adds files as user-message context, not system prompt content. Per aider's own community discussion, user-message instructions are less reliably followed by LLMs than system prompt instructions.
- Rather than ship a workaround that produces inconsistent results, the aider tab in v2 runs without injection. The model may occasionally produce TTS-tagged content if the user's environment provides instructions some other way (e.g., a project-level conventions file the user adds via `/read` in aider), but cctts itself does not configure this.
- When aider adds a CLI flag for system prompt injection, cctts will adopt it. See `FUTURE-FEATURES.md` for the action plan.

In v2:

- Aider tab visual experience is full: tab status, avatar reflecting state, notifications, permission detection (when patterns are added), terminal rendering — all work
- Aider tab spoken TTS is silent in practice — no `[[TTS]]` tags appear in aider's output, so the existing fallback-silent behavior plays nothing

The README and any user-facing documentation should be explicit about this so users aren't surprised.

---

## Concurrency Model Updates

The v1 concurrency model (per-component tokio tasks coordinated via channels) extends straightforwardly:

- Each tab spawns its own PTY reader task and processing task — N copies of these instead of 1
- The TTS synthesis task remains single (one synthesizer servicing the active tab's queue), since only one tab speaks at a time
- The audio playback task remains single (one audio output stream)
- The state manager handles per-tab state but remains a single task
- The notification queue is managed by the state manager (or a dedicated notifications task — implementation choice)

When a tab activates:

- The TTS synthesis task switches its input source to the new tab's text queue
- The audio playback queue is cleared (in-flight playback stops, pending segments discarded)
- The previously-active tab's TTS text queue is also cleared (segments that hadn't been synthesized are dropped, not held)

When a tab deactivates:

- Its processing layer continues to run, but its TTS text queue is no longer drained (segments accumulate but are discarded next time the tab is activated, because the active-tab-only model says "what's spoken is what's currently shown")

Actually, on reflection: for clarity and resource efficiency, a deactivated tab's processing layer should probably skip the TTS extraction step entirely (or queue to a discard sink) rather than queuing segments that will be dropped later. This is a small optimization but worth doing — saves the regex/parsing work for content that will never be heard.

---

## What's Out of Scope for v2

Items raised during v2 design that are deferred:

- **Per-tab TTS settings** (separate voice/speed/volume per tab) — global only in v2
- **Lazy tab spawn** — both PTYs eager at app launch
- **User-managed tabs** (open/close/rename arbitrary tabs) — v2 has the two hardcoded tabs only
- **Tab persistence** (restore last session's tabs) — not relevant in v2 since tabs are hardcoded
- **General terminal emulator wrapping** (a tab running just bash/zsh) — deferred to v3 if pursued
- **Aider TTS markup injection** — pending upstream aider CLI support, see FUTURE-FEATURES.md
- **Aider permission prompt detection** — Phase 2 of permission detection work; v2 implements Claude Code patterns first
- **Per-tab announcement toggles** — v2 has one global announcements toggle; per-event muting via empty strings is the per-tab control
- **Replay/skip/pause controls for TTS** — playback is fire-and-forget; tab switch interrupts but otherwise no transport controls
- **Smart Working indicator** (active-tab-aware suppression) — v2 ships continuous Working indicator on the user's request; can be revisited in polish
- **Drag-to-reorder tabs** — fixed order in v2
- **Tab-specific avatar configuration** (different images for Claude vs aider) — global avatar config, with state driven by active tab
- **Notification history UI** (a list of recent notifications you can review) — fire-and-forget, no log

---

## Glossary Additions

In addition to the v1 glossary, v2 introduces:

- **Tab**: an independently-spawned subprocess with its own PTY, processing layer, and avatar state
- **Active tab**: the currently-displayed and -interactive tab; only one is active at a time
- **Background tab**: any tab that is not currently active; continues running its subprocess but is not displayed
- **Tab status indicator**: visual element on the tab bar showing status (working, awaiting permission, error, done while away)
- **DoneWhileAway**: a UI flag set when a tab transitions to Idle while inactive; cleared when the user activates the tab
- **Notification**: an audible announcement triggered when a tab's state changes meaningfully and the user is on a different tab
- **Notification queue**: separate from the per-tab TTS queues; holds pending notifications until current TTS finishes
- **Per-tab dedup at play-time**: the queue retains all notifications in arrival order but, when playback fires, filters to keep only the most recent notification per tab

---

## Implementation Phasing for v2

Detailed milestone specifications are in separate `MILESTONE-V2-*.md` files. The expected phasing:

1. **Multi-tab foundation** (`MILESTONE-V2-01-multi-tab.md`): tab bar UI, per-tab PTY/processing/state-machine refactor, tab switching, `Ctrl+1`/`Ctrl+2` shortcuts, settings schema migration. Ships with both tabs functional but no permission detection or notifications yet — both tabs work as before, just with the multi-tab UI around them.
2. **Aider tab specifics** (`MILESTONE-V2-02-aider-tab.md`): aider as a first-class tab with proper launch handling, per-tab settings UI, documented TTS limitations. Most of the multi-tab work happens in milestone 1; this milestone ensures aider specifically is well-supported.
3. **Permission detection and tab status** (`MILESTONE-V2-03-permission-detection.md`): exact-string matching for Claude Code's permission prompts, AwaitingPermission state, tab status indicator rendering (Working continuous, AwaitingPermission, Error, DoneWhileAway). Aider permission patterns are stub-only; can be characterized in a future iteration.
4. **Notifications and status bar** (`MILESTONE-V2-04-notifications-statusbar.md`): notification queue with per-tab dedup at play-time, configurable text in settings, the bottom status bar with mute / announcements / volume controls.

Each milestone produces a working app at its level of completeness. Milestones are sequential.

---

## Document Maintenance

This document is updated when:

- A v2 architectural decision changes
- A new component is added in v2 scope
- A scope item moves between in-scope and out-of-scope for v2

If a v3 design happens, it would supersede this document with a new `DESIGN-V3.md`, leaving this v2 document as a historical record (Option B convention from the design discussion).
