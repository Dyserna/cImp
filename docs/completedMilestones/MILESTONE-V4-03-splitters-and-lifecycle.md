# Milestone V4-03: Splitter Resize and Pane Lifecycle UI

## Purpose

Make splitters draggable to resize, add the keyboard shortcuts for split/focus/close-pane operations, add the pane right-click context menu, and apply the v1.3 reinterpretation of `Ctrl+1`..`Ctrl+9` (now scoped to the focused pane).

After this milestone, the user can do everything via keyboard that they can do via mouse: split panes, move focus between them, close panes, switch tabs within a pane.

Read `DESIGN.md`, M1, M2 first.

## What This Milestone Delivers

1. Splitter resize: dragging the splitter line between two children of a Split adjusts the split's `ratio`. Min-size constraints (200px wide / 100px tall per pane) clamped during drag.
2. Window-resize handling: when the application window resizes such that a stored ratio would violate min sizes, the ratio is *visually* clamped on render but not overwritten in state — when the window grows back, the original ratio is honored.
3. `Ctrl+\\` shortcut: splits the focused pane horizontally (creates a new pane to the right with a fresh Shell tab).
4. `Ctrl+Shift+\\` shortcut: splits the focused pane vertically (creates a new pane below with a fresh Shell tab).
5. `Ctrl+Shift+W` shortcut: closes the focused pane (moves all its tabs to the sibling pane, then collapses).
6. `Ctrl+Alt+ArrowKey` shortcuts: focus moves to the geometrically adjacent pane.
7. `Ctrl+1`..`Ctrl+9` shortcuts: change semantics from v1.2's "global Nth tab" to v1.3's "Nth tab within focused pane."
8. Pane right-click context menu (right-click on the tab bar background, not on a tab): "Split horizontally" / "Split vertically" / "Close pane" / "Move all tabs to..." submenu.
9. Focused-pane visual indicator: a subtle border or top-of-tab-bar highlight on the focused pane.

## What This Milestone Does NOT Do

- No layout persistence (M4).
- No layout presets (M4).
- No advanced shortcut customization UI (defer; existing settings.json hand-edit suffices for v1.3).
- No animated transitions for pane operations (a sudden snap is fine; animations are polish).

## Implementation Steps

### 1. Wire splitter mousedown for resize

In `Split.svelte`, add mousedown on the `.splitter` element:

```typescript
function onSplitterMouseDown(event: MouseEvent) {
    event.preventDefault();
    event.stopPropagation();
    const splitEl = event.currentTarget.parentElement;  // the .split container
    const startRect = splitEl.getBoundingClientRect();
    const startRatio = split.ratio;
    const isHorizontal = split.direction === "horizontal";

    function onMove(e: MouseEvent) {
        const offset = isHorizontal ? (e.clientX - startRect.left) : (e.clientY - startRect.top);
        const total = isHorizontal ? startRect.width : startRect.height;
        let newRatio = offset / total;

        // Clamp to min sizes
        const minPx = isHorizontal ? MIN_PANE_WIDTH_PX : MIN_PANE_HEIGHT_PX;
        const minRatio = minPx / total;
        const maxRatio = 1 - minRatio;
        newRatio = Math.max(minRatio, Math.min(maxRatio, newRatio));

        layout.update(l => ({ ...l, tree: setSplitRatio(l.tree, split.id, newRatio) }));
    }

    function onUp() {
        window.removeEventListener("mousemove", onMove);
        window.removeEventListener("mouseup", onUp);
        document.body.style.cursor = "";
    }

    document.body.style.cursor = isHorizontal ? "col-resize" : "row-resize";
    window.addEventListener("mousemove", onMove);
    window.addEventListener("mouseup", onUp);
}
```

The `MIN_PANE_WIDTH_PX = 200` and `MIN_PANE_HEIGHT_PX = 100` constants live in `frontend/src/layout/constants.ts`.

`event.stopPropagation()` is important to prevent the mousedown from also bubbling to a drag-start on a nearby tab or focus-change on the pane.

