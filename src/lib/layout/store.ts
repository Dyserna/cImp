// Layout store. Holds the current layout tree and the focused-pane id.
// In M1 the tree starts as a single root pane that all tabs land in;
// debug-menu splits and (later) drag-and-drop mutate it from there.
// Persistence does not exist yet — the store rebuilds from scratch each
// launch, mirroring the snapshot flow that App.svelte already runs for
// the tabs store.
//
// The store also owns two thin lifecycle hooks called from the same
// places that mutate the tabs store:
//   * applyTabCreatedToLayout — append a new tab to its target pane.
//   * applyTabClosedFromLayout — remove a tab from whichever pane holds
//     it, collapsing non-root panes that become empty.
// Keeping them here (rather than in tabs/store.ts) avoids a circular
// import and keeps every layout-mutating path in one file.

import { derived, get, writable, type Readable, type Writable } from 'svelte/store';
import type { TabId } from '../tabs/types';
import type { DropTarget } from '../dnd/types';
import {
  closePane,
  findPaneContainingTab,
  firstPane,
  insertTabIntoPane,
  removeTab,
  setActiveTabId,
  setSplitRatio as setSplitRatioOp,
  setSplitRatios as setSplitRatiosOp,
  splitPane as splitPaneOp,
} from './tree';
import {
  newPaneId,
  type LayoutNode,
  type LayoutState,
  type PaneId,
  type PaneNode,
  type SplitDirection,
  type SplitId,
} from './types';
import { paneRegistry } from './registry';

/// The id of the initial root pane. Stable across the run because the
/// store creates the pane once at module load; persistence (M4) is what
/// would make these ids stable across launches.
const ROOT_PANE_ID: PaneId = newPaneId();

const initialPane: PaneNode = {
  type: 'pane',
  id: ROOT_PANE_ID,
  tab_ids: [],
  active_tab_id: null,
};

/// The full layout state — tree plus focused pane id. Subscribers (the
/// renderer, the focused-pane-derived stores below) re-run on every
/// reference-changing update; the tree ops are immutable so this happens
/// naturally as long as callers swap the result rather than mutating in
/// place.
export const layout: Writable<LayoutState> = writable<LayoutState>({
  tree: initialPane,
  focused_pane_id: ROOT_PANE_ID,
});

/// The pane that currently has focus. Resolves to the first pane in
/// document order if the focused id has gone stale (defensive — every
/// mutation that drops the focused pane should reset focus before the
/// store update lands, but the fallback prevents a null-deref on a
/// transient inconsistency).
export const focusedPane: Readable<PaneNode> = derived(layout, ($l) => {
  const { tree, focused_pane_id } = $l;
  for (const pane of paneIter(tree)) {
    if (pane.id === focused_pane_id) return pane;
  }
  return firstPane(tree);
});

/// The active tab of the focused pane — i.e. "the tab the application
/// considers active" for avatar/audio/compose routing. `null` only when
/// the focused pane is empty (transient).
export const focusedActiveTabId: Readable<TabId | null> = derived(
  focusedPane,
  ($p) => $p.active_tab_id,
);

function* paneIter(root: LayoutNode): Generator<PaneNode> {
  if (root.type === 'pane') {
    yield root;
    return;
  }
  yield* paneIter(root.first);
  yield* paneIter(root.second);
}

/// Where the next `tab-created` event should place its tab.
///
///   * `kind: 'pane'` — append to an existing pane. Set by the `+`
///     button on a pane's tab bar (so the new tab lands in the clicked
///     pane rather than the focused one) and cleared after consumption.
///   * `kind: 'split'` — split `sourcePaneId` and put the new tab in
///     the new sibling pane. Set by the `Ctrl+\` / `Ctrl+Shift+\`
///     pane-split shortcuts and the pane-context-menu's split entries
///     in M3. The source pane's tabs and active id are preserved.
///
/// If unset, new tabs append to the focused pane (the v1.2-equivalent
/// behavior). Two simultaneous requests would clobber each other; in
/// practice the dialog and the keyboard shortcuts are mutually
/// exclusive (the dialog is modal, and the shortcuts complete
/// synchronously before the user can chain another).
export type PendingTabPlacement =
  | { kind: 'pane'; paneId: PaneId }
  | {
      kind: 'split';
      sourcePaneId: PaneId;
      direction: SplitDirection;
      placeOn: 'first' | 'second';
    };

