# Design Document: cctts v4

## Purpose of This Document

This document captures the architecture and design decisions for cctts v4 — the multi-pane layout evolution of the v3 design. It supersedes `DESIGN-V3.md` (v3) as the current architectural truth. The v3 document remains as a historical record of the v3 architecture as it existed at v1.2 ship.

When this document conflicts with `DESIGN-V3.md`, this document wins. Where v3 design elements are unchanged in v4, this document references them rather than restating them — read both together for the complete picture. Likewise for v2 and v1.

The product version that ships from this design is **v1.3**. The design-document version (the "v4" in the filename) is independent of the product version.

The audience is Claude Code working on v1.3 implementation, plus any human reviewer.

---

## What v1.3 Adds

v1.2 shipped the user-managed-tabs system: per-tab PTY, processing, state machine; AI-tool tabs and Shell tabs; create/close/rename/configure UI; persistence. All tabs live in a single tab bar at the top, only one is visible at a time. v1.3 generalizes that single tab bar into a recursive **layout tree** of panes — independent rectangular regions, each with its own tab bar, holding a subset of the tabs. The user can split any pane horizontally or vertically, drag tabs between panes (including tearing them out to create new splits), resize splits, and the layout persists across launches.

Specific additions:

1. **Pane abstraction**. A pane is a rectangular region of the content area containing its own tab bar and its own active tab. Panes do not know about each other — they are siblings under the layout tree.
2. **Layout tree**. A binary tree where leaves are panes and internal nodes are splits (horizontal | vertical, with a ratio). The tree's root fills the content area. Single-pane mode (a tree consisting of just one pane) renders identically to v1.2's single tab bar — full backward compatibility.
3. **Drag-and-drop tab tearing**. The user grabs a tab by mousedown on its tab strip, drags, and releases over a target. Targets recognized:
   - On another tab in the same pane → reorder
   - On a tab in a different pane → move tab between panes
   - On the left/right/top/bottom edge of any pane → split that pane in the corresponding direction, place dragged tab in the new pane
   - On the center of a pane → move tab to that pane (same as dropping on its tab bar)
4. **Splitter resize**. Each split has a draggable splitter line between its two children. Drag adjusts the split ratio. Minimum pane size is enforced.
5. **Pane focus**. Exactly one pane is focused at any time. Focus is independent from tab activeness (each pane has its own active tab). Click in a pane to focus it. The avatar overlay, audio playback, keyboard input routing, and compose overlay all follow the focused pane's active tab.
6. **Pane-aware keyboard shortcuts**:
   - `Ctrl+1`..`Ctrl+9`: switch active tab *within the focused pane* (changed semantics from v1.2 where this was global)
   - `Ctrl+Alt+ArrowKey`: move focus to an adjacent pane (Left/Right/Up/Down)
   - `Ctrl+\\`: split the focused pane vertically (puts active tab into a new right-hand pane)
   - `Ctrl+Shift+\\`: split the focused pane horizontally (new bottom pane)
   - `Ctrl+Shift+W`: close the focused pane (moves its tabs into a sibling pane, then collapses)
7. **Layout persistence**. The full layout tree, focused pane, and per-pane active tab persist across app launches.
8. **Named layout presets**. The user can save the current layout under a name and restore it later from a menu. Useful for swapping between e.g. "review mode" (Claude + aider side-by-side) and "build mode" (Claude + shell + shell).
9. **Pane lifecycle UI**. Closing the last tab in a non-root pane collapses the pane; the tree rebalances. A pane context menu (right-click on the pane's tab strip background, not on a tab) offers "Close pane" and "Move all tabs to..." actions.

## What v1.3 Does NOT Change

The following components are unchanged in v1.3:

- The PTY-based architecture; the per-tab PTY, processing, and state machine continue to operate exactly as in v1.2 (v3). Panes are a frontend concern; the backend tab pipeline is unaware of pane structure except for the layout tree's persistence (a single new settings field).
- TabKind, AiToolKind, the kind-aware processing layer, shell auto-detection (v1.2)
- The TTS pipeline (Kokoro, sentence segmentation, audio queue) (v1)
- The avatar overlay's state machine, asset rendering, and waveform sibling (v1) — only the *which-tab-it-follows* logic changes (focused pane's active tab instead of just active tab)
- The compose overlay (v1) — submits to focused pane's active tab
- The bottom status bar (v1.1)
- The notification queue, dedup-at-play-time, per-kind allowlists (v2, v3)
- The settings store mechanism, debounced save, broadcast (v1)
- Subprocess-exit handling for Shell tabs and the closed-state restart flow (v3)
- Tab create/close/rename/configure operations and their persistence (v3) — but operations now route to a target *pane* (defaulting to the focused pane)
- The cross-platform stack and supported platforms

