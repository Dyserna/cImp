# Milestone V4-01: Layout Tree and Pane Component

## Purpose

Introduce the layout-tree data structure, refactor the v1.2 monolithic tab bar into a `Pane` component (each pane has its own tab bar), implement the tab DOM portal mounting that lets xterm.js instances move between panes without losing state, establish the focused-pane model, and route avatar/audio/compose to the focused pane's active tab.

This milestone ships *without* drag-and-drop. To validate the rendering and routing, a temporary debug menu item "Split focused pane (horizontal)" creates a programmatic split using the focused pane's active tab as the dragged tab. After this milestone:

- With no splits: the app looks and behaves identically to v1.2.
- With one or more splits: panes render side-by-side / stacked, focus follows clicks, the avatar reflects the focused pane's active tab, audio routes to focused pane only.

This is the architectural lift. The visible feature (drag) lands in M2; M1 is making sure the substrate works.

Read `DESIGN-V4.md` first; this document assumes its terminology.

## What This Milestone Delivers

1. The `LayoutNode` enum (`Split` | `Pane`), with `SplitId`, `PaneId`, `SplitDirection`.
2. A `LayoutStore` (frontend) holding the current layout tree and `focused_pane_id`.
3. Pane operations on the tree: `find_pane`, `find_split_containing`, `move_tab`, `split_pane`, `close_pane`, `set_split_ratio`. Implemented but only `split_pane` and `close_pane` are exercised by M1's debug menu; the rest land for M2/M3.
4. A `Pane.svelte` component that renders a single pane with its own tab bar (refactored from v1.2's tab bar) plus a content slot.
5. A `Split.svelte` component that recursively renders its two children with a (non-draggable in M1) divider between them.
6. A root `Layout.svelte` component that renders the tree.
7. Tab DOM portal mounting: a `terminals` store mapping `TabId → HTMLElement`, all mounted offscreen at app boot, moved into `Pane.svelte` content slots reactively when active.
8. Click-to-focus on any pane area sets `focused_pane_id`.
9. Avatar overlay subscribes to focused pane's active tab's avatar state — the avatar follows focus.
10. New backend command `set_audio_target_tab(tab_id)` that gates the audio queue. Frontend calls it on every focus change and on every active-tab change within the focused pane.
11. Compose overlay submits to focused pane's active tab.
12. `set_pane_active_tab(pane_id, tab_id)` and `set_focused_pane(pane_id)` Tauri commands.
13. A debug menu (only visible in dev builds, or under Settings → Developer for now) with "Split focused pane horizontally" and "Split focused pane vertically" entries that exercise `split_pane`.
14. The v1.2 tab bar is replaced by per-pane tab bars; in single-pane mode this is visually equivalent to v1.2.

## What This Milestone Does NOT Do

- No drag-and-drop (M2).
- No splitter resize (M3) — the divider is rendered but not draggable yet.
- No `Ctrl+\\` / `Ctrl+Shift+\\` / `Ctrl+Shift+W` / `Ctrl+Alt+Arrow` shortcuts (M3).
- No pane right-click context menu (M3).
- No layout persistence (M4) — the layout tree resets to "single root pane with all tabs" on every launch.
- No layout presets (M4).
- No drag-zone visualization (M2 owns the entire DnD layer).
- No min-pane-size enforcement (M3 — the splitter ships without resize, so no constraints to enforce yet).
- No `Ctrl+1`..`Ctrl+9` semantic change to "within focused pane" — keep v1.2's global behavior in M1, change in M3 with the rest of the pane shortcuts.

## Implementation Steps

### 1. Define the layout tree types

In `frontend/src/layout/types.ts`:

```typescript
export type SplitId = string;
export type PaneId = string;
export type SplitDirection = "horizontal" | "vertical";

export interface SplitNode {
    type: "split";
    id: SplitId;
    direction: SplitDirection;
    ratio: number;            // 0.0..1.0
    first: LayoutNode;
    second: LayoutNode;
}

export interface PaneNode {
    type: "pane";
    id: PaneId;
    tab_ids: string[];        // TabId values
    active_tab_id: string | null;
}

export type LayoutNode = SplitNode | PaneNode;
```

ID generation: prefix-uuid (`pane-{uuid}`, `split-{uuid}`). UUID library is fine; collisions across launches are not a concern.

### 2. Implement tree operations

In `frontend/src/layout/tree.ts`:

```typescript
export function findPane(root: LayoutNode, id: PaneId): PaneNode | null { ... }
export function findSplitContaining(root: LayoutNode, paneId: PaneId): SplitNode | null { ... }
export function moveTab(root: LayoutNode, tabId: string, fromPaneId: PaneId, toPaneId: PaneId, position: number): LayoutNode { ... }
export function splitPane(root: LayoutNode, paneId: PaneId, direction: SplitDirection, draggedTabId: string): { tree: LayoutNode, newPaneId: PaneId } { ... }
export function closePane(root: LayoutNode, paneId: PaneId): LayoutNode { ... }
export function setSplitRatio(root: LayoutNode, splitId: SplitId, ratio: number): LayoutNode { ... }
```

All operations return a new tree (immutable update), suitable for Svelte reactivity. Implementation hints:

- `splitPane` constructs a new `SplitNode` with the original pane (with `draggedTabId` removed) on one side and a new pane containing only `draggedTabId` on the other. Default ratio 0.5. Direction determines which side gets the new pane: convention — for horizontal splits, the new pane goes to the *right*; for vertical, to the *bottom*. (Drag-and-drop in M2 will override this based on which edge was dropped on.)
- `closePane` performs the standard binary-tree-deletion: find the parent Split, replace the parent with the surviving sibling. If `paneId` is the root, return the tree unchanged (root cannot be closed). M1's debug menu does not surface close, but M2's DnD-induced empty-pane collapse needs this.
- `moveTab` removes the tab from its source pane's `tab_ids` and inserts it into the destination pane's `tab_ids` at `position`. If the source pane is now empty, M2's caller decides whether to collapse it (the operation itself doesn't collapse — keep the operation pure).