/// FIFO queue of pending placements. A queue — not a single cell — so two
/// guided creations in flight at once don't clobber each other: pressing a
/// split shortcut twice in quick succession (key-repeat / fast double-press)
/// before the first `tab-created` arrives used to overwrite the first request,
/// sending its shell to the focused pane instead of its split. Each
/// `tab-created` consumes the oldest placement, and a single backend emits
/// those events in creation order, so guided creations route correctly in
/// order.
///
/// Residual limitation: an UNGUIDED creation (e.g. a programmatic / tool-tab
/// open that doesn't enqueue a placement) whose `tab-created` interleaves
/// between a guided request and that request's own event can still consume the
/// wrong placement. Fully closing that needs id-level correlation, which the
/// create-then-emit ordering (the event often arrives before the create IPC
/// resolves) doesn't reliably allow.
const placementQueue: PendingTabPlacement[] = [];

/// Note that the next created tab should be placed into `paneId`. Used
/// by the per-pane `+` button. Returns the queued placement so the
/// caller can `cancelPlacement` it if its create IPC fails.
export function requestTabIntoPane(paneId: PaneId): PendingTabPlacement {
  const placement: PendingTabPlacement = { kind: 'pane', paneId };
  placementQueue.push(placement);
  return placement;
}

/// Note that the next created tab should land in a fresh pane created
/// by splitting `sourcePaneId`. Used by the keyboard split shortcuts
/// and the pane context menu's split entries. Returns the queued
/// placement so the caller can `cancelPlacement` it on IPC failure.
export function requestTabIntoSplit(
  sourcePaneId: PaneId,
  direction: SplitDirection,
  placeOn: 'first' | 'second',
): PendingTabPlacement {
  const placement: PendingTabPlacement = { kind: 'split', sourcePaneId, direction, placeOn };
  placementQueue.push(placement);
  return placement;
}

/// Remove a specific queued placement. Called when the create IPC a
/// request was paired with fails — no `tab-created` will arrive to
/// consume it, so leaving it queued would mis-route the next real tab.
///
/// Identity-based (not "pop the last") because two guided creations can
/// be in flight at once: with queue [A, B], A's IPC failing must remove
/// A, not B — a LIFO pop would cancel the still-valid B and leave the
/// stale A to hijack B's own tab-created event. No-op when the
/// placement was already consumed.
export function cancelPlacement(placement: PendingTabPlacement): void {
  const idx = placementQueue.indexOf(placement);
  if (idx >= 0) placementQueue.splice(idx, 1);
}

/// Test-only queue reset so placement tests are order-independent.
/// Never call this from production code.
export function _resetPlacementQueueForTests(): void {
  placementQueue.length = 0;
}

