# Feature: Multi-Window Support (Tearing Tabs)

## Purpose

Allow a tab (or a pane's worth of tabs) to be torn out of the main window into a new top-level cctts window. Standard browser-style tab-tearing pattern. The headline use case is multi-monitor: arrange Claude on monitor 1, aider + shells on monitor 2, with each window holding its own multi-pane layout tree.

This is the only item in `FUTURE-FEATURES.md` that fundamentally challenges v1.3's *single-window assumption*. Every other deferred item composes with the existing architecture; this one forces revisiting several architectural decisions that were intentional in v1.3. The complexity warrants its own feature doc and almost certainly a multi-milestone rollout.

See `FUTURE-FEATURES.md` § "Tearing tabs into a new top-level window" for the full rationale; this doc captures the implementation strategy.

## What v1.3 assumes is single-window

Every assumption below was made deliberately in v1.3. Multi-window forces a decision on each:

| v1.3 single-window assumption | Multi-window options |
|---|---|
| One global `audio_target_tab` | (a) Only OS-focused window plays audio. (b) Audio mixing across windows. (c) Per-window target with cross-window mute when blurred. |
| One avatar overlay DOM instance, anchored to the focused-pane's active tab in *the* window | One overlay per window; each renders the active tab of its own focused pane. |
| One compose overlay | Same — one per window. |
| `settings.layout` is a single tree | `settings.windows: Vec<WindowState>`, each carrying its own tree + focused pane. |
| `audio_target_tab` is global | Either still global (audio is one user, one ears) or per-window (each window has its own target). |
| Persistence: one layout, one focused pane | Per-window persistence. |
| Tab id uniqueness across one window | Already a global string id; nothing changes. |
| TTS pipeline produces one audio stream | Same; the question is *which* window's TTS reaches it. |

The right answers cluster around two design decisions:

### Decision 1: Audio gating across windows

Recommend **option (a): only the OS-focused window plays audio.** Rationale:

- This is the natural generalization of v1.3's "one audio target per session" — extend from "the focused pane in the window" to "the focused pane in the focused window."
- Avoids audio mixing (rejected indefinitely in v1.3 — see FUTURE-FEATURES.md § "Audio mixing"). The objection — "Claude's voice and aider's voice and a shell error tone all overlap is incoherent" — applies as much across windows as across panes.
- DoneWhileAway flagging continues to handle non-focused completions gracefully (visual indicator on the tab strip; pane-level indicator if that ships).
- Simpler implementation: when the user's OS focus moves between windows, the audio target re-resolves to the newly-focused window's focused-pane's active tab.

Trade-off accepted: a long TTS playback in window A is interrupted when the user clicks window B. Match v1.3's behavior — focusing a different pane mid-playback already truncates or hands off; the same applies cross-window.

### Decision 2: Settings shape

Move from a flat `layout` field to a window-keyed structure:

```rust
pub struct WindowState {
    pub window_id: String,
    pub layout: LayoutPersisted,        // existing v1.3 shape
    pub bounds: Option<WindowBounds>,   // x, y, width, height — restore positions
    pub monitor_id: Option<String>,     // best-effort monitor placement on restore
}

// Top-level settings:
pub windows: Vec<WindowState>,
pub focused_window_id: String,
```

The v1.3 `layout` and `focused_pane_id` fields move *inside* a single `WindowState` entry. Migration from v1.3: synthesize a single `WindowState` from the existing `layout` field with a freshly-generated `window_id`.

Per-window state survives launches, including window position and (best-effort) monitor placement. Tauri 2.x exposes monitor info via `app.monitors()`; the restore path tries to place a window on its remembered monitor and falls back to default placement if the monitor is gone.

## Architecture

### Backend

Tauri 2.x supports multi-window natively (`WebviewWindow::new` / the equivalent v2 API). The hard part isn't creating windows — it's that several backend components currently address "the frontend" rather than "frontend instance N":

- **Event broadcaster** (`src-tauri/src/settings/broadcaster.rs`): today emits to one webview. Either emit to all webviews and let each frontend filter to its own state, or namespace events by window id and emit per-window.
- **Tab registry**: tabs themselves are window-agnostic — a tab is a PTY + its processing state, identified by tab id. Tabs aren't owned by windows; they can move between windows the same way they move between panes today. **This is the architectural payoff** — tabs being window-agnostic means tearing a tab is just "remove from window A's layout tree, add to window B's layout tree" with no PTY teardown.
- **Avatar / TTS / audio**: one shared TTS pipeline; one shared audio output. Per-tab settings (per the per-tab-overrides feature, if shipped) continue to apply. The audio gate decides which window's TTS reaches the speakers.

### Frontend

Each window runs its own copy of the Svelte frontend. Shared state needs distinguishing per-window from cross-window:

| State | Scope |
|---|---|
| `layoutTree` (current window's layout) | Per-window |
| `focusedPaneId` (within current window) | Per-window |
| `tabState` (tab processing flags, errors, DoneWhileAway) | Cross-window — same source of truth, all windows see the same tab state |
| `audioTargetTab` | Cross-window — global |
| `settings` (most fields) | Cross-window — global |
| `composeOverlay` | Per-window |

The Svelte stores currently in `src/lib/{layout,tabs,settings,avatarState,composeState}.ts` need an audit at implementation time: each store gets categorized as per-window (loaded from this window's `WindowState`) or cross-window (subscribed to global broadcasts). The backend's per-window event namespacing (or filter-by-window-id pattern) drives the frontend's subscription.

### Tab tearing

Two flows:

1. **Drag a tab outside the main window**: extends the v1.3 drag implementation. Today, dragging a tab outside the layout tree's bounds drops it back into its source pane (no-op). After this feature, releasing outside the window creates a new window with a single-pane layout containing the dragged tab. The drag layer (`src/lib/dnd/dropTarget.ts`) needs to detect "outside any drop target *and outside the window bounds*" as a distinct outcome.
2. **Right-click "Move to new window"**: explicit menu action on a tab or a whole pane. Simpler entry point for users who don't want to drag-and-drag.

Both flows route through the same backend command: `tear_tab_to_new_window(tab_id) -> window_id` (or `tear_pane_to_new_window(pane_id)`). The command:
1. Removes the tab/pane from its source window's layout tree.
2. Creates a new Tauri window.
3. Initializes the new window's `WindowState` with a layout containing the torn content.
4. The new window mounts and reads its `WindowState` on init.

The PTY for the torn tab is **never restarted** — it stays alive in the backend tab registry; only its xterm.js frontend instance is recreated in the new window's webview (similar to how v1.3 handles tab moves between panes via the portal pattern).

### Closing the last tab in a non-main window

Closing the last tab in a non-main window closes the window. Closing the main window quits the app (existing v1.3 behavior). Decide at implementation time whether the "main" window is sticky (the originally-launched window) or floating (whichever window the user has marked as primary). Recommend sticky.

## Implementation outline

### Stage A: settings shape migration (no user-visible feature yet)

Migrate the settings schema from flat `layout` to `windows: Vec<WindowState>` with a single entry. Update the v1.3 single-window code paths to read from `windows[0]` instead of the top-level field. No new windows yet; the frontend still only supports one window. This is a refactor stage; ship cleanly with no functional change.

### Stage B: backend multi-window plumbing

- Per-window event namespacing in the broadcaster.
- Tauri command surface: `create_window_with_layout`, `close_window`, `tear_tab_to_new_window`, `tear_pane_to_new_window`.
- Audio gate listens to OS focus events (Tauri's window focus event surface) and re-resolves the target tab.
- Window position/size persistence on close, restore on launch.

### Stage C: frontend window mounting + tab tearing UX

- Frontend code that mounts a new window from a `WindowState`.
- Drag-out-of-window detection in `dropTarget.ts`.
- Right-click "Move to new window" menu items.
- Avatar overlay, compose overlay, status bar all work in non-main windows.

### Stage D: layout presets in a multi-window world

Layout presets (V4-04) currently snapshot one tree. After multi-window, do they snapshot all windows? Per-window?

**Recommend: presets capture all windows.** A preset is "my workspace arrangement," and that arrangement spans windows when the user uses multiple. Restoring a preset closes existing non-main windows and re-creates them per the preset. The main window keeps its identity (window id stable) so audio gate behavior is predictable across preset switches.

Document this in the milestone: presets are full-snapshot, not per-window.

## Open questions

- **Multi-monitor behavior when a monitor is unplugged between launches**: the persisted `monitor_id` no longer resolves. Fall back to default placement (centered on primary monitor). Standard.
- **Per-window vs. global keyboard shortcuts**: most v1.3 shortcuts are pane/layout actions and naturally scope to the focused window. A few (mute, volume) are app-global. Audit the shortcut list at implementation time.
- **How does the avatar overlay behave in a non-focused window?** Recommend: render the same state machine but suppress audio-driven waveform animation (since audio target is the focused window). Keep the visual presence so a torn-out tab still feels "owned."
- **Tab uniqueness across windows in tab id space**: tab ids are already globally unique strings (V2 era). No change.
- **Tauri 2.x WebviewWindow ergonomics**: Tauri 2.x is what cctts is on today. Verify at implementation time that `WebviewWindow::new` works the way assumed above; test that webview-to-webview communication via the broadcaster pattern actually works (it should — events go via the Tauri core).
- **Compose overlay submitting cross-window**: with the pane-aware compose overlay (from Layout & Pane Operations group), the target dropdown could in principle span all windows' tabs. Recommend: per-window scoping. Cross-window targeting is too magical and the user's intent is rarely to do it. Defer if asked.

## Milestone recommendation

**Milestones definitely needed.** This feature is the largest in the deferred set; landing it in one milestone is not realistic. Carve along the stages above:

- `MILESTONE-V1.X-XX-multi-window-stage-a-schema.md` — Stage A. Pure refactor. Settings schema migration, internal code paths read from `windows[0]`. No user-visible change.
- `MILESTONE-V1.X-XX-multi-window-stage-b-backend.md` — Stage B. Backend command surface, audio gate refactor for OS focus events, position/size persistence. Frontend still mounts one window.
- `MILESTONE-V1.X-XX-multi-window-stage-c-tearing.md` — Stage C. Frontend multi-window mounting, drag-out detection, right-click move, end-to-end UX. **Feature ships at the end of this milestone.**
- `MILESTONE-V1.X-XX-multi-window-stage-d-presets.md` — Stage D. Preset shape change to capture all windows, restore-with-window-recreation behavior. Could fold into Stage C if scope feels manageable; recommend separate for clarity.

**When implementation starts, write the milestones in detail then.** The strategy here is the load-bearing part; the per-stage step list will follow naturally from the architecture decisions documented above. Re-confirm Tauri 2.x's multi-window primitives at pickup time — Tauri APIs evolve.

**Trigger to act**: per `FUTURE-FEATURES.md`, multi-monitor use becomes painful enough that the workaround (split-tree-only) feels limiting. Probably real on a daily-driver 2- or 3-monitor desk. Don't pre-emptively pick this up — it's substantial work and the v1.3 single-window case still composes well for many use cases.

## Files most likely touched

- `src-tauri/src/settings/{schema,migration,persistence,broadcaster,mod}.rs`
- `src-tauri/src/main.rs` — window creation, command registration, OS focus event handling
- `src-tauri/src/audio/...` — audio gate refactor for multi-window
- `src/lib/layout/{store,actions,persistence}.ts` — per-window awareness
- `src/lib/dnd/dropTarget.ts` — drag-outside-window detection
- `src/lib/{tabs,settings}/store.ts` — per-window vs. cross-window store split
- `src/lib/AvatarOverlay.svelte`, `ComposeOverlay.svelte`, `StatusBar.svelte` — verify each works in non-main windows
- New: a small `WindowFrame.svelte` or equivalent root component for non-main windows (likely subset of the main app shell)