Refer to `DESIGN-V3.md`, `DESIGN-V2.md`, and the v1 archive for details.

---

## Architecture

### Layout Tree

The content area renders a layout tree. Each node is one of:

```rust
pub enum LayoutNode {
    Split {
        id: SplitId,
        direction: SplitDirection,   // Horizontal | Vertical
        ratio: f32,                  // 0.0..1.0; first child's share of the available space
        first: Box<LayoutNode>,
        second: Box<LayoutNode>,
    },
    Pane {
        id: PaneId,
        tab_ids: Vec<TabId>,         // ordered tab list within this pane
        active_tab_id: Option<TabId>, // None only if tab_ids is empty (transient)
    },
}
```

Direction names: `Horizontal` means the children are arranged side-by-side (a horizontal arrangement of the children, with a *vertical* splitter between them). `Vertical` means children are stacked top-to-bottom. This is the convention CSS flexbox uses (`flex-direction: row` is the horizontal arrangement). It is the *opposite* of how some other tools name it — e.g., tmux's `split-window -h` creates side-by-side panes which it calls "horizontal split." We pick one and document it.

Invariants:
- The tree is non-empty: at minimum, there is one Pane node as the root.
- Every Pane has a unique `PaneId`.
- Every TabId in any pane's `tab_ids` corresponds to an existing tab in `settings.tabs`.
- Every tab in `settings.tabs` appears in exactly one pane's `tab_ids` (no orphans, no duplicates).
- A pane's `active_tab_id` is either `None` (only when `tab_ids` is empty, which is transient during operations) or an entry in its `tab_ids`.