/// Place a newly-created tab into the layout. Routing rules:
///
///   1. If `pendingTabPlacement` is `{ kind: 'split', ... }` and the
///      source pane still exists, split it and place the new tab in
///      the new sibling pane. Source pane's tab list and active tab
///      are preserved verbatim. Focus moves to the new pane.
///   2. If `pendingTabPlacement` is `{ kind: 'pane', ... }` and the
///      pane still exists, append to that pane and focus it.
///   3. Otherwise append to the focused pane (or, defensively, the
///      first pane in document order). Focus is unchanged unless the
///      target was the first-pane fallback, in which case it follows.
///
/// In all cases the new tab becomes the target pane's active tab,
/// matching v1.2's "switch to the new tab on creation" behavior scoped
/// to a pane. The oldest queued placement is consumed (shifted off) on
/// every call, regardless of which branch fires, so a subsequent
/// `tab-created` event can't accidentally reuse stale routing.
export function applyTabCreatedToLayout(tabId: TabId): void {
  layout.update((state) => {
    const placement = placementQueue.shift() ?? null;

    if (placement && placement.kind === 'split') {
      const source = findPaneInTree(state.tree, placement.sourcePaneId);
      if (source) {
        // splitPane is tolerant of a draggedTabId not present in the
        // target pane: in that case the target is preserved verbatim
        // and the new sibling pane gets the (brand-new) tab as its
        // only entry. This is exactly what we want for shortcut-driven
        // splits.
        const result = splitPaneOp(
          state.tree,
          placement.sourcePaneId,
          placement.direction,
          tabId,
          { placeOn: placement.placeOn },
        );
        if (result) {
          return { tree: result.tree, focused_pane_id: result.newPaneId };
        }
      }
      // Source pane vanished between the request and the tab-created
      // event (rare — would require a near-simultaneous structural
      // mutation). Fall through to the default routing.
    }

    let targetId: PaneId | null = null;
    if (placement && placement.kind === 'pane') {
      // Check the requested pane still exists; if not, fall through.
      if (findPaneInTree(state.tree, placement.paneId)) {
        targetId = placement.paneId;
      }
    }
    if (!targetId) {
      // Use focused pane if it still exists, else first pane.
      let focused: PaneNode | null = null;
      for (const p of paneIter(state.tree)) {
        if (p.id === state.focused_pane_id) {
          focused = p;
          break;
        }
      }
      targetId = (focused ?? firstPane(state.tree)).id;
    }

    const pane = findPaneInTree(state.tree, targetId);
    const insertAt = pane ? pane.tab_ids.length : 0;
    const tree = insertTabIntoPane(state.tree, targetId, tabId, insertAt, {
      activate: true,
    });
    return { tree, focused_pane_id: targetId };
  });
}

function findPaneInTree(root: LayoutNode, id: PaneId): PaneNode | null {
  for (const pane of paneIter(root)) {
    if (pane.id === id) return pane;
  }
  return null;
}

/// Remove a tab from the layout. If the holding pane becomes empty and
/// is not the root, collapse it via the standard rebalance. Focus moves
/// to whichever pane the rebalance produces — for a leaf collapse, that
/// is the surviving sibling subtree's first pane in document order.
export function applyTabClosedFromLayout(tabId: TabId): void {
  layout.update((state) => {
    const { tree: afterRemove, paneId } = removeTab(state.tree, tabId);
    if (!paneId) {
      return state.tree === afterRemove ? state : { ...state, tree: afterRemove };
    }
    const pane = findPaneInTree(afterRemove, paneId);
    if (pane && pane.tab_ids.length === 0 && afterRemove.type !== 'pane') {
      // Non-root empty pane → collapse. closePane returns the
      // deepest-leftmost leaf of the surviving sibling subtree as
      // `next_focus`; prefer that over a tree-wide firstPane so focus
      // stays close to where the user just was.
      const { tree: collapsed, next_focus } = closePane(afterRemove, paneId);
      let nextFocus = state.focused_pane_id;
      const focusedStillExists = findPaneInTree(collapsed, nextFocus) !== null;
      if (!focusedStillExists || nextFocus === paneId) {
        nextFocus = next_focus ?? firstPane(collapsed).id;
      }
      return { tree: collapsed, focused_pane_id: nextFocus };
    }
    return { tree: afterRemove, focused_pane_id: state.focused_pane_id };
  });
}

/// Re-insert a tab that exists in the tabs store but is absent from the
/// layout tree — a UI-hidden tab being revealed (hiding removes the tab
/// from the tree exactly like closing does, so its pane space is freed;
/// see tabs/visibility.ts). Appends it to the focused pane, makes it that
/// pane's active tab, and focuses the pane. No-op when the tab already
/// lives in some pane (defensive against a double-reveal).
export function restoreTabToLayout(tabId: TabId): void {
  layout.update((state) => {
    if (findPaneContainingTab(state.tree, tabId)) return state;
    const focused =
      findPaneInTree(state.tree, state.focused_pane_id) ?? firstPane(state.tree);
    const tree = insertTabIntoPane(state.tree, focused.id, tabId, focused.tab_ids.length, {
      activate: true,
    });
    return { tree, focused_pane_id: focused.id };
  });
}