Add unit tests (`frontend/src/layout/tree.test.ts`) covering: split, close, move within same pane, move across panes, deep-nested operations, edge cases (single-pane root, two-pane balanced, three-level deep).

### 3. Create the LayoutStore

In `frontend/src/stores/layout.ts`:

```typescript
import { writable, derived } from "svelte/store";

export interface LayoutState {
    tree: LayoutNode;
    focused_pane_id: PaneId;
}

const initialPane: PaneNode = {
    type: "pane",
    id: "pane-default",
    tab_ids: [],
    active_tab_id: null,
};

export const layout = writable<LayoutState>({
    tree: initialPane,
    focused_pane_id: "pane-default",
});

// Derived: focused pane node
export const focusedPane = derived(layout, ($l) => findPane($l.tree, $l.focused_pane_id)!);

// Derived: focused pane's active tab id
export const focusedActiveTabId = derived(focusedPane, ($p) => $p.active_tab_id);
```

The initial tree is a single empty pane. Tabs get added to it as they're created. M1 wires creation to the focused pane, so tabs created at app launch (Claude, aider, Shell 1) all land in `pane-default`.

### 4. Tab-store wiring on launch

In v1.2, `App.svelte` (or wherever the root component lives) reads `settings.tabs` and renders them in the tab bar. M1 changes this:

- After loading `settings.tabs`, populate `pane-default.tab_ids` with all tab IDs in their settings order.
- Set `pane-default.active_tab_id` to v1.2's persisted `session.active_tab_id` (or the first tab ID).
- Set `focused_pane_id = "pane-default"`.
- Render the tree (which is just the one pane).

The tab bar now lives inside the Pane component, not at the app root.

### 5. Build the Pane component

`frontend/src/components/Pane.svelte`:

```svelte
<script>
    import { layout } from '../stores/layout';
    import TabBar from './TabBar.svelte';
    import { terminals } from '../stores/terminals';

    export let pane;

    let contentSlot;
    let mountedTabId = null;

    // Reactive: when the pane's active tab changes, swap the DOM
    $: if (contentSlot && pane.active_tab_id !== mountedTabId) {
        // Detach previous tab's terminal element back to offscreen
        if (mountedTabId && terminals.has(mountedTabId)) {
            const offscreen = document.getElementById('terminal-offscreen');
            offscreen.appendChild(terminals.get(mountedTabId));
        }
        // Attach new tab's terminal element
        if (pane.active_tab_id && terminals.has(pane.active_tab_id)) {
            contentSlot.appendChild(terminals.get(pane.active_tab_id));
        }
        mountedTabId = pane.active_tab_id;
    }

    function handlePaneClick() {
        layout.update(l => ({ ...l, focused_pane_id: pane.id }));
    }

    $: focused = $layout.focused_pane_id === pane.id;
</script>

<div class="pane" class:focused on:click={handlePaneClick}>
    <TabBar {pane} />
    <div class="pane-content" bind:this={contentSlot}>
        <!-- xterm.js DOM is portaled in here -->
    </div>
</div>

<style>
    .pane { display: flex; flex-direction: column; height: 100%; min-width: 0; min-height: 0; }
    .pane.focused .tab-bar { /* focused indicator */ }
    .pane-content { flex: 1; min-height: 0; position: relative; }
</style>
```

The `min-width: 0; min-height: 0` is critical for nested flexbox layout — without it, panes can refuse to shrink below their content's intrinsic size and the splitting math breaks.

### 6. Refactor TabBar to be pane-scoped

The v1.2 `TabBar.svelte` rendered all tabs from a global tab store. Refactor so it accepts a `pane` prop and renders only the tabs in `pane.tab_ids`:

- Iterate `pane.tab_ids`, look up each in the tabs store, render.
- Active tab styling driven by `pane.active_tab_id`.
- Click on a tab: set `pane.active_tab_id` (via `set_pane_active_tab`).
- The `+` button creates a new shell tab and adds it to *this* pane's `tab_ids`.
- Close button on user tabs: removes from `tab_ids`. If `tab_ids` becomes empty and this is the only pane in the tree, the tab cannot be closed (builtin protection still applies — Claude/aider can't be closed). If empty after close and not the root pane, M2's caller will collapse the pane; M1 doesn't exercise this case yet.

### 7. Build the Split component

`frontend/src/components/Split.svelte`:

```svelte
<script>
    import LayoutNodeRenderer from './LayoutNodeRenderer.svelte';
    export let split;
</script>

<div class="split" class:horizontal={split.direction === 'horizontal'} class:vertical={split.direction === 'vertical'}>
    <div class="split-child" style:flex-basis={`${split.ratio * 100}%`}>
        <LayoutNodeRenderer node={split.first} />
    </div>
    <div class="splitter" />
    <div class="split-child" style:flex-basis={`${(1 - split.ratio) * 100}%`}>
        <LayoutNodeRenderer node={split.second} />
    </div>
</div>

<style>
    .split { display: flex; height: 100%; width: 100%; min-width: 0; min-height: 0; }
    .split.horizontal { flex-direction: row; }
    .split.vertical { flex-direction: column; }
    .split-child { min-width: 0; min-height: 0; overflow: hidden; }
    .splitter { background: var(--border); flex-shrink: 0; }
    .horizontal > .splitter { width: 4px; cursor: col-resize; }
    .vertical > .splitter { height: 4px; cursor: row-resize; }
</style>
```

Note `cursor: col-resize`/`row-resize` is set in M1 for the visual cue, but mousedown on the splitter is not yet wired to a resize handler — that's M3.

### 8. LayoutNodeRenderer (recursive)

`frontend/src/components/LayoutNodeRenderer.svelte`:

```svelte
<script>
    import Pane from './Pane.svelte';
    import Split from './Split.svelte';
    export let node;
</script>

{#if node.type === 'pane'}
    <Pane pane={node} />
{:else}
    <Split split={node} />
{/if}
```

Root component (was `App.svelte`'s tab bar / content area) becomes:

```svelte
<LayoutNodeRenderer node={$layout.tree} />
```

### 9. Tab DOM portal infrastructure

Add a hidden offscreen container in the app root template:

```html
<div id="terminal-offscreen" style="position: absolute; left: -10000px; top: -10000px; visibility: hidden;"></div>
```

In `frontend/src/stores/terminals.ts`:

```typescript
class TerminalStore {
    private elements = new Map<string, HTMLElement>();
    private xtermInstances = new Map<string, Terminal>();   // xterm.js Terminal objects

    createForTab(tabId: string): HTMLElement {
        const el = document.createElement('div');
        el.className = 'terminal-host';
        el.style.height = '100%';
        el.style.width = '100%';
        const offscreen = document.getElementById('terminal-offscreen');
        offscreen.appendChild(el);

        const term = new Terminal({ /* options */ });
        term.open(el);
        // Wire term to PTY via existing v1 IPC...

        this.elements.set(tabId, el);
        this.xtermInstances.set(tabId, term);
        return el;
    }

    has(tabId: string): boolean { return this.elements.has(tabId); }
    get(tabId: string): HTMLElement | undefined { return this.elements.get(tabId); }

    destroyForTab(tabId: string) {
        const el = this.elements.get(tabId);
        if (el) el.remove();
        this.xtermInstances.get(tabId)?.dispose();
        this.elements.delete(tabId);
        this.xtermInstances.delete(tabId);
    }
}

export const terminals = new TerminalStore();
```

When a tab is created (settings load, `create_shell_tab` event), call `terminals.createForTab(tabId)`. When a tab is closed, `terminals.destroyForTab(tabId)`.

The element is initially mounted offscreen; the Pane component portals it into its content slot when the tab is active in that pane. Moving to another pane = another Pane component portals it into its slot — `appendChild` on a different parent moves the DOM node. xterm.js handles this gracefully because its Terminal object holds a reference to the host element regardless of where it's mounted in the DOM tree.

**Caveat**: xterm.js may need a `term.fit()` or `term.resize()` call after a parent change if the new container has different dimensions. Wire a resize call in the Pane's mount effect, after `appendChild`. The xterm.js fit-addon (already in v1) handles this.

### 10. Audio target tab gate (backend)

In `src/state/audio.rs` (or wherever audio playback lives):

```rust
pub struct AudioState {
    pub target_tab: RwLock<Option<TabId>>,
    // existing fields...
}
```

Tauri command:

```rust
#[tauri::command]
async fn set_audio_target_tab(tab_id: Option<String>, state: tauri::State<'_, AppState>) -> Result<(), String> {
    *state.audio.target_tab.write().await = tab_id.map(TabId::from);
    Ok(())
}
```

In the audio playback task's queue-pop loop:

```rust
let buffer = audio_queue.pop().await;
let target = *audio.target_tab.read().await;
if Some(buffer.source_tab) == target {
    sink.append(buffer.samples);
} else {
    // Drop. The DoneWhileAway flag (set elsewhere) covers visual signaling.
}
```

Buffers carry their `source_tab` (already true in v1.2 — verify; otherwise add it).

### 11. Frontend wiring of audio target

The frontend calls `set_audio_target_tab` whenever:
- The focused pane changes.
- The focused pane's active tab changes.

Compute via a `derived` store on `layout`:

```typescript
export const audioTargetTab = derived(focusedPane, ($p) => $p.active_tab_id);

audioTargetTab.subscribe((tabId) => {
    invoke('set_audio_target_tab', { tabId });
});
```

When `tabId` is null (empty pane — transient), call with null and the backend drops everything.

### 12. Avatar overlay routing

The avatar component currently subscribes to `tabs[active].avatar_state`. Change to subscribe to `tabs[focusedActiveTabId].avatar_state`:

```typescript
import { focusedActiveTabId } from '../stores/layout';
import { tabs } from '../stores/tabs';

const avatarTab = derived([focusedActiveTabId, tabs], ([$id, $tabs]) => $tabs.find(t => t.id === $id));
const avatarState = derived(avatarTab, ($t) => $t?.avatar_state ?? AvatarState.Idle);
```

Identical wiring downstream — only the source-of-truth changes.

### 13. Compose overlay routing

The compose overlay's submit handler currently calls `pty_write(active_tab_id, content)`. Change to `pty_write(focused_pane_active_tab_id, content)`. The `focused_pane_active_tab_id` is `$focusedActiveTabId` from the layout store.

### 14. Click-to-focus

Each Pane component's root element has `on:click={() => layout.update(l => ({ ...l, focused_pane_id: pane.id }))}`. This listener uses event capture or a regular click — verify it doesn't interfere with tab clicks (which need to set `pane.active_tab_id` instead). Tab clicks should `event.stopPropagation()` or, simpler: tab clicks call `set_pane_active_tab(pane.id, tab.id)` which both focuses the pane AND sets the active tab. That removes the need for the pane-level click listener to do separate work — keep the pane-level listener but make it fire the focus update only; tab clicks override.

xterm.js focus is a separate concern: when the user clicks into a pane's terminal area, xterm.js takes keyboard focus (its standard behavior). The pane-level click already fires first (or use `mousedown` for the focus update). Verify keyboard input flows to the right place.

### 15. Debug menu for splitting

In Settings → Developer (or under a temporary keystroke like `Ctrl+Shift+F12`):

- "Split focused pane horizontally" → calls `splitPane(tree, focusedPaneId, "horizontal", focusedActiveTabId)`. Updates the layout store. Sets `focused_pane_id` to the new pane.
- "Split focused pane vertically" → same with vertical.
- "Reset layout" → replaces tree with a single root pane containing all tabs.

This is throwaway code for M1 testing. M2 deletes it (replaces with the real DnD-driven path) or keeps it gated behind a developer flag for M3 onward.

### 16. set_pane_active_tab and set_focused_pane (Tauri commands)

For consistency with v1.2's flow (frontend mutates settings → backend confirms via event), wire these as Tauri commands even though the layout state lives in the frontend. This keeps the door open for M4's persistence and lets any backend logic (like the audio gate) react in one place.

Or, simpler for M1: keep the layout entirely in the frontend store; the only backend-touched concern is `set_audio_target_tab`. M4 introduces `save_layout` for persistence. Pick the simpler path; the Tauri commands `set_pane_active_tab` / `set_focused_pane` are not strictly needed in M1.

**Decision**: keep layout in frontend store only for M1; no new Tauri commands except `set_audio_target_tab`. Revisit in M4.

## Files Touched / Added

**Added:**
- `frontend/src/layout/types.ts`
- `frontend/src/layout/tree.ts`
- `frontend/src/layout/tree.test.ts`
- `frontend/src/stores/layout.ts`
- `frontend/src/stores/terminals.ts`
- `frontend/src/components/Pane.svelte`
- `frontend/src/components/Split.svelte`
- `frontend/src/components/LayoutNodeRenderer.svelte`
- Backend `src/state/audio.rs` extension (or new module) for the target-tab gate

**Modified:**
- `frontend/src/components/TabBar.svelte` (now takes a `pane` prop, renders only that pane's tabs)
- Frontend root component (replace direct tab bar + content rendering with `<LayoutNodeRenderer>`)
- Frontend avatar component (subscribe to focused-pane-derived stores)
- Frontend compose component (submit to focused pane's active tab)
- Frontend tabs store (where `terminals.createForTab(...)` is called on tab creation; `terminals.destroyForTab(...)` on tab close)
- Backend `src/ipc/mod.rs` (register `set_audio_target_tab`)
- Backend audio playback task (target-tab gate)

**No backend schema changes in M1** (persistence comes in M4).

## Edge Cases and Gotchas

- **xterm.js resize after DOM move**: `appendChild`-based moves don't fire native resize events in all browsers. After moving a tab's DOM into a pane's content slot, explicitly call the xterm.js fit-addon's `fit()` (or equivalent) on the next animation frame to ensure the terminal redraws at the correct size.
- **Pane component remount vs rerender**: Svelte's keyed each-blocks are essential when rendering the recursive tree. Use `{#each}` keyed by node ID where applicable, and ensure `LayoutNodeRenderer` is keyed too — otherwise tree mutations can cause spurious unmounts that break the portal mounting logic.
- **First-paint ordering**: the offscreen container must exist in the DOM before any tab is created (otherwise `terminals.createForTab` has nowhere to mount). Add it in the root layout before any other component.
- **Click handler propagation**: the pane's click handler firing on tab clicks is fine (focusing the pane is harmless), but make sure clicking the `+` button doesn't propagate spuriously. `event.stopPropagation()` on `+` is overkill; just ensure the focus update is idempotent.
- **xterm.js `dispose()` timing**: when a tab is closed, the v1.2 close handler kills the PTY, removes the tab from settings, etc. Add `terminals.destroyForTab(tabId)` to that flow. Order: detach DOM (if mounted), then dispose xterm.js, then `el.remove()` from offscreen.
- **The single-pane invariant**: at all times, at least one pane exists (the root or a deeper leaf). After M1's debug-menu split, two panes exist. After a hypothetical "close last pane" (which M1 doesn't expose), the operation must reject. Guard in `closePane`: return tree unchanged if `paneId === root && root.type === 'pane'`.
- **Focus on initial layout**: `focused_pane_id` starts at `pane-default`. After the first split, focus moves to the new pane (per design). Verify this in M1's debug split path.
- **Audio target on app launch**: before the first focus-change event fires, the audio target is whatever the initial focus is set to. Make sure the initial `set_audio_target_tab` call happens at app boot, not lazily on first change.
- **Empty pane (transient)**: a pane can be empty (`tab_ids.length === 0`) for a moment during a move operation. The Pane component should render gracefully — empty tab bar, placeholder content area saying "No tabs in this pane" or similar. M1 won't reach this state in practice (debug split always carries a tab), but harden against it for M2.
- **Avatar transition during pane focus change**: switching focus from pane A (active tab in Speaking state) to pane B (active tab in Idle state) should trigger the avatar's transition animation Speaking → Idle, exactly as if the user had switched tabs in v1.2. This happens automatically because the avatar subscribes to a derived store; when the source changes, the state changes, and the existing transition logic fires.

## Manual Verification Checklist

- [ ] App launches with single-pane mode; visually identical to v1.2.
- [ ] All tabs (Claude, aider, Shell 1, plus any user-created) render in the single pane's tab bar.
- [ ] Click any tab: it activates, terminal renders.
- [ ] Switch between Claude and aider: avatar updates per tab.
- [ ] Avatar follows the active tab as in v1.2.
- [ ] Audio plays for the active tab as in v1.2.
- [ ] Compose overlay submits to the active tab.
- [ ] Open the debug menu, choose "Split focused pane horizontally".
- [ ] The active tab is now in a new right-hand pane; the original pane retains the rest.
- [ ] Both panes' tab bars render correctly with their respective tabs.
- [ ] Click a tab in the left pane: that pane becomes focused; avatar updates to match its active tab.
- [ ] Click a tab in the right pane: focus moves; avatar updates.
- [ ] Type into a non-focused pane's terminal area (e.g., click into right pane's terminal while left pane was focused): the click focuses the right pane; subsequent typing goes to the right pane's active tab.
- [ ] In Claude (suppose left pane), trigger TTS by sending a message that elicits speech. Audio plays.
- [ ] Switch focus to right pane (click into it). Audio cuts immediately.
- [ ] Switch focus back to left pane: any new TTS plays; audio that was synthesized during the away period was dropped (not replayed).
- [ ] Compose overlay (`Ctrl+Shift+E`): submission goes to the focused pane's active tab.
- [ ] Split vertically: panes stack top-and-bottom; verify rendering and focus.
- [ ] Reset layout: returns to single-pane.
- [ ] Drag a tab between tabs *in the same pane* (v1.2 reorder): still works (this should be unchanged from v1.2; verify no regression).
- [ ] Create a new shell tab via `+` button: it lands in the focused pane.
- [ ] Close a user tab via `×`: works as before.
- [ ] App restart: layout is back to single-pane (no persistence yet — expected).

## Done Criteria

- All "What This Milestone Delivers" items work.
- All "Manual Verification Checklist" items pass.
- Single-pane mode is visually and behaviorally identical to v1.2 (no regression).
- Two-pane mode (after debug split) routes avatar / audio / compose correctly per focus.
- Tab DOM portal mounting preserves xterm.js state when tabs move between panes via the debug split.
- Unit tests pass for `tree.ts`.
- `cargo test` passes.
