# Milestone V2-04: Notifications and Status Bar

## Goal

Add the audible notification system that announces tab state changes when the user is on a different tab, and add the bottom status bar with mute TTS, disable announcements, and volume controls. The notification queue uses per-tab dedup at play-time as designed. This is the final v2 functional milestone — after this, v2 is feature-complete.

## Why This Milestone Last

The notification system depends on permission detection (V2-03) to fire `awaiting_permission` notifications, and on the tab status flags from V2-03 to know when to fire `idle` notifications. With those in place, the notification system can be built cleanly. The bottom status bar is a small UI addition that fits naturally alongside the announcement controls.

## Scope

### In Scope

- A notification queue separate from the per-tab TTS queues
- Notification triggers on these inactive-tab state transitions:
  - Anything → Idle (`idle` notification)
  - Anything → AwaitingPermission (`awaiting_permission` notification)
  - Anything → Error (`error` notification)
- Per-tab dedup at play-time: queue retains all notifications in arrival order, filters to most-recent-per-tab when ready to play
- Notifications wait for currently-playing TTS to finish before playing
- After playing, regular tab TTS resumes naturally (only happens for the currently-active tab)
- Notification text is configurable per (tab, event) — UI for this was added in V2-02; the firing logic is added in this milestone
- Empty notification text disables that specific (tab, event) notification (no announcement plays)
- Global toggle: `behavior.announcements_enabled` — when false, no notifications fire regardless of state changes
- Bottom status bar UI:
  - Thin horizontal bar below the terminal area, ~28px tall
  - Right side contains three controls: mute TTS button, disable announcements button, volume slider
  - Left side: empty in v2 (reserved for future text)
  - Live-updating from settings (changes elsewhere reflect here, and vice versa)

### Out of Scope

- Per-tab announcement toggles (per design — only global toggle in v2; per-event muting via empty text strings)
- Notification history UI (notifications are fire-and-forget)
- Replay/skip controls for notifications
- Visual notifications (toast-style) — audio-only in v2
- Custom notification voices distinct from regular TTS voice (same Kokoro voice)
- Notifications when avatar transitions through Working states (not a notification trigger)

## Acceptance Criteria

### Notification triggers

1. When the active tab is X and tab Y (Y ≠ X) transitions from non-Idle to Idle, an `idle` notification for tab Y is queued
2. When the active tab is X and tab Y transitions to AwaitingPermission, an `awaiting_permission` notification for tab Y is queued
3. When the active tab is X and tab Y transitions to Error, an `error` notification for tab Y is queued
4. Transitions on the *currently active* tab do NOT queue notifications (the user can see them directly)
5. Transitions to Working states (Thinking, Speaking) do NOT trigger notifications
6. When `behavior.announcements_enabled` is false, no notifications are queued regardless of state changes

### Queue and playback

