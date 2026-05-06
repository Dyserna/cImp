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
  splitPane as splitPaneOp,
} from './tree';
import {
  newPaneId,
  type LayoutNode,
  type LayoutState,
  type PaneId,
  type PaneNode,
  type SplitDirection,
} from './types';

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

/// The pane that the next `tab-created` event should land in. Set by
/// the `+` button before opening the new-shell-tab dialog; consumed
/// (cleared) on the next tab-created. If unset, new tabs land in the
/// focused pane. Two `+` clicks in different panes can't realistically
/// race because the dialog is modal — but if they ever did, the second
/// would clobber the first, which is acceptable for M1.
export const pendingTabTargetPane: Writable<PaneId | null> = writable(null);

/// Note that the next created tab should be placed into `paneId`.
export function requestTabIntoPane(paneId: PaneId): void {
  pendingTabTargetPane.set(paneId);
}

/// Append a newly-created tab to its target pane. Routing rules:
///   1. If `pendingTabTargetPane` is set and that pane still exists,
///      use it (and clear the cell).
///   2. Else use the currently focused pane.
///   3. Else (defensive) use the first pane in document order.
/// The newly-added tab becomes the target pane's active tab and the
/// target pane becomes focused — matching v1.2's "switch to the new tab
/// on creation" behavior, scoped to a pane.
export function applyTabCreatedToLayout(tabId: TabId): void {
  layout.update((state) => {
    let targetId = get(pendingTabTargetPane);
    if (targetId) {
      pendingTabTargetPane.set(null);
      // Check the requested pane still exists; if not, fall through.
      let stillExists = false;
      for (const p of paneIter(state.tree)) {
        if (p.id === targetId) {
          stillExists = true;
          break;
        }
      }
      if (!stillExists) targetId = null;
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

/// Split the focused pane in the given direction, moving its currently
/// active tab into the new pane. New pane gets focus. No-op when the
/// focused pane has no active tab (empty pane). Used by the M1 debug
/// menu and the Ctrl+\ / Ctrl+Shift+\ shortcuts in M3.
export function splitFocusedPane(direction: SplitDirection): void {
  layout.update((state) => {
    const pane = findPaneInTree(state.tree, state.focused_pane_id);
    if (!pane || !pane.active_tab_id) return state;
    const result = splitPaneOp(state.tree, pane.id, direction, pane.active_tab_id);
    if (!result) return state;
    return { tree: result.tree, focused_pane_id: result.newPaneId };
  });
}

/// Replace the entire tree with a single root pane containing every tab
/// in the current layout, in their current document order. Convenience
/// for the debug menu's "Reset layout" entry.
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
    const id = newPaneId();
    const root: PaneNode = {
      type: 'pane',
      id,
      tab_ids: allTabs,
      active_tab_id: firstActive ?? allTabs[0] ?? null,
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
