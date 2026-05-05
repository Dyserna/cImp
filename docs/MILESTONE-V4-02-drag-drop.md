# Milestone V4-02: Drag-and-Drop Tab Tearing

## Purpose

Implement custom mouse-based drag-and-drop for tabs: tab tearing into new panes (creating splits), moving tabs between existing panes, and reordering within a pane. This is the user-facing feature of v1.3 — after this milestone, the user can do everything they asked for: drag aider out of its position so Claude and aider show side-by-side, drag a Shell tab into its own pane, etc.

The substrate exists from M1 (layout tree, panes, focus model, portal mounting). M2 adds the interaction layer on top.

Pane lifecycle (close-collapse-rebalance) lands in this milestone too because it is needed when a drag empties the source pane.

Read `DESIGN-V4.md` and `MILESTONE-V4-01-layout-foundation.md` first.

## What This Milestone Delivers

1. Custom mouse-based drag handler on tab strips. Distinguishes click vs. drag via a 4px movement threshold.
2. Ghost tab element rendered in a fixed-position overlay layer, following the cursor during drag.
3. Drop-zone hit-testing against all visible panes. Five zones per pane: split-left (~25% from left edge), split-right, split-top (top 25%, excluding the tab bar — see below), split-bottom, center (move-to-pane).
4. The tab bar is its own zone: dropping over a pane's tab bar reorders within that pane, or moves the tab in if it's a different pane.
5. Drop-zone visualization: a translucent colored overlay showing where the dropped tab will land. Different visual treatments per zone type (split, move, reorder).
6. Drop logic for each zone:
   - Reorder within source pane: update `tab_ids` order.
   - Move to different pane (center or tab bar drop): `moveTab` to target pane; if source pane is now empty and not root, collapse.
   - Split (edge drop): `splitPane` on the target pane in the appropriate direction; set new pane focused. If source pane is now empty and not root, collapse.