7. The notification queue stores all queued notifications in arrival order, tagged with their tab ID
8. When ready to play, the queue is filtered: for each tab, only the most recent notification is retained. Older notifications from the same tab are dropped at play-time.
9. Notifications from different tabs all survive the filter (in their original arrival order)
10. Notifications wait for any currently-playing TTS audio to finish before playing
11. Notifications play sequentially, in arrival order (after dedup)
12. After all queued notifications finish, regular TTS resumes for the active tab (if there's content queued)

### Notification text

13. Each (tab, event) combination uses the configured text from settings (`tabs.<tab>.notifications.<event>`)
14. Empty notification text means no announcement plays for that specific (tab, event) — the queue silently skips it at play-time
15. Notification text changes apply immediately — the next notification fired uses the latest configured text
16. The same Kokoro voice, speed, and volume settings as regular TTS apply to notifications

### Bottom status bar

17. The status bar is rendered below the terminal area, spanning the full window width, ~28px tall
18. The right side contains, in order: mute TTS button, disable announcements button, volume slider
19. Each control is small (icons ~16-20px, slider ~80-100px wide)
20. The mute button toggles `tts.muted` in settings; clicking immediately mutes/unmutes audio
21. The mute button's icon reflects the current state (e.g., speaker icon when not muted, speaker-with-slash when muted)
22. The announcements button toggles `behavior.announcements_enabled` in settings; clicking immediately enables/disables notification firing
23. The announcements button's icon reflects the current state (e.g., bell when enabled, bell-with-slash when disabled)
24. The volume slider is bound to `tts.volume` in settings; dragging adjusts volume in real time
25. All three controls have hover tooltips: "Mute TTS", "Disable announcements", "Volume"
26. The left side of the bar is empty (no content, just the bar's background)

### Settings integration

27. Toggling mute or announcements via the bottom bar updates the settings JSON (debounced)
28. Changes made in the settings window (TTS section, Behavior section) update the bottom bar UI in real time
29. Volume changes from either source apply live to audio output

### Cross-platform

30. The status bar renders consistently on Windows and Linux
31. Notification audio works correctly on both platforms

## Implementation Approach

### Backend: Notification Manager

Add a new module:

```
src-tauri/src/
  notifications/
    mod.rs
    manager.rs       # NotificationManager: queue, dedup, play
    triggers.rs      # logic for deciding when to queue from state events
```

#### `NotificationManager`

```rust
pub struct NotificationManager {
    queue: Vec<QueuedNotification>,
    settings: Arc<RwLock<Settings>>,
    audio: Arc<AudioOutput>,
    tts_engine: Arc<TtsEngine>,
    state_rx: broadcast::Receiver<StateEvent>,
}

#[derive(Clone, Debug)]
pub struct QueuedNotification {
    pub tab: TabId,
    pub event: NotificationEvent,
    pub text: String,
    pub timestamp: Instant,
}

#[derive(Clone, Copy, Debug)]
pub enum NotificationEvent {
    Idle,
    AwaitingPermission,
    Error,
}

impl NotificationManager {
    pub async fn run(mut self) {
        // Loop: listen for state events
        // - On qualifying state changes, queue a notification (if announcements enabled and tab is inactive)
        // - On TTS-finished events, drain the queue (with dedup) and play notifications
        // - When queue is empty, do nothing until next state event
        loop {
            tokio::select! {
                Ok(event) = self.state_rx.recv() => {
                    self.handle_state_event(event).await;
                }
                // Also need to be notified when audio playback completes,
                // so we can drain the queue at the right time
                Some(_) = self.audio.on_playback_idle().next() => {
                    self.try_drain_queue().await;
                }
            }
        }
    }
}
```

#### Triggering logic

```rust
impl NotificationManager {
    async fn handle_state_event(&mut self, event: StateEvent) {
        let settings = self.settings.read().await;
        if !settings.behavior.announcements_enabled {
            return;
        }
        
        let active_tab = self.get_active_tab().await;
        
        let notification = match event {
            StateEvent::StateChanged { tab, state: AvatarState::Idle } if tab != active_tab => {
                Some((tab, NotificationEvent::Idle))
            }
            StateEvent::StateChanged { tab, state: AvatarState::Error } if tab != active_tab => {
                Some((tab, NotificationEvent::Error))
            }
            StateEvent::AwaitingPermissionChanged { tab, awaiting: true } if tab != active_tab => {
                Some((tab, NotificationEvent::AwaitingPermission))
            }
            _ => None,
        };
        
        if let Some((tab, event)) = notification {
            let text = self.get_notification_text(&settings, tab, event);
            if !text.is_empty() {
                self.queue.push(QueuedNotification {
                    tab,
                    event,
                    text,
                    timestamp: Instant::now(),
                });
                tracing::debug!("queued notification: tab={:?} event={:?}", tab, event);
            }
        }
    }
    
    fn get_notification_text(&self, settings: &Settings, tab: TabId, event: NotificationEvent) -> String {
        let tab_settings = match tab {
            TabId::Claude => &settings.tabs.claude,
            TabId::Aider => &settings.tabs.aider,
        };
        match event {
            NotificationEvent::Idle => tab_settings.notifications.idle.clone(),
            NotificationEvent::AwaitingPermission => tab_settings.notifications.awaiting_permission.clone(),
            NotificationEvent::Error => tab_settings.notifications.error.clone(),
        }
    }
}
```

#### Queue draining and dedup

```rust
impl NotificationManager {
    async fn try_drain_queue(&mut self) {
        if self.queue.is_empty() {
            return;
        }
        if self.audio.is_playing() {
            return;  // wait for TTS to finish
        }
        
        // Per-tab dedup: keep only the most recent notification per tab
        let deduped = dedup_per_tab(&self.queue);
        self.queue.clear();  // drain
        
        // Synthesize and queue audio for each
        for notification in deduped {
            match self.tts_engine.synthesize(TtsRequest {
                text: notification.text.clone(),
                request_id: 0,  // notifications don't need request IDs
            }).await {
                Ok(response) => {
                    self.audio.enqueue(response.samples, response.sample_rate);
                }
                Err(e) => {
                    tracing::warn!("notification synthesis failed: {}", e);
                }
            }
        }
    }
}

fn dedup_per_tab(notifications: &[QueuedNotification]) -> Vec<QueuedNotification> {
    let mut latest_per_tab: HashMap<TabId, &QueuedNotification> = HashMap::new();
    for n in notifications {
        let entry = latest_per_tab.entry(n.tab).or_insert(n);
        if n.timestamp > entry.timestamp {
            *entry = n;
        }
    }
    
    // Preserve original arrival order of the surviving notifications
    let surviving_tabs: HashSet<TabId> = latest_per_tab.values()
        .map(|n| n.tab)
        .collect();
    
    let mut result = Vec::new();
    let mut seen_tabs = HashSet::new();
    for n in notifications {
        if surviving_tabs.contains(&n.tab) && latest_per_tab[&n.tab].timestamp == n.timestamp && !seen_tabs.contains(&n.tab) {
            result.push(n.clone());
            seen_tabs.insert(n.tab);
        }
    }
    result
}
```

The dedup function returns the most recent notification for each tab, in the order those tabs first appear in the queue. This preserves "first-appearing-tab-plays-first" semantics while collapsing per-tab duplicates.

#### Audio playback completion signal

The audio output module needs to emit a signal when its playback queue empties. Add to `AudioOutput`:

```rust
impl AudioOutput {
    pub fn on_playback_idle(&self) -> broadcast::Receiver<()> {
        // returns a receiver that fires every time playback transitions from playing -> idle
    }
}
```

The notification manager uses this to know when it's safe to play queued notifications.

### Frontend: Bottom Status Bar

```
src/lib/
  StatusBar.svelte
  status/
    MuteButton.svelte
    AnnouncementsButton.svelte
    VolumeSlider.svelte
```

#### `StatusBar.svelte`

```svelte
<script lang="ts">
  import MuteButton from './status/MuteButton.svelte';
  import AnnouncementsButton from './status/AnnouncementsButton.svelte';
  import VolumeSlider from './status/VolumeSlider.svelte';
</script>

<div class="status-bar">
  <div class="status-bar-left">
    <!-- empty in v2; reserved for future text -->
  </div>
  <div class="status-bar-right">
    <MuteButton />
    <AnnouncementsButton />
    <VolumeSlider />
  </div>
</div>

<style>
  .status-bar {
    display: flex;
    flex-direction: row;
    align-items: center;
    justify-content: space-between;
    height: 28px;
    background: var(--status-bar-bg, #1a1a1a);
    border-top: 1px solid var(--status-bar-border, #333);
    padding: 0 8px;
    flex-shrink: 0;
  }
  .status-bar-left {
    flex: 1;
  }
  .status-bar-right {
    display: flex;
    flex-direction: row;
    align-items: center;
    gap: 8px;
  }
</style>
```

#### `MuteButton.svelte`

```svelte
<script lang="ts">
  import { settingsStore, updateSettings } from '../settings/store';
  
  $: muted = $settingsStore.tts.muted;
  
  function toggle() {
    const settings = $settingsStore;
    settings.tts.muted = !settings.tts.muted;
    updateSettings(settings);
  }
</script>

<button class="status-button" on:click={toggle} title={muted ? 'Unmute TTS' : 'Mute TTS'}>
  {#if muted}
    🔇
  {:else}
    🔊
  {/if}
</button>

<style>
  .status-button {
    background: transparent;
    border: none;
    color: var(--status-text, #ccc);
    cursor: pointer;
    width: 24px;
    height: 24px;
    border-radius: 4px;
    display: flex;
    align-items: center;
    justify-content: center;
  }
  .status-button:hover {
    background: var(--status-button-hover, #333);
  }
</style>
```

The emoji icons are placeholder. For a polished look, use SVG icons or an icon font. Lucide icons (already mentioned for the gear button in v1) are a good choice.

#### `AnnouncementsButton.svelte`

Same pattern as MuteButton:

```svelte
<script lang="ts">
  import { settingsStore, updateSettings } from '../settings/store';
  
  $: enabled = $settingsStore.behavior.announcements_enabled;
  
  function toggle() {
    const settings = $settingsStore;
    settings.behavior.announcements_enabled = !settings.behavior.announcements_enabled;
    updateSettings(settings);
  }
</script>

<button class="status-button" on:click={toggle} title={enabled ? 'Disable announcements' : 'Enable announcements'}>
  {#if enabled}
    🔔
  {:else}
    🔕
  {/if}
</button>
```

#### `VolumeSlider.svelte`

```svelte
<script lang="ts">
  import { settingsStore, updateSettings } from '../settings/store';
  
  $: volume = $settingsStore.tts.volume;
  
  function handleChange(e: Event) {
    const target = e.target as HTMLInputElement;
    const settings = $settingsStore;
    settings.tts.volume = parseFloat(target.value);
    updateSettings(settings);
  }
</script>

<div class="volume-control" title="Volume">
  <span class="volume-icon">🔉</span>
  <input
    type="range"
    min="0"
    max="1"
    step="0.01"
    value={volume}
    on:input={handleChange}
  />
</div>

<style>
  .volume-control {
    display: flex;
    flex-direction: row;
    align-items: center;
    gap: 4px;
  }
  .volume-icon {
    color: var(--status-text, #ccc);
    font-size: 14px;
  }
  input[type="range"] {
    width: 80px;
    height: 16px;
  }
</style>
```

#### Layout integration

Update `App.svelte` to include the status bar at the bottom:

```svelte
<main>
  <TabBar />
  <div class="terminal-area">
    {#each tabs as tab}
      <div class="terminal-pane" class:hidden={$activeTab !== tab.id}>
        <Terminal tabId={tab.id} />
      </div>
    {/each}
    <AvatarOverlay />
    <WaveformOverlay />
    <ComposeOverlay />
  </div>
  <StatusBar />
</main>

<style>
  main {
    display: flex;
    flex-direction: column;
    height: 100vh;
  }
  .terminal-area {
    position: relative;
    flex: 1;
    min-height: 0;
  }
</style>
```

The avatar overlay's positioning needs to account for the new status bar at the bottom — its margins should be relative to the terminal-area container, not the window root. (Already covered in V2-01's tab bar layout note; same principle applies for the bottom bar.)

## Validation Steps

### Notification triggering

1. **Idle notification on inactive tab**: be on the aider tab. Send a message to the Claude tab beforehand and wait until Claude finishes (transitions to Idle while you're on aider). Verify you hear the configured Claude idle notification (default: "Claude is idle").
2. **No notification when on the active tab**: stay on the Claude tab. Send a message and wait for Claude to finish. Verify NO notification plays (you saw the transition directly).
3. **AwaitingPermission notification**: be on aider. Trigger a permission prompt in the Claude tab (somehow — easiest is to set things up so Claude wants to use a tool, then switch tabs). Verify the configured awaiting_permission notification plays.
4. **Error notification**: be on aider. Kill the Claude subprocess. Verify the configured error notification plays.
5. **No notification on Working transitions**: be on aider. Trigger Claude to work (it goes to Thinking and Speaking). Verify no notifications play during these transitions.
6. **Disabled announcements**: turn off the announcements toggle (via bottom bar or settings). Repeat tests 1-4. Verify no notifications fire.

### Queue and dedup behavior

7. **Multiple notifications from same tab**: be on aider. Trigger Claude through several state changes that would each notify (e.g., Idle, then trigger Working/AwaitingPermission, then resolve to Idle again). Verify only the most recent Claude notification plays.
8. **Notifications from multiple tabs**: contrived test — find a way to trigger both Claude and aider notifications in close succession (e.g., have both tabs working, switch to a hypothetical third tab if possible, otherwise use the bottom bar to disable+re-enable announcements while triggering each). For v2 with two tabs this is hard to test naturally; instead, verify by code inspection or unit tests that the dedup logic preserves multi-tab notifications.
9. **Wait for current TTS**: have Claude actively speaking when a notification queues. Verify the notification waits until the current speech finishes, then plays.
10. **Empty notification text disables**: set Claude's idle notification text to empty string in settings. Trigger an idle notification while inactive. Verify nothing plays for that notification (other notifications still work).

### Bottom status bar

11. **Visible at bottom**: launch app. Verify status bar renders at the bottom of the window with the right-side controls visible.
12. **Mute toggle**: click mute button. Verify icon changes and TTS audio is silenced. Click again to unmute.
13. **Mute syncs with settings**: change the mute setting via the settings window. Verify the bottom bar's mute button updates.
14. **Announcements toggle**: click announcements button. Verify icon changes. Trigger a notification-worthy event on an inactive tab. Verify no notification plays. Toggle back on. Verify next event triggers a notification.
15. **Volume slider**: drag volume slider. Verify audio volume changes in real time. Drag to zero. Verify silence (effectively muted). Drag back up.
16. **Tooltips**: hover over each control. Verify tooltip text appears.
17. **Layout**: resize the window. Verify the status bar stays at the bottom and the controls remain in the right-side cluster. Verify the left side is empty.

### Cross-platform

18. Verify all of the above on the second platform.

## Known Risks and Mitigation

- **Notification timing edge cases**: state transitions can fire while audio is mid-playback. The "wait for current TTS to finish" logic depends on a clean signal from the audio output. If that signal lags or fires inconsistently, notifications may play at wrong times. Mitigation: thorough testing of the audio idle-event mechanism.
- **Notifications during compose overlay**: if the user has the compose overlay open and a notification fires, the notification plays normally. The compose overlay is a UI element, not a TTS state. This is the intended behavior — notifications are independent of compose state.
- **Notification text with TTS markup**: if a user puts `[[TTS]]` tags in their notification text, the synthesizer should still treat the text literally (the tags don't get stripped because they aren't from the processing layer). This is fine but worth documenting — users shouldn't put markup tags in notification strings.
- **Long notification text**: if the user configures a very long notification text, the synthesis takes longer and may delay subsequent notifications. Acceptable; the user controls this.
- **Multiple notifications stacking up while announcements disabled**: if announcements are disabled and state changes occur, the queue accumulates nothing (we early-return before queueing). When re-enabled, only future events trigger. This is correct behavior — the user said "don't announce these things" so we don't.
- **Bottom bar squeezing the terminal**: 28px is small but accumulates with the tab bar (32px) and any avatar overlay. Total chrome is ~60px on a 1080p window — about 5%. Fine. On smaller windows (e.g., a 720p secondary monitor), it's noticeable. Worth verifying the layout still works at smaller window sizes.
- **Volume slider precision**: a 0.0–1.0 range with 80px width gives roughly 80 distinct positions. Adequate for typical use. If users want finer control, a numeric input could be added in settings, but the bar slider is for quick adjustment.

## What "Done" Looks Like

The app provides full multi-tab awareness. While focused on one tab, you stay informed about what's happening in others through tab status indicators (V2-03) and audible notifications (this milestone). The bottom bar provides quick access to the most-used controls — mute, announcements toggle, volume — without needing to open the settings window. The audio experience is coherent: regular TTS plays for the active tab, notifications announce cross-tab events at appropriate moments without interrupting in-progress speech, and you can mute or disable everything quickly when needed.

v2 is feature-complete after this milestone. Polish items (UI refinements, additional shortcuts, performance tuning) can be addressed informally based on real use, without a dedicated polish milestone.

---

## After v2

The application is feature-complete for v2. Future work falls into the parking lot from `DESIGN.md`:

- Aider TTS markup injection (pending upstream support — see FUTURE-FEATURES.md)
- Aider permission prompt detection (Phase 2 of permission detection)
- Per-tab TTS settings
- User-managed tabs / tab persistence / drag-to-reorder (v3)
- General terminal emulator wrapping as a third tab type (v3)
- Notification history UI
- Replay/skip/pause controls for TTS

Each of these is a fresh design conversation when the time comes. v2 is done.