### 2. Visual-only clamp on render

In `Split.svelte`'s rendering logic, when computing the flex-basis from the ratio, clamp to min-pixel-equivalent if needed:

```typescript
$: clampedRatio = (() => {
    if (!containerRect) return split.ratio;
    const isHorizontal = split.direction === "horizontal";
    const total = isHorizontal ? containerRect.width : containerRect.height;
    const minPx = isHorizontal ? MIN_PANE_WIDTH_PX : MIN_PANE_HEIGHT_PX;
    if (total < 2 * minPx) return 0.5;  // fallback when too small even for two min-sized panes
    const minRatio = minPx / total;
    return Math.max(minRatio, Math.min(1 - minRatio, split.ratio));
})();
```

Use `clampedRatio` for the flex-basis computation, but never write back to `split.ratio` — the user's stored preference is preserved.

`containerRect` is bound via a Svelte action that observes the container's resize via `ResizeObserver`. When the window resizes, `containerRect` updates, the clamping recomputes, and the layout adapts.

### 3. Keyboard shortcut handler infrastructure

The existing v1.2 shortcut store handles `Ctrl+T`, `Ctrl+W`, `Ctrl+1..9`, etc. M3 extends it with the new pane shortcuts.

Settings schema additions (no migration needed; missing entries get defaults):

```json
"shortcuts": {
    "focus_pane_left": "Ctrl+Alt+Left",
    "focus_pane_right": "Ctrl+Alt+Right",
    "focus_pane_up": "Ctrl+Alt+Up",
    "focus_pane_down": "Ctrl+Alt+Down",
    "split_pane_horizontal": "Ctrl+\\",
    "split_pane_vertical": "Ctrl+Shift+\\",
    "close_pane": "Ctrl+Shift+W"
}
```

### 4. Implement `split_pane_horizontal` / `split_pane_vertical`

```typescript
function splitPaneShortcut(direction: SplitDirection) {
    const state = get(layout);
    const focusedPane = findPane(state.tree, state.focused_pane_id)!;

    // Create a fresh Shell tab for the new pane
    const newTabId = await invoke<string>("create_shell_tab", {
        name: nextShellName(),
        // ...defaults from auto-detection...
    });

    // Split: the new pane goes on the "second" side (right for horizontal, below for vertical)
    // The new pane contains only the new tab. The focused pane is unchanged.
    const result = splitPane(state.tree, state.focused_pane_id, direction, newTabId, "second", /* but the dragged tab path expects the tab to come from the source pane */ );
    // ...actually for shortcuts we don't move an existing tab, we add a new one.
    // Need a different operation for this case.
}
```

The `splitPane` operation in M1 was designed around "move an existing tab to a new sibling pane." The shortcut case is "create a new pane with a brand-new tab next to the focused pane, leaving the focused pane untouched."