7. Pane lifecycle: when a pane becomes empty as a result of a drag, it is closed via `closePane` and the tree rebalances. Builtin tabs cannot end up in a closed pane (they can't be removed) but a Shell-only pane can become empty.
8. `Esc` cancels an in-progress drag.
9. Cursor styling during drag (`grabbing`).

## What This Milestone Does NOT Do

- No splitter resize (M3) — splits created via drag use the default 0.5 ratio.
- No `Ctrl+\\` / pane shortcuts (M3).
- No pane right-click context menu (M3).
- No layout persistence (M4) — drag-created layouts reset on app restart.
- No tearing into a new window (out of scope for v1.3).
- No touch / pen input (out of scope for v1.3).
- No keyboard equivalent for drag operations (M3 ships split-pane shortcuts which cover the common cases; full keyboard DnD parity is out of scope for v1.3).

## Implementation Steps

### 1. Drag state machine

Add `frontend/src/stores/drag.ts`:

```typescript
type DragState =
    | { kind: "idle" }
    | { kind: "pending"; tabId: string; sourcePaneId: PaneId; startX: number; startY: number; pointerId: number }
    | { kind: "dragging"; tabId: string; sourcePaneId: PaneId; cursorX: number; cursorY: number; dropTarget: DropTarget | null };

type DropTarget =
    | { kind: "reorder"; paneId: PaneId; insertIndex: number }
    | { kind: "moveToPane"; paneId: PaneId }
    | { kind: "split"; paneId: PaneId; direction: "left" | "right" | "top" | "bottom" };

export const drag = writable<DragState>({ kind: "idle" });
```

### 2. Mousedown on a tab

In `TabBar.svelte` (or a new `Tab.svelte` extracted for clarity), bind `mousedown` on each tab:

```typescript
function onTabMouseDown(event: MouseEvent, tab: Tab, paneId: PaneId) {
    if (event.button !== 0) return;  // left button only
    drag.set({
        kind: "pending",
        tabId: tab.id,
        sourcePaneId: paneId,
        startX: event.clientX,
        startY: event.clientY,
        pointerId: 0,
    });
    // Attach window-level listeners for mousemove/mouseup
    window.addEventListener("mousemove", onWindowMouseMove);
    window.addEventListener("mouseup", onWindowMouseUp);
    window.addEventListener("keydown", onWindowKeyDown);
}
```

The `tab.id` and `paneId` are captured in the drag state, not in closures — this keeps the cleanup simple.

### 3. Threshold transition (pending → dragging)

In `onWindowMouseMove`:

```typescript
function onWindowMouseMove(event: MouseEvent) {
    const state = get(drag);
    if (state.kind === "pending") {
        const dx = event.clientX - state.startX;
        const dy = event.clientY - state.startY;
        if (Math.hypot(dx, dy) >= 4) {
            // Promote to dragging
            drag.set({
                kind: "dragging",
                tabId: state.tabId,
                sourcePaneId: state.sourcePaneId,
                cursorX: event.clientX,
                cursorY: event.clientY,
                dropTarget: null,
            });
            document.body.style.cursor = "grabbing";
        }
    } else if (state.kind === "dragging") {
        const dropTarget = computeDropTarget(event.clientX, event.clientY);
        drag.set({ ...state, cursorX: event.clientX, cursorY: event.clientY, dropTarget });
    }
}
```

The 4px threshold is the standard "is this a drag or a click?" distinction. Below threshold, mouseup ends without doing anything (a click); above, it commits the drop.

### 4. Drop-target computation

`computeDropTarget(x, y)`:

1. Find the pane under the cursor: iterate registered pane elements (each Pane.svelte exposes its bounding rect via a store or DOM query). If none, return `null`.
2. Determine the zone within that pane:
   - Get the pane's `getBoundingClientRect`: `{ left, top, right, bottom, width, height }`.
   - Get the tab bar's bounding rect (the tab bar element within the pane).
   - If the cursor is within the tab bar rect: it's a "tab bar" zone — reorder if same pane, move-to-pane otherwise.
     - For reorder, find the insertion index: iterate the tab elements in the bar; the insert index is the first tab whose center is to the right of (for horizontal) the cursor. If past the last, insert at end.
   - Else, the cursor is in the content area. Compute relative position:
     - `rx = (x - contentLeft) / contentWidth`
     - `ry = (y - contentTop) / contentHeight`
     - If `rx < 0.25`: split-left
     - Else if `rx > 0.75`: split-right
     - Else if `ry < 0.25`: split-top
     - Else if `ry > 0.75`: split-bottom
     - Else (center 50% × 50%): move-to-pane

Tweak the zone thresholds during M2 if user testing finds them awkward. 25% edges with a 50% × 50% center is the VS Code default and works well in practice.

For pane element registration, expose a small utility:

```typescript
// frontend/src/layout/registry.ts
class PaneRegistry {
    private panes = new Map<PaneId, HTMLElement>();
    register(id: PaneId, el: HTMLElement) { this.panes.set(id, el); }
    unregister(id: PaneId) { this.panes.delete(id); }
    findUnderCursor(x: number, y: number): { id: PaneId; el: HTMLElement } | null {
        for (const [id, el] of this.panes) {
            const r = el.getBoundingClientRect();
            if (x >= r.left && x <= r.right && y >= r.top && y <= r.bottom) {
                return { id, el };
            }
        }
        return null;
    }
}
export const paneRegistry = new PaneRegistry();
```

`Pane.svelte` registers on mount, unregisters on destroy.

### 5. Ghost tab rendering

Add `frontend/src/components/DragGhost.svelte`:

```svelte
<script>
    import { drag } from '../stores/drag';
    import { tabs } from '../stores/tabs';

    $: state = $drag;
    $: ghostTab = state.kind === "dragging" ? $tabs.find(t => t.id === state.tabId) : null;
</script>

{#if state.kind === "dragging" && ghostTab}
    <div class="drag-ghost"
         style="left: {state.cursorX + 8}px; top: {state.cursorY + 8}px;">
        {ghostTab.name}
    </div>
{/if}

<style>
    .drag-ghost {
        position: fixed;
        z-index: 10000;
        pointer-events: none;
        opacity: 0.8;
        background: var(--tab-bg);
        border: 1px solid var(--accent);
        padding: 4px 12px;
        font-size: 0.9em;
        border-radius: 4px;
    }
</style>
```

Mount `<DragGhost />` once at the app root, alongside `<LayoutNodeRenderer>`.

### 6. Drop-zone overlay

Add `frontend/src/components/DropZoneOverlay.svelte`. When `state.kind === "dragging"` and `state.dropTarget !== null`, render a translucent rectangle showing where the dropped tab will land:

- For `reorder`: a thin vertical line (~3px wide) between the two tabs at the insertion point.
- For `moveToPane`: a translucent rectangle filling the target pane's tab bar area.
- For `split`: a translucent rectangle filling half the target pane (left/right/top/bottom half).

Computed from the target pane's current bounding rect.

```svelte
{#if state.kind === "dragging" && state.dropTarget}
    {@const rect = computeOverlayRect(state.dropTarget)}
    <div class="drop-zone-overlay"
         style="left: {rect.x}px; top: {rect.y}px; width: {rect.width}px; height: {rect.height}px;" />
{/if}

<style>
    .drop-zone-overlay {
        position: fixed;
        z-index: 9999;
        pointer-events: none;
        background: rgba(80, 140, 255, 0.25);
        border: 2px solid rgba(80, 140, 255, 0.6);
        transition: all 80ms ease-out;
    }
</style>
```

The 80ms transition smooths the overlay's movement when the target zone changes during a drag.

### 7. Mouseup → commit drop

```typescript
function onWindowMouseUp() {
    const state = get(drag);
    cleanup();
    if (state.kind !== "dragging" || !state.dropTarget) {
        return;  // cancel
    }

    const t = state.dropTarget;
    layout.update(l => {
        let tree = l.tree;
        if (t.kind === "reorder") {
            tree = reorderInPane(tree, state.sourcePaneId, state.tabId, t.insertIndex);
        } else if (t.kind === "moveToPane") {
            tree = moveTab(tree, state.tabId, state.sourcePaneId, t.paneId, /* end */);
            tree = collapseIfEmpty(tree, state.sourcePaneId);
        } else if (t.kind === "split") {
            tree = removeFromPane(tree, state.tabId, state.sourcePaneId);
            tree = collapseIfEmpty(tree, state.sourcePaneId);
            const direction: SplitDirection = (t.direction === "left" || t.direction === "right") ? "horizontal" : "vertical";
            const placeOn: "first" | "second" = (t.direction === "left" || t.direction === "top") ? "first" : "second";
            const result = splitPane(tree, t.paneId, direction, state.tabId, placeOn);
            tree = result.tree;
            return { tree, focused_pane_id: result.newPaneId };
        }
        return { ...l, tree };
    });
}

function cleanup() {
    drag.set({ kind: "idle" });
    document.body.style.cursor = "";
    window.removeEventListener("mousemove", onWindowMouseMove);
    window.removeEventListener("mouseup", onWindowMouseUp);
    window.removeEventListener("keydown", onWindowKeyDown);
}
```

The `splitPane` operation needs an extension from M1's signature: it must accept which side (`"first"` or `"second"`) the new pane goes on, depending on whether the user dropped on the left/top edge (new pane goes first) or right/bottom edge (new pane goes second). Update `tree.ts` accordingly.

`collapseIfEmpty` is a new helper: if the named pane is now empty and is not the root, call `closePane`. If it's root and empty, do nothing (root-only invariant).

### 8. Esc cancellation

```typescript
function onWindowKeyDown(event: KeyboardEvent) {
    if (event.key === "Escape") {
        const state = get(drag);
        if (state.kind === "dragging" || state.kind === "pending") {
            cleanup();
            event.preventDefault();
        }
    }
}
```

### 9. Pane lifecycle: collapse-on-empty

`closePane` was implemented in M1. M2 wires it into `collapseIfEmpty`:

```typescript
function collapseIfEmpty(tree: LayoutNode, paneId: PaneId): LayoutNode {
    const pane = findPane(tree, paneId);
    if (!pane) return tree;  // already gone
    if (pane.tab_ids.length > 0) return tree;  // not empty
    if (tree.type === "pane" && tree.id === paneId) return tree;  // root-only, can't close
    return closePane(tree, paneId);
}
```

Focus handling on collapse: if the focused pane is the one being collapsed, focus moves to the surviving sibling (deepest leftmost leaf, per `DESIGN-V4`).

Update `closePane` in `tree.ts` to also surface which pane the focus should move to (return both the new tree and a `next_focus: PaneId`). Update `collapseIfEmpty` callers to use it.

### 10. Builtin tab guard

Builtin tabs (Claude, aider) cannot be closed via `×` (existing v1.2 rule). Can they be the only tab in a pane that gets emptied by a drag? No, because a drag *moves* the tab out — it doesn't close it. The destination pane has it; the source pane no longer does. If the source becomes empty (whether the moved tab was a builtin or not), it collapses. So builtin protection doesn't conflict with M2's drag logic.

But: ensure the user cannot create an unrecoverable state. E.g., if both Claude and aider end up in the same nested pane and the user accidentally collapses other panes around it, the builtins are still accessible — they're just in a single pane. Verify no path puts builtins into a "lost" state.

### 11. Testing helpers

Add unit tests for `tree.ts`:

- `splitPane` with `placeOn: "first"` and `"second"` produces correct trees.
- `closePane` rebalances correctly when the closed pane is one child of a deeply-nested split tree.
- `closePane` returns `next_focus` pointing at the surviving sibling's leftmost leaf.
- `moveTab` between deeply-nested panes works.
- `collapseIfEmpty` is a no-op for the root-only case.

Manual testing for the DnD layer is the primary verification — M2's UI is hard to unit-test exhaustively without browser-driving tooling.

### 12. Cleanup of M1 debug menu

The debug "Split focused pane" menu items from M1 can be removed or kept behind a developer flag. Suggestion: keep them (they're cheap), gated behind a `developer_mode` settings flag. They double as a fallback if drag-and-drop has issues on a specific platform.

## Files Touched / Added

**Added:**
- `frontend/src/stores/drag.ts`
- `frontend/src/layout/registry.ts`
- `frontend/src/components/DragGhost.svelte`
- `frontend/src/components/DropZoneOverlay.svelte`

**Modified:**
- `frontend/src/components/TabBar.svelte` (mousedown handler on tabs)
- `frontend/src/components/Pane.svelte` (registers with paneRegistry on mount)
- `frontend/src/layout/tree.ts` (`splitPane` extended with `placeOn`; `closePane` returns `next_focus`)
- Frontend root component (mounts DragGhost, DropZoneOverlay)

## Edge Cases and Gotchas

- **Pointer capture on the source tab**: in some webviews, the original tab element loses pointer events when its parent rerenders during the drag. Use `setPointerCapture` on the source element after mousedown, or attach mousemove/mouseup to `window` (as shown above) to bypass element-level event flow. The window approach is what VS Code uses; recommended.
- **Cursor outside window during drag**: the `mouseup` may not fire if the user releases outside the window. Handle this by also listening for `mouseleave` on `document` and treating it like a cancel — or use `pointerup` events which behave more reliably.
- **Drop on the source pane's center while there are other tabs**: the user dropped back on the same pane. Treat as "no-op" — same as cancel.
- **Drop on the source pane's edge (split into the source pane)**: this would split the source pane into itself + a new pane containing the dragged tab. Awkward but valid. Handle it cleanly: it's the same as moving the tab to a new pane that gets created via splitting the source.
- **Source pane has only one tab**: dropping that tab as a split somewhere else: the source pane becomes empty and collapses. The split happens at the destination as planned. Order matters — see step 7.
- **Builtin tab dragged to a new pane**: allowed. Builtin tabs are pinned in display order *within* their pane (Claude before aider) but the pane can be anywhere in the tree. After dragging Claude out, the source pane still has aider; the destination pane has just Claude. The user might want to rejoin them — they can drag again.
- **Drop-zone hit-test during animations**: if a pane is mid-transition (just appeared after a split), `getBoundingClientRect` returns the post-transition rect. Fine — by the time the user drops, the transition is done. Just verify no timing race makes the registry stale.
- **DragGhost element flicker on first frame**: the ghost mounts on the dragging-state transition. To avoid a one-frame jump, set initial position from the cursor coordinates already in state.
- **Drop zone for split-top is partially behind the tab bar**: the tab bar takes ~30px at the top of the pane. The split-top zone should start *below* the tab bar; otherwise dropping near the top edge ambiguously hits both "drop on tab bar" and "split-top." Resolve by shrinking the split-top zone: if cursor is within the tab bar rect → tab bar zone; only if cursor is in the content area within ~25% of the *content area's* top → split-top.
- **Many panes (5+) with rapid mousemove**: `computeDropTarget` runs on every mousemove. With 5 panes and 60fps mousemove, that's 300 hit-tests per second — trivially fast. Don't over-engineer with throttling.
- **z-index stacking**: ghost, drop-zone overlay, modal dialogs, settings window. Ensure ghost is above drop-zone overlay, and both are below modal dialogs. Use a small set of named z-index constants in CSS.

## Manual Verification Checklist

Pick up directly from M1's verification (single-pane works, debug-split works).

Drag basics:
- [ ] Mousedown on a tab and click without moving: nothing happens (still a click — does the click switch tabs as before? yes — verify).
- [ ] Mousedown on a tab and move 4+ pixels: ghost tab appears under the cursor, original tab still in place.
- [ ] Drag the ghost over the same pane's tab bar: a thin vertical line shows the insert position.
- [ ] Release: tab reorders within the pane.

Drag to split:
- [ ] Drag a tab over the right edge of the same pane (right ~25%): drop zone shows a half-pane on the right.
- [ ] Release: split is created; dragged tab lives in the new right pane; original pane keeps the rest.
- [ ] Drag a tab over the bottom edge: vertical split with new pane below.
- [ ] Drag a tab over the top edge: vertical split with new pane above.
- [ ] Drag a tab over the left edge: horizontal split with new pane on the left.

Drag between panes (with at least 2 panes existing):
- [ ] Drag a tab from one pane to another pane's tab bar: tab moves to the destination pane.
- [ ] Drag a tab from one pane to another pane's center: same — moves to destination.
- [ ] Drag the only tab out of a pane and drop into another: source pane collapses; tree rebalances; the surviving pane fills the freed space.
- [ ] Drag a tab from pane A to a split zone of pane B: split B; tab goes into the new sub-pane.

Esc and cancel:
- [ ] Start a drag, press Esc: ghost disappears, no changes to the layout.
- [ ] Start a drag, release outside any pane: drag cancels; no changes.

Focus:
- [ ] After a drag-induced split: the new pane is focused; avatar/audio/compose route to its active tab.
- [ ] After a tab move: focus moves to the destination pane (because the moved tab becomes its active tab and we want to follow what we just moved).

Builtin protection:
- [ ] Drag Claude into a new pane: works.
- [ ] Now both Claude and aider are in different panes. Verify they still function (notifications, TTS, permission detection on Claude).
- [ ] Drag Claude back into aider's pane: builtins are now together again; Claude appears at end (or per the move's insert index).

xterm.js state preservation:
- [ ] In Claude, generate some output (e.g., ask it a question). Drag Claude into a new pane. The output is still there; the conversation is intact.
- [ ] Type into Claude after the drag: input is sent correctly to the same `claude` subprocess.

App restart:
- [ ] After M2, layout still resets to single-pane on restart (persistence is M4). Verify no errors during restart with multi-pane state.

## Done Criteria

- All 9 "What This Milestone Delivers" items work.
- All "Manual Verification Checklist" items pass.
- xterm.js state survives all drag operations including across panes.
- No regression in M1 behavior or v1.2 single-pane behavior.
- No memory leaks: dragging extensively doesn't accumulate orphan DOM elements (verify in browser devtools — number of `.terminal-host` elements stays equal to the tab count).
- `cargo test` passes; frontend tree-operation unit tests pass.
