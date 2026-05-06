// Layout-tree operations. All functions take and return immutable trees —
// callers swap the new root into the layout store, which triggers Svelte
// reactivity. Mutating in place would defeat the `writable.update`
// reference-equality short-circuit and force every subscriber to rerun.
//
// These ops are pure and pane-id-addressed; the lifecycle layer
// (move-then-collapse, split-then-focus) lives one level up in the layout
// store. Keeping the operations pure lets `tree.test.ts` cover them
// without spinning up Svelte stores.

import type { TabId } from '../tabs/types';
import {
  newPaneId,
  newSplitId,
  type LayoutNode,
  type PaneId,
  type PaneNode,
  type SplitDirection,
  type SplitId,
  type SplitNode,
} from './types';

/// Walk the tree to find the pane with the given id. Returns `null` when
/// no pane matches. O(n) in tree size; trees are small (handfuls of nodes
/// in practice), so a recursive walk is fine.
export function findPane(root: LayoutNode, id: PaneId): PaneNode | null {
  if (root.type === 'pane') {
    return root.id === id ? root : null;
  }
  return findPane(root.first, id) ?? findPane(root.second, id);
}

/// Find the Split node that has the given pane as a direct child. Returns
/// `null` when the pane is the root or doesn't exist. Used by `closePane`
/// (needs the parent split to perform the standard binary-tree-deletion
/// rebalance) and by drag-and-drop code in M2.
export function findSplitContaining(root: LayoutNode, paneId: PaneId): SplitNode | null {
  if (root.type === 'pane') return null;
  if (
    (root.first.type === 'pane' && root.first.id === paneId) ||
    (root.second.type === 'pane' && root.second.id === paneId)
  ) {
    return root;
  }
  return findSplitContaining(root.first, paneId) ?? findSplitContaining(root.second, paneId);
}

/// Substitute one node for another anywhere in the tree, returning a new
/// root. If `target` is the root itself, returns `replacement`. Internal
/// helper for the structural ops below.
function replaceNode(
  root: LayoutNode,
  target: LayoutNode,
  replacement: LayoutNode,
): LayoutNode {
  if (root === target) return replacement;
  if (root.type === 'pane') return root;
  const first = replaceNode(root.first, target, replacement);
  const second = replaceNode(root.second, target, replacement);
  if (first === root.first && second === root.second) return root;
  return { ...root, first, second };
}

/// Map every pane in the tree through `fn`, returning a new root. Used
/// internally by the per-pane mutators below (`moveTab`, `setActiveTabId`).
function mapPanes(root: LayoutNode, fn: (pane: PaneNode) => PaneNode): LayoutNode {
  if (root.type === 'pane') {
    const next = fn(root);
    return next === root ? root : next;
  }
  const first = mapPanes(root.first, fn);
  const second = mapPanes(root.second, fn);
  if (first === root.first && second === root.second) return root;
  return { ...root, first, second };
}

/// Insert a tab into a pane's tab list at `position` (clamped to bounds).
/// If the pane was empty, the inserted tab becomes active. Otherwise the
/// existing active tab is preserved unless the caller passes
/// `activate: true`.
export function insertTabIntoPane(
  root: LayoutNode,
  paneId: PaneId,
  tabId: TabId,
  position: number,
  options: { activate?: boolean } = {},
): LayoutNode {
  return mapPanes(root, (pane) => {
    if (pane.id !== paneId) return pane;
    if (pane.tab_ids.includes(tabId)) return pane;
    const insertAt = Math.max(0, Math.min(position, pane.tab_ids.length));
    const tab_ids = [...pane.tab_ids.slice(0, insertAt), tabId, ...pane.tab_ids.slice(insertAt)];
    const wasEmpty = pane.tab_ids.length === 0;
    const active_tab_id =
      wasEmpty || options.activate ? tabId : pane.active_tab_id;
    return { ...pane, tab_ids, active_tab_id };
  });
}

/// Remove a tab from whichever pane currently holds it. Returns the new
/// tree and the id of the pane that held it (or `null` if not found). If
/// the removed tab was active, the new active tab is the one to its left
/// (or to its right if it was the leftmost), or `null` if the pane is now
/// empty. The caller (lifecycle layer) decides whether to collapse a
/// now-empty pane.
export function removeTab(
  root: LayoutNode,
  tabId: TabId,
): { tree: LayoutNode; paneId: PaneId | null } {
  let foundPaneId: PaneId | null = null;
  const tree = mapPanes(root, (pane) => {
    const idx = pane.tab_ids.indexOf(tabId);
    if (idx < 0) return pane;
    foundPaneId = pane.id;
    const tab_ids = pane.tab_ids.slice(0, idx).concat(pane.tab_ids.slice(idx + 1));
    let active_tab_id: TabId | null = pane.active_tab_id;
    if (pane.active_tab_id === tabId) {
      active_tab_id = tab_ids.length === 0 ? null : tab_ids[Math.max(0, idx - 1)] ?? null;
    }
    return { ...pane, tab_ids, active_tab_id };
  });
  return { tree, paneId: foundPaneId };
}

/// Move a tab between panes (or reorder within the same pane). The
/// underlying ops are remove + insert, so this is the composition of
/// those two; the resulting tree is consistent at every step.
export function moveTab(
  root: LayoutNode,
  tabId: TabId,
  toPaneId: PaneId,
  position: number,
): LayoutNode {
  const { tree: removed } = removeTab(root, tabId);
  return insertTabIntoPane(removed, toPaneId, tabId, position, { activate: true });
}