/// Set the active tab of a specific pane. The pane is also focused as a
/// side effect — clicking a tab is a focus action by definition.
export function setPaneActiveTab(paneId: PaneId, tabId: TabId): void {
  layout.update((state) => {
    const tree = setActiveTabId(state.tree, paneId, tabId);
    if (tree === state.tree && state.focused_pane_id === paneId) return state;
    return { tree, focused_pane_id: paneId };
  });
}

/// Focus a specific pane without changing its active tab.
export function setFocusedPane(paneId: PaneId): void {
  layout.update((state) => {
    if (state.focused_pane_id === paneId) return state;
    if (!findPaneInTree(state.tree, paneId)) return state;
    return { ...state, focused_pane_id: paneId };
  });
}

/// Replace the entire tree with a single root pane containing every tab
/// in the current layout, in their current document order. Convenience
/// for the debug menu's "Reset layout" entry.
///
/// The merged pane's active tab is the *focused* pane's active tab (the
/// one driving avatar/audio/compose routing right now), falling back to
/// the first active tab in document order — resetting the layout
/// shouldn't silently switch what the user is looking at.
export function resetLayoutToSinglePane(): void {
  layout.update((state) => {
    const allTabs: TabId[] = [];
    let firstActive: TabId | null = null;
    for (const pane of paneIter(state.tree)) {
      for (const id of pane.tab_ids) allTabs.push(id);
      if (firstActive === null && pane.active_tab_id !== null) {
        firstActive = pane.active_tab_id;
      }
    }
    const focusedActive =
      findPaneInTree(state.tree, state.focused_pane_id)?.active_tab_id ?? null;
    const id = newPaneId();
    const root: PaneNode = {
      type: 'pane',
      id,
      tab_ids: allTabs,
      active_tab_id: focusedActive ?? firstActive ?? allTabs[0] ?? null,
    };
    return { tree: root, focused_pane_id: id };
  });
}

/// Set the active tab on the focused pane to `tabId` (in addition to
/// reconciling focus). Used by the v1.2 shortcut `Ctrl+1..9` translated
/// into pane-scoped semantics in M3.
export function setFocusedPaneActiveTab(tabId: TabId): void {
  const state = get(layout);
  const pane = findPaneContainingTab(state.tree, tabId);
  if (!pane) return;
  setPaneActiveTab(pane.id, tabId);
}

/// If the named pane exists, has zero tabs, and is not the root,
/// collapse it via the standard rebalance. Returns the (possibly
/// unchanged) tree. Caller composes this between a removeTab and
/// the next structural op when handling a drag drop.
function collapseIfEmpty(tree: LayoutNode, paneId: PaneId): LayoutNode {
  const pane = findPaneInTree(tree, paneId);
  if (!pane) return tree;
  if (pane.tab_ids.length > 0) return tree;
  if (tree.type === 'pane') return tree;
  const { tree: collapsed } = closePane(tree, paneId);
  return collapsed;
}