Add a new operation `splitPaneWithNewTab(tree, paneId, direction, newTabId, placeOn)` that:
1. Finds the target pane.
2. Wraps it in a new Split node with the original on one side and a new pane (containing only `newTabId`) on the other.
3. Does *not* remove `newTabId` from anywhere (it's a fresh tab not yet in any pane).
4. Returns the new tree and the new pane ID.

This is a small variant of `splitPane`; share code where natural.

After the split, focus moves to the new pane (the user just created it; they likely want to type in it).

### 5. Implement `close_pane`

```typescript
function closePaneShortcut() {
    const state = get(layout);
    const focusedId = state.focused_pane_id;

    // Determine the target pane to receive moved tabs: the surviving sibling
    const parent = findSplitContaining(state.tree, focusedId);
    if (!parent) return;  // root pane — can't close

    const sibling = parent.first.type === "pane" && parent.first.id === focusedId ? parent.second : parent.first;
    const targetPaneId = leftmostLeafPaneId(sibling);

    // Move all tabs from focused to target
    const focusedPane = findPane(state.tree, focusedId)!;
    let tree = state.tree;
    for (const tabId of focusedPane.tab_ids) {
        tree = moveTab(tree, tabId, focusedId, targetPaneId, /* end */);
    }

    // Now focused is empty; collapse it
    tree = closePaneTree(tree, focusedId);
    layout.set({ tree, focused_pane_id: targetPaneId });
}
```

`leftmostLeafPaneId` walks the sibling subtree and returns the first leaf pane found in document order. Add to `tree.ts`.

`closePaneTree` is the existing `closePane` operation from M1 (renamed in this snippet to avoid shadowing the local `closePane` function).

### 6. Implement `focus_pane_*`

Geometric adjacency: given the focused pane's `getBoundingClientRect`, find the next pane in the arrow direction.

```typescript
function focusPane(direction: "left" | "right" | "up" | "down") {
    const focusedEl = paneRegistry.find(get(layout).focused_pane_id);
    if (!focusedEl) return;
    const focusedRect = focusedEl.getBoundingClientRect();

    let bestPane: PaneId | null = null;
    let bestDistance = Infinity;

    for (const [id, el] of paneRegistry.entries()) {
        if (id === get(layout).focused_pane_id) continue;
        const r = el.getBoundingClientRect();

        // Check if `r` is in the right direction relative to focusedRect
        const inDirection = (() => {
            switch (direction) {
                case "left":  return r.right <= focusedRect.left + 1;
                case "right": return r.left  >= focusedRect.right - 1;
                case "up":    return r.bottom <= focusedRect.top + 1;
                case "down":  return r.top    >= focusedRect.bottom - 1;
            }
        })();
        if (!inDirection) continue;

        // Check perpendicular-axis overlap
        const overlap = (() => {
            if (direction === "left" || direction === "right") {
                return Math.max(0, Math.min(r.bottom, focusedRect.bottom) - Math.max(r.top, focusedRect.top));
            } else {
                return Math.max(0, Math.min(r.right, focusedRect.right) - Math.max(r.left, focusedRect.left));
            }
        })();
        if (overlap <= 0) continue;

        // Distance: edge-to-edge in the direction
        const distance = (() => {
            switch (direction) {
                case "left":  return focusedRect.left   - r.right;
                case "right": return r.left             - focusedRect.right;
                case "up":    return focusedRect.top    - r.bottom;
                case "down":  return r.top              - focusedRect.bottom;
            }
        })();
        if (distance < bestDistance) {
            bestDistance = distance;
            bestPane = id;
        }
    }

    if (bestPane) {
        layout.update(l => ({ ...l, focused_pane_id: bestPane }));
    }
}
```

Tie-breaking: closest in the arrow direction wins. If multiple panes are at the same distance with the same overlap, pick the one with the largest overlap. Edge cases (no pane in that direction): no-op.

### 7. `Ctrl+1`..`Ctrl+9` reinterpretation

The existing handlers from v1.2 looked up the Nth global tab. M3 changes this:

```typescript
function switchToTabN(n: number) {
    const focusedPane = findPane(get(layout).tree, get(layout).focused_pane_id)!;
    const targetTabId = focusedPane.tab_ids[n - 1];
    if (!targetTabId) return;  // no Nth tab in this pane
    setPaneActiveTab(focusedPane.id, targetTabId);
}
```

If the focused pane has fewer than N tabs, the shortcut is a silent no-op.

This is a behavior change from v1.2. Document in the README. The closest analog is iTerm2 / VS Code, both of which scope `Cmd+N` / `Ctrl+N` to "current group" or "current pane."

For users who relied on v1.2 globalness, the workaround is to put all tabs in a single pane.

### 8. `Ctrl+T` and `Ctrl+W` (already pane-aware after M1)

`Ctrl+T` (new shell tab) creates a tab in the focused pane — this should already be the case after M1 (the `+` button on each pane creates in that pane; `Ctrl+T` is equivalent to clicking `+` on the focused pane). Verify and document.

`Ctrl+W` (close active tab) closes the focused pane's active tab, with the existing builtin protection. Verify.

### 9. Pane right-click context menu

Add `frontend/src/components/PaneContextMenu.svelte`. Triggered by right-click on the pane's tab bar *background* (not on a tab — that's the v1.2 tab context menu). Distinguish via event target inspection: if `event.target` matches `.tab-bar-background` or the `+` button area, show pane menu; if it matches a tab element, show tab menu.