/// Set a pane's active tab. No-op if the tab is not in the pane.
export function setActiveTabId(root: LayoutNode, paneId: PaneId, tabId: TabId): LayoutNode {
  return mapPanes(root, (pane) => {
    if (pane.id !== paneId) return pane;
    if (!pane.tab_ids.includes(tabId)) return pane;
    if (pane.active_tab_id === tabId) return pane;
    return { ...pane, active_tab_id: tabId };
  });
}

/// Split a pane in the given direction, creating a new sibling pane
/// that holds `draggedTabId`. If the tab is currently in the target
/// pane, it is removed from the kept side first; if it's elsewhere in
/// the tree (the cross-pane drag-to-split case), the caller is
/// expected to have already removed it from its source pane and the
/// target is preserved verbatim.
///
/// `placeOn` controls which side of the new split the *new* pane lands
/// on. Default `'second'` keeps the legacy convention (new pane to the
/// right for horizontal, bottom for vertical) used by the M1 debug
/// menu and the M3 keyboard splits. M2's DnD layer flips it to
/// `'first'` for left/top edge drops.
///
/// Returns the new tree, the id of the new pane, and the id of the
/// containing split. The caller (typically the layout store) decides
/// whether to focus the new pane.
export function splitPane(
  root: LayoutNode,
  paneId: PaneId,
  direction: SplitDirection,
  draggedTabId: TabId,
  options: { placeOn?: 'first' | 'second' } = {},
): { tree: LayoutNode; newPaneId: PaneId; splitId: SplitId } | null {
  const target = findPane(root, paneId);
  if (!target) return null;

  let keptPane: PaneNode;
  if (target.tab_ids.includes(draggedTabId)) {
    const remainingTabIds = target.tab_ids.filter((id) => id !== draggedTabId);
    const remainingActive =
      target.active_tab_id === draggedTabId
        ? (remainingTabIds[0] ?? null)
        : target.active_tab_id;
    keptPane = {
      ...target,
      tab_ids: remainingTabIds,
      active_tab_id: remainingActive,
    };
  } else {
    keptPane = target;
  }

  const newPaneIdValue = newPaneId();
  const newPane: PaneNode = {
    type: 'pane',
    id: newPaneIdValue,
    tab_ids: [draggedTabId],
    active_tab_id: draggedTabId,
  };

  const placeOn = options.placeOn ?? 'second';
  const splitId = newSplitId();
  const split: SplitNode = {
    type: 'split',
    id: splitId,
    direction,
    ratio: 0.5,
    first: placeOn === 'first' ? newPane : keptPane,
    second: placeOn === 'first' ? keptPane : newPane,
  };

  const tree = replaceNode(root, target, split);
  return { tree, newPaneId: newPaneIdValue, splitId };
}

/// Remove a pane from the tree. The parent Split is replaced by the
/// surviving sibling (standard binary-tree-deletion). If `paneId` is
/// the root or not in the tree, the tree is returned unchanged.
///
/// Also returns `next_focus`: the deepest-leftmost leaf of the
/// surviving sibling subtree (per DESIGN-V4's focus-on-collapse rule),
/// or `null` when the close was a no-op. Callers that need to refocus
/// after a collapse use this directly; `firstPane(tree)` would pick
/// the leftmost pane of the *whole* tree, which jumps focus much
/// further than the user expects.
export function closePane(
  root: LayoutNode,
  paneId: PaneId,
): { tree: LayoutNode; next_focus: PaneId | null } {
  if (root.type === 'pane') return { tree: root, next_focus: null };
  const parent = findSplitContaining(root, paneId);
  if (!parent) return { tree: root, next_focus: null };
  const sibling = parent.first.type === 'pane' && parent.first.id === paneId
    ? parent.second
    : parent.first;
  const tree = replaceNode(root, parent, sibling);
  return { tree, next_focus: firstPane(sibling).id };
}

/// Update a split's ratio, clamped to `[0.05, 0.95]` to prevent panes
/// from collapsing to invisible widths. Min-pixel-size enforcement
/// against the rendered geometry is a separate M3 concern; this clamp
/// is just to keep the data model sane.
export function setSplitRatio(root: LayoutNode, splitId: SplitId, ratio: number): LayoutNode {
  const clamped = Math.max(0.05, Math.min(0.95, ratio));
  if (root.type === 'pane') return root;
  if (root.id === splitId) {
    if (root.ratio === clamped) return root;
    return { ...root, ratio: clamped };
  }
  const first = setSplitRatio(root.first, splitId, clamped);
  const second = setSplitRatio(root.second, splitId, clamped);
  if (first === root.first && second === root.second) return root;
  return { ...root, first, second };
}

/// Yield every pane in document order (left-to-right, depth-first). Used
/// by integrity checks (e.g. orphan-tab detection in M4) and by callers
/// that need to iterate every pane regardless of nesting.
export function* eachPane(root: LayoutNode): Generator<PaneNode> {
  if (root.type === 'pane') {
    yield root;
    return;
  }
  yield* eachPane(root.first);
  yield* eachPane(root.second);
}

/// First pane in document order. Useful as a fallback when "the focused
/// pane" needs to be re-resolved after a structural change.
export function firstPane(root: LayoutNode): PaneNode {
  for (const pane of eachPane(root)) return pane;
  // Unreachable: the tree invariant requires at least one pane.
  throw new Error('layout tree contains no panes');
}

/// Find the pane currently holding `tabId`. Returns `null` if no pane
/// holds it.
export function findPaneContainingTab(root: LayoutNode, tabId: TabId): PaneNode | null {
  for (const pane of eachPane(root)) {
    if (pane.tab_ids.includes(tabId)) return pane;
  }
  return null;
}