Operations on the tree (all live in `frontend/src/layout/tree.ts` or equivalent):
- `find_pane(id) -> Pane`
- `find_split_containing(pane_id) -> Option<Split>`
- `move_tab(tab, from_pane, to_pane, position)`
- `split_pane(pane_id, direction, dragged_tab) -> (kept_pane_id, new_pane_id)` — creates a new Split node above the target pane, places `dragged_tab` in a new sibling pane, returns the IDs
- `close_pane(pane_id)` — removes the pane from the tree, replaces its parent Split with the surviving sibling, rebalances. Reject if `pane_id` is the root and the tree has only one pane (you can't close the only pane).
- `set_split_ratio(split_id, ratio)`

These operations mutate the tree atomically and are followed by a debounced settings save and a frontend re-render.

### Pane Component

A pane is rendered by `frontend/src/components/Pane.svelte`. It contains:

- **A tab bar at the top**: identical to v1.2's tab bar component, but rendering only the tabs in this pane (not all tabs globally). The `+` button still exists per pane and creates a new Shell tab in *this* pane. The right-click context menu and inline rename behave per-tab as in v1.2.
- **The content area below**: an xterm.js instance for the active tab (one per tab; the tab's xterm.js instance moves with it when the tab moves between panes — see "Tab DOM Mounting" below).
- **The closed-overlay** (v1.2's `ClosedShellOverlay`) when the active tab is a closed Shell tab.

The pane also has a focus state. When focused, the tab bar has a subtle highlight (e.g., a thin colored line under the active tab in a brighter color, or a soft border around the pane). Click anywhere in the pane (tab bar, terminal area, closed overlay) sets focus to that pane.

### Splits and Splitter UI

Internal `Split` nodes are rendered by `frontend/src/components/Split.svelte`. A split is a flexbox container containing:

- The first child node (recursively rendered)
- A 4px-wide draggable splitter (cursor: `col-resize` for horizontal splits, `row-resize` for vertical)
- The second child node (recursively rendered)

The splitter handles mouse drag: tracks the mousedown position and the split's pixel dimensions, computes a new ratio on mousemove, calls `set_split_ratio`. Min-pane-size enforcement: the ratio is clamped so that neither child shrinks below 200px wide (for horizontal splits) or 100px tall (for vertical splits). Document these as constants `MIN_PANE_WIDTH_PX` and `MIN_PANE_HEIGHT_PX`.

If the user resizes the application window such that the current ratio would violate min sizes, the ratio is clamped on render — the user's stored preference is not overwritten unless they actively drag the splitter. When the window grows again, the original ratio is honored.

### Tab DOM Mounting

A subtle but important implementation detail: each tab owns an xterm.js DOM element (and a `<canvas>` inside it for rendering). When a tab moves from one pane to another, that DOM element must move with it — re-creating it would lose terminal state, scrollback, and the connection to the PTY's frontend buffer.

The standard pattern for this in component frameworks (Svelte, React, etc.) is **portals** or **detached DOM mounting**: each tab's xterm.js lives in a stable container outside the pane component tree, and the pane's content area is just a placeholder div. A small layout-effect on the pane component moves the active tab's DOM into its placeholder via `appendChild` (which moves rather than copies in the DOM API).

Implementation:
- `frontend/src/stores/terminals.ts` (or similar) maintains a `Map<TabId, HTMLElement>` of all xterm.js root elements, mounted to a hidden offscreen container.
- Each `Pane.svelte` has an empty `<div bind:this={contentSlot}>` placeholder.
- A reactive effect in `Pane.svelte` watches `pane.active_tab_id` and, on change, calls `contentSlot.appendChild(terminals.get(activeTabId))`.
- When a tab becomes inactive (its pane switched to a different tab), its DOM gets moved back to the offscreen container automatically by the next pane that wants it, OR explicitly by the previous pane. Choose the explicit-detach approach: when the active tab changes, first detach the previous tab's DOM (move back to offscreen) before attaching the new one.

This pattern keeps PTY connections, scrollback, and xterm.js state intact across all tab/pane operations including dragging.

### Focus Model

Exactly one pane is focused. Focus is tracked in the layout store:

```typescript
// frontend/src/stores/layout.ts
interface LayoutState {
    tree: LayoutNode;
    focused_pane_id: PaneId;
}
```

Focus changes:

- Click anywhere inside a pane → focus that pane.
- A new pane is created (via split) → focus moves to the new pane (the one containing the dragged-or-newly-created tab).
- A pane is closed → focus moves to the surviving sibling.
- `Ctrl+Alt+ArrowKey` → focus moves to the adjacent pane in that direction. "Adjacent" is geometric: among all panes whose bounding box has any overlap with the focused pane along the axis perpendicular to the arrow direction, pick the closest in the arrow's direction. If no such pane exists, the shortcut is a no-op.

Things that follow focus:

| Concern | Routing rule |
|---------|--------------|
| Avatar overlay state | Reflects the focused pane's active tab (its avatar state, awaiting_permission, etc.) |
| Audio playback (TTS, notifications) | Only the focused pane's active tab is allowed to drive audio. Pending audio for the previously-focused tab is dropped on focus change, identical to v1.2's tab-switch behavior. |
| Compose overlay submission | Sends to the focused pane's active tab. |
| Keyboard input (when typed into a pane's terminal area) | Goes to the tab whose xterm.js has actual DOM focus, which is normally the focused pane's active tab — but if the user clicked into a non-focused pane's terminal, that click already moved focus, so they remain consistent. |
| `Ctrl+T` (new shell tab) | Creates the new tab in the focused pane. |
| `Ctrl+W` (close active tab) | Closes the focused pane's active tab (or its tab in the focused pane that's active — same thing). |

Audio routing rationale: this preserves the v1.2 invariant that "the avatar reflects what you hear." If a non-focused pane's tab generates TTS, the synthesis still happens (the pipeline runs), but the resulting audio buffer is dropped at the queue-pop boundary because the source tab is no longer focused. The user sees the source tab's `DoneWhileAway` indicator on its tab strip — same v1.1 mechanism — and can switch to that pane to hear it (well, no, they can't replay it; same as v1.2). The DoneWhileAway flag ensures they don't miss that something happened.

### Active Tab vs Focused Pane

These are independent concepts:

- Each pane has its own active tab (the one whose terminal is visible in that pane's content area). Multiple panes can each have their own active tab simultaneously.
- One pane is focused at a time. The focused pane's active tab is "the active tab of the application" for purposes of avatar/audio/compose/shortcuts.

The legacy `set_active_tab(tab_id)` Tauri command from v1.2 needs reinterpretation. v1.3 splits it:

- `set_pane_active_tab(pane_id, tab_id)`: changes which tab is active in a specific pane. Triggers DOM remount per the portal logic above.
- `set_focused_pane(pane_id)`: changes which pane is focused. Triggers avatar/audio/compose routing changes.
- `set_active_tab_legacy(tab_id)` (kept for compatibility): finds the pane containing `tab_id`, sets it as the pane's active tab, and focuses that pane. Equivalent to the v1.2 behavior in the single-pane case.

### Backend Awareness of Panes

The Rust backend remains *almost* unaware of panes. Specifically:

- The state manager continues to track per-tab state. It does not track panes.
- The TTS pipeline, audio queue, and notification queue remain global singletons.
- The PTY readers, processing layers, and state-machine signal handling are unchanged.

The only backend-visible aspect of panes is **layout persistence**: the Tauri command `save_layout(layout_state)` accepts a serialized layout tree from the frontend and persists it to settings. The frontend reads the layout from settings at launch via the existing settings load. There are no other backend layout operations — all tree manipulation happens in the frontend. The backend treats the layout as an opaque JSON value attached to the settings.

This separation is deliberate. The layout is a UI concern; the data and lifecycle are backend concerns. Mixing them adds complexity without benefit — the backend has no need to reason about panes.

A small additional backend change: the **audio routing gate**. Currently audio plays for the active tab. v1.3 adds: audio plays only for *the focused pane's active tab*. The frontend tells the backend (via a new event or Tauri command) which tab is currently the "audio target." The backend's audio playback logic checks this on every queue-pop and discards buffers belonging to other tabs. This single Tauri command (`set_audio_target_tab(tab_id)`) is the only new backend-tab-routing concept.

### Drag-and-Drop Implementation

Use **custom mouse-based** drag handling, not HTML5 drag-and-drop. HTML5 DnD has well-known issues across webviews: limited drag-image control, inconsistent focus behavior, no good way to implement multi-zone drop targets with hover preview. Every tab-tearing UI of consequence (VS Code, Chrome, Firefox) uses custom mouse handling for the same reason.

The drag flow:

1. **mousedown on a tab**: record initial cursor position, the tab's TabId, and source PaneId. Don't start the drag yet — wait for movement past a threshold (e.g., 4px) to distinguish drag from click.
2. **mousemove past threshold**: enter drag mode. Display a "ghost" tab element following the cursor (rendered in a fixed-position overlay layer at the top of the document). Begin computing drop targets.
3. **drop target computation, on every mousemove**: hit-test the cursor against every visible pane. For the pane under the cursor, determine the zone:
   - Within ~25% of the left edge → split-left zone (will create a new pane on the left)
   - Within ~25% of the right edge → split-right zone
   - Within ~25% of the top edge (above the tab bar's bottom edge counts as "tab bar zone" — see below) → split-top zone
   - Within ~25% of the bottom edge → split-bottom zone
   - Center 50% → move-to-pane zone
   - Over the tab bar specifically → reorder zone (insert before the nearest tab) or move-to-pane (if past the last tab's right edge)
4. **drop zone visualization**: render a translucent colored rectangle showing where the dropped tab will end up. Different colors/styles per zone type can help — e.g., a thin highlight over the tab bar for reorder, a translucent half-pane for splits.
5. **mouseup**:
   - In a reorder zone: move the tab within its source pane to the new position.
   - In a move-to-pane zone (different pane's center or tab bar): remove the tab from source pane's `tab_ids`, append to target pane's `tab_ids`, set as target's active tab.
   - In a split zone: call `split_pane(target, direction, dragged_tab)`. The dragged tab becomes the sole tab in the new pane; the new pane is focused.
   - Outside any pane: cancel the drag. (No tearing-into-new-window in v1.3.)
6. **Throughout the drag**: hide the ghost tab on mouseup; clean up the drop-zone overlay; restore cursor.
7. **Esc during drag**: cancel.

Drop-zone hit-testing happens against the actual rendered geometry of panes (use `getBoundingClientRect`). When panes resize during a window resize mid-drag (rare), recompute on the next mousemove.

A note on the "split that creates a single-tab pane out of the source" case: if the user drags the only tab out of a pane and drops it elsewhere as a split, the source pane is now empty and must be collapsed. Handle this in the same atomic operation — remove tab from source, if source is now empty close the source pane, then perform the split at the destination. The order matters because if you collapse first, the pane IDs change and the destination might have moved.

### Pane Lifecycle

A pane is created when:
- The app launches with a layout containing it (loaded from settings)
- A drag-drop creates a split (the new pane on the receiving side of the split contains the dragged tab)
- An explicit `Ctrl+\\` or `Ctrl+Shift+\\` shortcut splits the focused pane (the new pane contains the focused pane's currently-active tab, which is *moved* not duplicated)

A pane is destroyed when:
- The last tab in it is moved or closed, AND it is not the only pane in the tree. (The root-and-only pane is never destroyed; if its last tab were closed, that tab close is rejected — but in v1.3 with builtins always present, this case doesn't arise because Claude/aider always exist.)
- The user invokes `Ctrl+Shift+W` or right-click → "Close pane": all tabs in the pane move to a designated sibling pane (the surviving sibling of the parent split, which becomes that split's replacement).

When a pane is destroyed, the tree rebalances: the parent Split node is replaced by the surviving sibling. The split ratio of the parent is discarded; the sibling now occupies the entire space the split had. Other ancestor splits are unaffected.

### Right-Click Pane Context Menu

Right-click on a pane's tab bar background (not on a tab — that's the v1.2 tab context menu) opens a small popover with:

- **Split horizontally** (creates a new empty pane to the right; the active tab moves there) — wait, no. Splits should always have content; create the split such that the active tab moves to one side and a new pane is created with... actually, what goes in the new pane? Two options:
  1. The new pane contains the active tab (moved). The original pane retains the rest.
  2. The new pane is empty / contains a new fresh shell tab.
  
  Going with option 2: split + create new shell. This matches the typical user intent ("I want a new shell next to my current pane"). The split can also happen without splitting via the drag-and-drop path, where the user explicitly chooses what goes where. The context menu option is a convenience for the common "give me a new shell over there" case.

- **Split vertically** — same, creates a new pane below with a fresh Shell tab.
- **Close pane** — moves all tabs in this pane to a sibling pane (chosen by the rule below) and removes this pane. Disabled when the tree has only one pane.
- **Move all tabs to →** submenu listing other panes by name (or pane index if unnamed). Selecting a target moves all tabs from this pane to the target pane and closes this one.

The "sibling pane" for Close is determined by walking up to the parent Split: the other child of the same Split is the sibling. If the sibling is itself a Split (not a Pane), pick a leaf pane within it — specifically the deepest leftmost pane of the sibling subtree (deterministic, predictable).

### Compose Overlay (Per-Focus Routing)

The compose overlay is unchanged structurally. It still slides up from the bottom of the window, spans the full window width, and submits via `Ctrl+Enter`. The change: submission targets the focused pane's active tab.

If the user is typing in the compose overlay while the focused pane changes (e.g., they click into another pane while still composing), the overlay stays open and the *next* submit targets the new focused pane's active tab. This matches user expectation — the compose overlay is a transient input mode, and "submit to whatever is focused now" is the simplest rule.

### Status Indicators

Each pane's tab bar shows status indicators per-tab the same way v1.2 does — the indicators are tab-state-derived, not pane-derived. A tab in a non-focused pane that fires `DoneWhileAway` shows the same indicator it would have in v1.2 when the user was on a different tab.

The DoneWhileAway flag triggers when:
- A tab transitions to Idle/AwaitingPermission/Error/Exited while the user is "away from" that tab.
- "Away from" in v1.2 meant the tab was not the active tab. In v1.3 this becomes: the tab is not the focused pane's active tab.

The flag clears when the user focuses that tab (focuses its pane AND it's the pane's active tab).

---

## UI Changes

### Single-pane Mode (Default After Migration)

After upgrading from v1.2, the layout tree consists of a single root pane containing all existing tabs in order. The visual result is identical to v1.2: one tab bar at the top, one content area below. The user opts into multi-pane explicitly (drag, shortcut, or context menu) — they are never forced into it.

### Drag Affordance

Tab strips show a `grab` cursor on hover (CSS `cursor: grab`). On mousedown, cursor becomes `grabbing`. During drag, the ghost tab uses the standard tab styling at ~80% opacity following the cursor.

### Drop Zone Visualization

When a drag is in progress and the cursor is over a pane:
- The detected drop zone is highlighted with a translucent overlay (e.g., `rgba(blueish, 0.3)`).
- For split zones, the overlay covers the half (or quadrant) where the new pane will appear.
- For move/reorder zones, the overlay is more subtle — just a thin highlighted line where the dropped tab will land.

### Focused Pane Indicator

The focused pane has a subtle visual cue: e.g., a 2px-thick colored top border on its tab bar (consistent with the active-tab indicator color), or a soft outer glow. Pick something low-noise — the user shouldn't be visually distracted, but should always be able to tell which pane has focus at a glance.

### Splitter Style

A 4px-wide line in the app's neutral border color, with a 1px highlighted center on hover. Cursor changes to `col-resize` or `row-resize` on hover. During drag, a slightly thicker visual (e.g., 6px) for grab feel.

### Layout Preset Menu

A small "Layouts" menu in the application title bar or settings window with:
- **Save current layout as...** → prompts for a name; saves the current layout tree under that name.
- **Recent presets** (up to ~5) listed for one-click restore.
- **Manage presets...** → opens a dialog to view all saved presets, rename, delete.

Restoring a preset: the current layout tree is replaced by the preset's tree. Tab assignment to panes is preserved if the tab IDs in the preset still exist; orphan tabs (in `settings.tabs` but not in any preset pane) go to the focused pane after restore. Missing tabs (in the preset but not in `settings.tabs` — e.g., user deleted a Shell tab since the preset was saved) are silently dropped.

---

## Settings Schema Changes

### New Top-Level `layout` Field

```json
{
  "layout": {
    "tree": {
      "type": "split",
      "id": "split-1",
      "direction": "horizontal",
      "ratio": 0.5,
      "first": {
        "type": "pane",
        "id": "pane-1",
        "tab_ids": ["claude"],
        "active_tab_id": "claude"
      },
      "second": {
        "type": "pane",
        "id": "pane-2",
        "tab_ids": ["aider", "shell-default-1"],
        "active_tab_id": "aider"
      }
    },
    "focused_pane_id": "pane-1"
  }
}
```

The `type` discriminator (`split` | `pane`) makes the recursive structure unambiguously deserializable. IDs (`split-1`, `pane-1`, etc.) are stable across launches and used by the frontend's tree operations. Generated as `pane-{uuid}` and `split-{uuid}` at creation time — short prefixes ease debugging.

### `session.active_tab_id` Becomes Per-Pane

v1.2's `session.active_tab_id` is now redundant with the per-pane `active_tab_id` fields in the layout tree. Migration: the v1.2 value is used to determine which pane is focused at first launch on v1.3 (the pane containing that tab) and dropped from settings.

### Layout Preset Storage

Saved presets live under a new top-level field:

```json
"layout_presets": [
  {
    "name": "Review mode",
    "created_at": "2026-04-12T10:30:00Z",
    "tree": { "type": "split", ... }
  },
  {
    "name": "Build mode",
    "created_at": "2026-04-15T14:22:00Z",
    "tree": { "type": "pane", ... }
  }
]
```

Each preset stores only the layout tree (not focused_pane_id — the user's intent on restore is "set up panes this way"; focus follows their next click or the first pane in document order).

### Migration from v1.2

On first launch with a v1.2 settings file:

1. Read the v1.2 `tabs` array (already in v1.2's `tabs[...]` shape).
2. Construct the initial layout: a single root pane containing all tab IDs in order, with `active_tab_id` set to v1.2's `session.active_tab_id` (or the first tab if absent).
3. Set `focused_pane_id` to the root pane's id.
4. Initialize `layout_presets: []`.
5. Remove `session.active_tab_id` (now redundant).
6. Backup the v1.2 file as `config.json.v1.2.bak`.

The migration is idempotent: running on a v1.3 file is a no-op because `layout` is already present.

### Shortcut Schema Additions

```json
"shortcuts": {
  "switch_to_tab_1": "Ctrl+1",
  "switch_to_tab_2": "Ctrl+2",
  "switch_to_tab_3": "Ctrl+3",
  "switch_to_tab_4": "Ctrl+4",
  "switch_to_tab_5": "Ctrl+5",
  "switch_to_tab_6": "Ctrl+6",
  "switch_to_tab_7": "Ctrl+7",
  "switch_to_tab_8": "Ctrl+8",
  "switch_to_tab_9": "Ctrl+9",
  "new_shell_tab": "Ctrl+T",
  "close_tab": "Ctrl+W",
  "focus_pane_left": "Ctrl+Alt+Left",
  "focus_pane_right": "Ctrl+Alt+Right",
  "focus_pane_up": "Ctrl+Alt+Up",
  "focus_pane_down": "Ctrl+Alt+Down",
  "split_pane_horizontal": "Ctrl+\\",
  "split_pane_vertical": "Ctrl+Shift+\\",
  "close_pane": "Ctrl+Shift+W"
}
```

`switch_to_tab_N` semantics change: now scoped to the focused pane (was global in v1.2). The migration adds the new shortcuts with their defaults if absent; it does not modify existing entries.

The `Ctrl+\\` choice for horizontal split (side-by-side panes) follows iTerm2's convention. `Ctrl+Shift+\\` for vertical (stacked) is symmetric. If these collide with anything in the webview, the user can remap.

---

## Concurrency Model Updates

The v1.2 concurrency model is essentially unchanged because panes are a frontend concern. The only backend addition is the audio-target-tab gate:

- A new `AppState` field `audio_target_tab: RwLock<Option<TabId>>` tracks which tab is allowed to play audio.
- The Tauri command `set_audio_target_tab(tab_id)` updates this field. The frontend calls it on every focus change and on every active-tab change within the focused pane.
- The audio playback task, before popping a buffer from the queue and sending to `cpal`, checks: does this buffer's source tab match `audio_target_tab`? If not, drop the buffer.
- TTS synthesis still runs for all tabs (it's cheap on the 5090, and the user might want the eventual audio if they refocus quickly — though current design drops it on refocus, so synthesis is slightly wasted work for non-focused tabs; acceptable for v1.3, optimize later if needed).

The notification system is unchanged. Per-tab dedup-at-play-time still works, just that "play time" now respects the audio target gate (notifications for non-focused-pane tabs drop at the audio queue rather than playing). The DoneWhileAway flag is still set, the visual indicator on the tab bar still shows.

---

## What's Out of Scope for v1.3

- **Tearing tabs into a new window** (i.e., dragging outside the application window to create a new top-level window). Useful in browsers but adds substantial multi-window machinery in Tauri. Deferred to v1.4 if requested.
- **Maximize pane** (a "Zen mode" temporarily showing only the focused pane full-screen). Nice-to-have, deferred.
- **Pane swap** (pick two panes, swap their positions in the tree). Achievable through drag, but a one-click swap is convenience-level — deferred.
- **Remember layout per project / working directory** (different layouts based on which directory cctts launched from). Deferred — single global layout in v1.3.
- **Split ratio quick-presets** (like "50/50", "70/30"). Deferred.
- **Custom keyboard shortcuts UI for layout actions** — the shortcuts are configurable via settings.json hand-editing in v1.3 (same as v1.2's other shortcuts), but a dedicated shortcut-editor UI is deferred.
- **Hide pane tab bar when only one tab is in the pane** — would save ~30px of vertical space; minor polish, deferred.
- **Pane numbering / labels** in the focus indicator (so `Ctrl+Alt+1` could focus pane 1 directly, distinct from focus-arrow). Deferred to v1.4.
- **Pane-aware compose overlay** (compose targets a specific pane, not the focused one). Compose targets focused-pane in v1.3.
- **Audio mixing** (multiple panes playing simultaneously). Deferred indefinitely; the focused-pane-only routing is intentional for clarity.
- **Per-pane font / theme** — global only.
- **DoneWhileAway indicator on the pane itself** (e.g., when focus is on pane A, pane B's tab fires DoneWhileAway, the *pane* might show a subtle border indicator in addition to the tab indicator). Useful but adds complexity; deferred.
- **Touch / pen drag-and-drop** — mouse only in v1.3.

---

## Glossary Additions

In addition to v1, v2, v3 glossaries:

- **Layout tree**: the binary tree describing the arrangement of panes in the content area. Internal nodes are splits, leaves are panes.
- **Pane**: a rectangular leaf region of the layout tree containing its own tab bar and one active tab. Multiple panes can be visible simultaneously.
- **Split**: an internal node of the layout tree containing two children and a direction (horizontal | vertical) and a ratio (the first child's share of the available space).
- **Splitter**: the draggable line between a split's two children; dragging adjusts the split ratio.
- **Focused pane**: the single pane currently receiving routing for avatar, audio, compose, and most keyboard shortcuts.
- **Drop zone**: a region of a pane (left/right/top/bottom edges, center, or tab bar) that, when a tab is released over it, triggers a specific outcome (split, move, reorder).
- **Ghost tab**: the visual element following the cursor during a drag.
- **Tab tearing**: the act of dragging a tab out of its pane to create a new pane (split) elsewhere.
- **Tree rebalancing**: when a pane is destroyed, its parent split is replaced by the surviving sibling, which is the standard binary-tree-deletion operation.
- **Layout preset**: a named saved layout tree, restorable via menu.
- **Audio target tab**: the single tab whose audio buffers are allowed to play. Set by the frontend on every focus or active-tab change. All other buffers are dropped at the audio queue.

---

## Implementation Phasing for v1.3

Detailed milestone specifications are in separate `MILESTONE-V4-*.md` files. The expected phasing:

1. **Layout tree and pane component** (`MILESTONE-V4-01-layout-foundation.md`): introduce the layout tree data structure, the Pane component with its own tab bar (refactored from v1.2's monolithic tab bar), tab DOM portal mounting, focus model, focus-following routing for avatar/audio/compose. Ship with a hardcoded debug command "Split focused pane" (no DnD yet) so the rendering and routing can be validated. After this milestone: with no splits, the app behaves identically to v1.2; with one split, two panes work side by side.

2. **Drag-and-drop tab tearing** (`MILESTONE-V4-02-drag-drop.md`): custom mouse-based drag handler, ghost tab, drop-zone hit-testing for the five zones, drop logic for each zone (reorder, move, split). After this: users can drag tabs to create splits. Pane lifecycle (close-collapse-rebalance) lands in this milestone too because it's needed when a drag empties a pane.

3. **Splitter resize and pane lifecycle UI** (`MILESTONE-V4-03-splitters-and-lifecycle.md`): draggable splitter with min-size constraints; `Ctrl+\\`/`Ctrl+Shift+\\` shortcuts to split the focused pane (creates a fresh Shell tab in the new pane); right-click pane context menu (Close pane, Move all tabs to...); `Ctrl+Shift+W` close pane shortcut; `Ctrl+Alt+Arrow` focus shortcuts.

4. **Layout persistence and presets** (`MILESTONE-V4-04-persistence-presets.md`): serialize/deserialize the layout tree to settings; v1.2 → v1.3 migration; integrity check (orphan tabs, missing tabs); save/restore named presets; preset management UI.

5. **Polish** (`MILESTONE-V4-05-polish.md`): cross-platform validation of drag-and-drop on Windows/Linux webviews (WebView2 vs WebKitGTK have different mouse event quirks); focused-pane visual indicator tuning; tab DOM portal edge cases; pane bar overflow when many tabs are in one pane; accessibility (focus visible, keyboard equivalents for drag operations); README updates.

Each milestone produces a working app at its level of completeness. Milestones are sequential.

---

## Document Maintenance

This document is updated when:

- A v1.3 architectural decision changes
- A new component is added in v1.3 scope
- A scope item moves between in-scope and out-of-scope for v1.3

If a v1.4 design happens, it would supersede this document with a new `DESIGN-V5.md`, leaving this v4 document as a historical record.