/// Apply a committed drag drop to the layout. Three branches:
///
///   * `reorder`: same-pane index move. Adjusts the insert index for
///     the gap left by removal so "drop on the original spot" is a
///     no-op rather than a left-shift.
///   * `moveToPane`: cross-pane move. Removes from source, collapses
///     the source if it was the source's last tab and source is not
///     root, appends to destination.
///   * `split`: edge drop. Same-pane edges call splitPane directly so
///     the kept pane and the new sibling are produced atomically; if
///     the dragged tab was the source's only one, the kept side is
///     empty and gets collapsed (which dissolves the just-created
///     split, leaving a clean replacement). Cross-pane edges remove
///     from source first (with optional collapse) and then split the
///     destination.
///
/// Focus follows the dropped tab in every branch — to the same pane
/// for reorder, to the destination for moveToPane, to the new pane
/// for split.
export function commitDrop(
  tabId: TabId,
  sourcePaneId: PaneId,
  target: DropTarget,
): void {
  layout.update((state) => {
    if (target.kind === 'reorder') {
      if (target.paneId !== sourcePaneId) return state;
      const sourcePane = findPaneInTree(state.tree, sourcePaneId);
      if (!sourcePane) return state;
      const oldIndex = sourcePane.tab_ids.indexOf(tabId);
      if (oldIndex < 0) return state;
      // Shift the insert index down by one if the removal happens to
      // its left, so dragging a tab a few slots forward lands at the
      // expected position rather than one short.
      let insertIndex = target.insertIndex;
      if (insertIndex > oldIndex) insertIndex -= 1;
      if (insertIndex === oldIndex) return state;
      const { tree: afterRemove } = removeTab(state.tree, tabId);
      const tree = insertTabIntoPane(afterRemove, sourcePaneId, tabId, insertIndex, {
        activate: true,
      });
      return { tree, focused_pane_id: sourcePaneId };
    }

    if (target.kind === 'moveToPane') {
      if (target.paneId === sourcePaneId) return state;
      const { tree: afterRemove } = removeTab(state.tree, tabId);
      const collapsed = collapseIfEmpty(afterRemove, sourcePaneId);
      const targetPane = findPaneInTree(collapsed, target.paneId);
      if (!targetPane) return state;
      const tree = insertTabIntoPane(
        collapsed,
        target.paneId,
        tabId,
        targetPane.tab_ids.length,
        { activate: true },
      );
      return { tree, focused_pane_id: target.paneId };
    }

    // split
    const direction: SplitDirection =
      target.direction === 'left' || target.direction === 'right'
        ? 'horizontal'
        : 'vertical';
    const placeOn: 'first' | 'second' =
      target.direction === 'left' || target.direction === 'top' ? 'first' : 'second';

    if (target.paneId === sourcePaneId) {
      // Same-pane edge drop. splitPane sees the tab in target.tab_ids
      // and removes it from the kept side as part of the split.
      const result = splitPaneOp(state.tree, target.paneId, direction, tabId, { placeOn });
      if (!result) return state;
      let tree = result.tree;
      // If the dragged tab was the source's only one, the kept side
      // is empty after splitPane. Collapsing it dissolves the brand-new
      // split; the new pane (with the dragged tab) becomes the
      // replacement of the original target.
      const kept = findPaneInTree(tree, sourcePaneId);
      if (kept && kept.tab_ids.length === 0 && tree.type !== 'pane') {
        const { tree: collapsed } = closePane(tree, sourcePaneId);
        tree = collapsed;
      }
      return { tree, focused_pane_id: result.newPaneId };
    }

    // Cross-pane split: source ≠ target. Remove from source, collapse
    // if source emptied, then split the destination with the imported
    // tab. splitPane's tolerant contract (M2 step 1 change) means it
    // accepts a draggedTabId not present in the target.
    const { tree: afterRemove } = removeTab(state.tree, tabId);
    const collapsed = collapseIfEmpty(afterRemove, sourcePaneId);
    const result = splitPaneOp(collapsed, target.paneId, direction, tabId, { placeOn });
    if (!result) return state;
    return { tree: result.tree, focused_pane_id: result.newPaneId };
  });
}

/// Update a split's ratio. Thin wrapper around the tree op so the
/// splitter drag handler can call it without importing tree.ts
/// directly. Clamping to `[0.05, 0.95]` happens in the tree op; the
/// drag handler also applies a min-pixel clamp on top of that to
/// guarantee neither pane shrinks below MIN_PANE_*_PX.
export function setSplitRatio(splitId: SplitId, ratio: number): void {
  layout.update((state) => {
    const tree = setSplitRatioOp(state.tree, splitId, ratio);
    if (tree === state.tree) return state;
    return { ...state, tree };
  });
}