Items:
- **Split horizontally** → calls `splitPaneShortcut("horizontal")`.
- **Split vertically** → calls `splitPaneShortcut("vertical")`.
- **Close pane** → calls `closePaneShortcut()`. Disabled (greyed out) when focused pane is the root.
- **Move all tabs to →** submenu listing other panes by their first tab's name (since panes are unnamed in v1.3). Selecting a target moves all tabs to it and collapses this pane.

The "Move all tabs to" submenu's pane labels: a pane is identified by its tab list. Use the format `"<active tab name> + N more"` if more than one, or just `<active tab name>` if exactly one. Example: "Claude + 2 more" or "Shell 3."

### 10. Focused-pane visual indicator

In `Pane.svelte`'s CSS:

```css
.pane.focused {
    box-shadow: inset 0 2px 0 0 var(--accent);
    /* OR: a 2px solid border on the top of the tab bar element */
}
```

Choose subtle. The user doesn't need a flashing alert — they need a low-noise persistent cue. Test on both webviews to make sure it renders distinctly enough on the typical app theme but doesn't dominate.

### 11. Edge case: closing the root pane

`Ctrl+Shift+W` on the root pane (the only pane in the tree): no-op. The pane context menu's "Close pane" item is disabled. No error, no toast — the action just isn't available when there's only one pane.

### 12. Edge case: splitting an empty pane

Shouldn't happen (panes always have at least one tab; the only transient empty state is during a drag and is collapsed immediately afterward). But defensive check: if `splitPaneShortcut` is called and the focused pane is empty, just create a new pane next to it without moving anything. The new pane has the new shell; the empty pane stays empty until cleanup. Or: refuse with a no-op. Either is fine; pick one.

## Files Touched / Added

**Added:**
- `frontend/src/layout/constants.ts` (MIN_PANE_WIDTH_PX, MIN_PANE_HEIGHT_PX)
- `frontend/src/components/PaneContextMenu.svelte`

**Modified:**
- `frontend/src/components/Split.svelte` (mousedown for resize, ResizeObserver, clamping)
- `frontend/src/layout/tree.ts` (`splitPaneWithNewTab`, `leftmostLeafPaneId`)
- `frontend/src/components/Pane.svelte` (focused-pane CSS class, contextmenu wiring on tab bar background)
- `frontend/src/components/TabBar.svelte` (distinguish tab vs background contextmenu target)
- Frontend shortcut handler (new pane shortcuts; reinterpretation of Ctrl+N)
- Frontend `stores/layout.ts` (`splitPaneShortcut`, `closePaneShortcut`, `focusPane`)
- Settings schema defaults (new shortcut entries — but no migration code; missing entries default at runtime)

## Edge Cases and Gotchas