/// Apply several split-ratio updates atomically — one store update, one
/// tree walk, one render. The splitter drag uses this to move a divider
/// while keeping every pane NOT adjacent to it at its absolute size: the
/// dragged split's ratio and the compensating ratios of its nested
/// same-direction splits must land in the same frame (see layout/resize.ts).
export function setSplitRatios(
  updates: ReadonlyArray<{ id: SplitId; ratio: number }>,
): void {
  layout.update((state) => {
    const tree = setSplitRatiosOp(state.tree, updates);
    if (tree === state.tree) return state;
    return { ...state, tree };
  });
}

/// Close the focused pane: move all of its tabs into the leftmost-leaf
/// pane of the surviving sibling subtree (preserving order; the source's
/// active tab becomes the destination's active tab so the user's
/// "current thread" stays current), then collapse the now-empty source.
/// No-op when the focused pane is the root — there is nowhere for the
/// tabs to go.
///
/// Builtin AI tabs come along automatically because
/// they are normal entries in `tab_ids`; the close-tab IPC's
/// `builtin-not-closable` guard doesn't apply here — we are moving the
/// tab, not closing it.
export function closeFocusedPane(): void {
  layout.update((state) => {
    const focusedId = state.focused_pane_id;
    const focused = findPaneInTree(state.tree, focusedId);
    if (!focused) return state;
    // Root pane has no sibling — nothing to merge into.
    if (state.tree.type === 'pane') return state;

    // Find the surviving sibling subtree's leftmost leaf so the moved
    // tabs land somewhere predictable and close to where the user just
    // was. closePane returns this id as `next_focus`, but we need it
    // *before* the close so we can move the tabs into it first.
    const tabsToMove = [...focused.tab_ids];
    const previousActive = focused.active_tab_id;

    // Snapshot the sibling subtree to compute its leftmost leaf.
    let parentSplit: { first: LayoutNode; second: LayoutNode } | null = null;
    for (const node of nodeIter(state.tree)) {
      if (node.type !== 'split') continue;
      if (
        (node.first.type === 'pane' && node.first.id === focusedId) ||
        (node.second.type === 'pane' && node.second.id === focusedId)
      ) {
        parentSplit = node;
        break;
      }
    }
    if (!parentSplit) return state;
    const sibling = parentSplit.first.type === 'pane' && parentSplit.first.id === focusedId
      ? parentSplit.second
      : parentSplit.first;
    const targetPaneId = firstPane(sibling).id;

    // Move tabs one at a time so each call's tree update is consistent.
    let tree: LayoutNode = state.tree;
    for (const tabId of tabsToMove) {
      const { tree: afterRemove } = removeTab(tree, tabId);
      const target = findPaneInTree(afterRemove, targetPaneId);
      if (!target) {
        // Defensive: target vanished (unreachable — removeTab never
        // deletes panes). Abort the whole op atomically: continuing to
        // the closePane below would drop the just-removed tab on the
        // floor AND destroy every not-yet-moved tab with the source
        // pane subtree.
        return state;
      }
      tree = insertTabIntoPane(afterRemove, targetPaneId, tabId, target.tab_ids.length, {
        activate: false,
      });
    }

    // Restore the source's previously-active tab as the destination's
    // active tab so the user's current thread keeps the spotlight.
    if (previousActive !== null) {
      tree = setActiveTabId(tree, targetPaneId, previousActive);
    }

    // Collapse the now-empty source pane via the standard rebalance.
    const { tree: collapsed } = closePane(tree, focusedId);
    return { tree: collapsed, focused_pane_id: targetPaneId };
  });
}

/// Yield every node (splits + panes) in the tree in document order.
/// Internal helper for `closeFocusedPane`'s parent-finding walk.
function* nodeIter(root: LayoutNode): Generator<LayoutNode> {
  yield root;
  if (root.type === 'split') {
    yield* nodeIter(root.first);
    yield* nodeIter(root.second);
  }
}

/// Move keyboard focus to the geometrically-adjacent pane in the given
/// direction. Adjacency is computed against the live `getBoundingClientRect`
/// of every registered pane: candidates must be in the named direction
/// (their leading edge >= the focused pane's trailing edge, with a 1px
/// tolerance for floating-point splitter widths) AND must overlap the
/// focused pane's perpendicular axis. Among qualifying candidates, the
/// closest one wins. No-op when no candidate exists in that direction.
export function focusPaneInDirection(direction: 'left' | 'right' | 'up' | 'down'): void {
  const state = get(layout);
  const focusedRect = paneRegistry.getPaneRect(state.focused_pane_id);
  if (!focusedRect) return;

  let bestPane: PaneId | null = null;
  let bestDistance = Infinity;
  let bestOverlap = -Infinity;

  for (const pane of paneIter(state.tree)) {
    if (pane.id === state.focused_pane_id) continue;
    const r = paneRegistry.getPaneRect(pane.id);
    if (!r) continue;

    let inDirection: boolean;
    let distance: number;
    let overlap: number;
    switch (direction) {
      case 'left':
        inDirection = r.right <= focusedRect.left + 1;
        distance = focusedRect.left - r.right;
        overlap = Math.max(0, Math.min(r.bottom, focusedRect.bottom) - Math.max(r.top, focusedRect.top));
        break;
      case 'right':
        inDirection = r.left >= focusedRect.right - 1;
        distance = r.left - focusedRect.right;
        overlap = Math.max(0, Math.min(r.bottom, focusedRect.bottom) - Math.max(r.top, focusedRect.top));
        break;
      case 'up':
        inDirection = r.bottom <= focusedRect.top + 1;
        distance = focusedRect.top - r.bottom;
        overlap = Math.max(0, Math.min(r.right, focusedRect.right) - Math.max(r.left, focusedRect.left));
        break;
      case 'down':
        inDirection = r.top >= focusedRect.bottom - 1;
        distance = r.top - focusedRect.bottom;
        overlap = Math.max(0, Math.min(r.right, focusedRect.right) - Math.max(r.left, focusedRect.left));
        break;
    }
    if (!inDirection) continue;
    if (overlap <= 0) continue;

    // Tie-break: smallest distance wins; among equal-distance
    // candidates, prefer the one with the largest perpendicular
    // overlap (most "in line" with the focused pane).
    if (distance < bestDistance || (distance === bestDistance && overlap > bestOverlap)) {
      bestDistance = distance;
      bestOverlap = overlap;
      bestPane = pane.id;
    }
  }

  if (bestPane) setFocusedPane(bestPane);
}

/// Move all of `sourcePaneId`'s tabs into `targetPaneId`, then collapse
/// the source. Used by the pane context menu's "Move all tabs to →"
/// submenu. The source pane's active tab becomes the destination's
/// active tab. Focus moves to the destination.
///
/// No-op when source and target are the same, when either pane doesn't
/// exist, or when the source is the root pane (root cannot be
/// collapsed).
export function moveAllTabsToPane(sourcePaneId: PaneId, targetPaneId: PaneId): void {
  if (sourcePaneId === targetPaneId) return;
  layout.update((state) => {
    const source = findPaneInTree(state.tree, sourcePaneId);
    const target = findPaneInTree(state.tree, targetPaneId);
    if (!source || !target) return state;
    if (state.tree.type === 'pane') return state;

    const tabsToMove = [...source.tab_ids];
    const previousActive = source.active_tab_id;

    let tree: LayoutNode = state.tree;
    for (const tabId of tabsToMove) {
      const { tree: afterRemove } = removeTab(tree, tabId);
      const t = findPaneInTree(afterRemove, targetPaneId);
      if (!t) {
        // Defensive: target vanished (unreachable — removeTab never
        // deletes panes). Abort atomically rather than losing the
        // removed tab and collapsing the rest with the source pane.
        return state;
      }
      tree = insertTabIntoPane(afterRemove, targetPaneId, tabId, t.tab_ids.length, {
        activate: false,
      });
    }
    if (previousActive !== null) {
      tree = setActiveTabId(tree, targetPaneId, previousActive);
    }
    const { tree: collapsed } = closePane(tree, sourcePaneId);
    return { tree: collapsed, focused_pane_id: targetPaneId };
  });
}