- **Splitter drag during ongoing tab drag**: shouldn't happen (a drag is exclusive — mousedown on a tab starts tab drag; mousedown on a splitter starts splitter drag). But verify: don't both share the same window-level handlers in conflicting ways. The cleanup logic from M2 handles tab drag; splitter drag in M3 has its own handlers attached at mousedown and detached at mouseup.
- **Min-size constraint with deeply nested splits**: if a split's available space is less than `2 * MIN_PANE_WIDTH_PX`, the clamp formula in step 2 falls back to ratio 0.5 (both children share equally below their min size). This is acceptable degradation when the window is too small for the layout. Don't try to be cleverer.
- **`Ctrl+\` collision in webview**: WebView2 may have a default binding. Test; if it conflicts, document and let the user remap.
- **`Ctrl+Alt+Arrow` on Linux desktops with workspace shortcuts**: GNOME/KDE often use `Ctrl+Alt+Arrow` for workspace switching at the OS level. The OS shortcut wins (the webview never sees the event). Document this as a known limitation; suggest remapping in settings.json (e.g., to `Ctrl+Shift+Arrow`).
- **`Ctrl+Shift+W` on Windows webview**: WebView2 might bind this to "close window." Test.
- **Focus arrow keys with non-axis-aligned panes**: panes are always rectangles aligned to the layout tree's splits; non-axis-aligned arrangements are not possible. The geometric adjacency check works correctly for any tree structure.
- **Closing a pane that contains the source of in-flight TTS**: if Claude is mid-speaking in pane A (focused), and the user closes pane A via `Ctrl+Shift+W`: focus moves to the sibling pane, audio cuts (because audio target tab changes). The Claude tab moves to the sibling pane and continues generating. Correct behavior; verify.
- **Right-click on `+` button**: shouldn't trigger the pane context menu; treat as a regular contextmenu suppression on the button. Let the button consume right-click silently.
- **The Move-all-to-X submenu when there are many other panes**: list them all. If there are 8+ panes, the submenu can be long but functional. Polish (scrolling, search) is deferred.

## Manual Verification Checklist

Splitter:
- [ ] After creating a horizontal split (drag a tab to the right edge), drag the splitter line: the panes resize.
- [ ] Drag the splitter all the way to one side: the smaller pane stops at ~200px width, doesn't disappear.
- [ ] Resize the application window so the split is too narrow for both panes' min sizes: ratio clamps visually.
- [ ] Resize back to a larger window: the original ratio is restored (verify via splitter position).
- [ ] Same checks for vertical split with min-height = 100px.
- [ ] Splitter cursor: `col-resize` for horizontal, `row-resize` for vertical, on hover.

Shortcuts:
- [ ] `Ctrl+\\` on the focused pane: a new pane appears to the right with a fresh Shell tab; the new pane is focused.
- [ ] `Ctrl+Shift+\\`: same vertically (new pane below).
- [ ] `Ctrl+Shift+W` on a non-root pane: pane closes; tabs move to sibling; tree rebalances; focus moves to sibling.
- [ ] `Ctrl+Shift+W` on root pane: no-op.
- [ ] `Ctrl+Alt+Right` with a pane to the right of the focused one: focus moves there.
- [ ] `Ctrl+Alt+Left` from there: focus moves back.
- [ ] `Ctrl+Alt+Up`/`Down` with a vertical split: focus moves between top/bottom.
- [ ] `Ctrl+Alt+Right` with no pane to the right: no-op.
- [ ] `Ctrl+1`..`Ctrl+9`: switches the Nth tab *within the focused pane only*. Switching panes via `Ctrl+Alt+Arrow` and trying `Ctrl+1` again switches the new pane's Nth tab.
- [ ] `Ctrl+T`: creates a new Shell tab in the focused pane.
- [ ] `Ctrl+W`: closes the focused pane's active tab.

Pane context menu:
- [ ] Right-click on a pane's tab bar background (not on a tab, not on `+`): pane menu opens.
- [ ] "Split horizontally" / "Split vertically": works.
- [ ] "Close pane": works (disabled on root pane).
- [ ] "Move all tabs to" submenu: lists other panes by their active-tab labels; selecting one moves all tabs and closes this pane.
- [ ] Right-click on a tab itself: tab menu (from v1.2) opens, not pane menu.

Focused-pane indicator:
- [ ] After splitting, the focused pane has a visual indicator distinct from the unfocused pane.
- [ ] Click into the unfocused pane: indicator moves.

## Done Criteria

- All 9 "What This Milestone Delivers" items work.
- All "Manual Verification Checklist" items pass on Windows and Linux.
- Splitter resize is smooth (no jitter, no jump-on-grab).
- Min-size clamping behaves correctly during window resize.
- All keyboard shortcuts work or are documented as colliding with OS/webview shortcuts.
- No regression in M1 / M2 / v1.2 behavior.
- `cargo test` passes.
