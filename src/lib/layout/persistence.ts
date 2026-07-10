// Layout persistence: hydration, integrity check, and the debounced
// save subscription that pushes layout-store changes back to the
// backend.
//
// Hydration flow on launch:
//   1. settings.layout is non-null → validateAndRepairLayout adapts it
//      to the live tab list (drops missing tabs, places orphans, etc.).
//   2. settings.layout is null → defaultLayoutForTabs builds a single
//      root pane containing every tab in order. This is the fresh-
//      install / first-launch path.
//
// Save flow at runtime: every layout-store update fires a 250ms
// debounced `save_layout` IPC call. The first emission after
// installLayoutPersistence() (the hydration emission, or the initial
// store value if no hydration happened) is intentionally swallowed —
// we don't want to round-trip the store's value back to the backend
// on launch.
//
// Restore-preset flow: callers swap the layout-store value via
// `layout.set(...)`; the same debounced subscription persists the new
// tree. Presets do not include focus, so callers should compute a
// focus id (typically leftmost-leaf) before setting the store.

import { type Unsubscriber } from 'svelte/store';
import { layout } from './store';
import { saveLayout } from './ipc';
import { closePane, eachPane } from './tree';
import {
  newPaneId,
  type LayoutNode,
  type LayoutState,
  type PaneId,
  type PaneNode,
} from './types';
import type { LayoutPersisted, TabConfig } from '../settings/types';

/// Serialize the in-memory `LayoutState` for the backend. The wire
/// shape is identical to the in-memory shape — both use the
/// `'split' | 'pane'`-discriminated tree with the same field names —
/// so this is structural identity. Kept as a named function so future
/// shape divergences have a single conversion point.
export function serializeLayout(state: LayoutState): LayoutPersisted {
  return { tree: state.tree, focused_pane_id: state.focused_pane_id };
}

/// Build a single root pane containing every tab in order. Used on
/// fresh installs and as the recovery fallback when a persisted layout
/// is irreparably broken. The first tab becomes active.
export function defaultLayoutForTabs(tabs: TabConfig[]): LayoutState {
  const id = newPaneId();
  const tab_ids = tabs.map((t) => t.id);
  const pane: PaneNode = {
    type: 'pane',
    id,
    tab_ids,
    active_tab_id: tab_ids[0] ?? null,
  };
  return { tree: pane, focused_pane_id: id };
}

/// Pane id of the leftmost leaf in `node`. Used to seed
/// `focused_pane_id` after restoring a preset (presets carry only the
/// tree, not focus) and as the deterministic fallback when the
/// persisted `focused_pane_id` no longer exists.
export function leftmostLeafPaneId(node: LayoutNode): PaneId {
  let cursor: LayoutNode = node;
  while (cursor.type === 'split') cursor = cursor.first;
  return cursor.id;
}

/// Map a tree, applying `fn` to every leaf pane. Returns a new root if
/// any pane changed; otherwise the same root by reference. Internal
/// helper for `validateAndRepairLayout` — it's similar to tree.ts's
/// internal `mapPanes` but exported here would couple persistence to
/// the tree module's internals.
function walkPanes(root: LayoutNode, fn: (pane: PaneNode) => PaneNode): LayoutNode {
  if (root.type === 'pane') {
    const next = fn(root);
    return next === root ? root : next;
  }
  const first = walkPanes(root.first, fn);
  const second = walkPanes(root.second, fn);
  if (first === root.first && second === root.second) return root;
  return { ...root, first, second };
}

/// Find the first non-root empty pane in document order, or null if
/// none exists. The integrity loop collapses these one at a time
/// because a single `closePane` call invalidates ids beyond it.
function findFirstEmptyNonRootPane(root: LayoutNode): PaneId | null {
  if (root.type === 'pane') return null;
  for (const pane of eachPane(root)) {
    if (pane.tab_ids.length === 0) return pane.id;
  }
  return null;
}

/// Clamp every split's ratio into the same `[0.05, 0.95]` band that
/// `setSplitRatio` enforces at runtime (non-finite → 0.5). The backend
/// deserializes `ratio` as a bare float with no range check, so a
/// hand-edited settings file can deliver 5.0 or -3.0; unrepaired, the
/// bad value round-trips through every subsequent save and Split.svelte
/// renders one frame of negative flex before its measured clamp kicks
/// in. Returns the same root by reference when nothing changed.
function sanitizeSplitRatios(root: LayoutNode): LayoutNode {
  if (root.type === 'pane') return root;
  const first = sanitizeSplitRatios(root.first);
  const second = sanitizeSplitRatios(root.second);
  const ratio = Number.isFinite(root.ratio)
    ? Math.max(0.05, Math.min(0.95, root.ratio))
    : 0.5;
  if (first === root.first && second === root.second && ratio === root.ratio) {
    return root;
  }
  return { ...root, first, second, ratio };
}

/// Adapt a persisted layout to the live tab list. Six concerns:
///
///   0. Sanitize split ratios (see `sanitizeSplitRatios`).
///   1. Drop tab ids no longer in `settings.tabs` — these are tabs the
///      user deleted between launches — and dedupe: a tab id may appear
///      at most once in the whole tree (first occurrence in document
///      order wins). The backend does no uniqueness check, and a
///      duplicate id reaching the render layer is a real bug — a
///      duplicate within one pane breaks the keyed `{#each}` over
///      `tab_ids`, and a cross-pane duplicate makes two panes fight
///      over the single terminal-host element. The pane's
///      `active_tab_id` is reset to the first remaining tab if it was
///      dropped.
///   2. Place orphan tabs (in `settings.tabs` but not in any pane) at
///      the end of the focused pane's tab list. Common path: a Shell
///      tab created after a preset save, then the preset is restored.
///   3. Validate `focused_pane_id` against the tree — if the persisted
///      focus pane no longer exists, fall back to leftmost leaf.
///   4. Collapse non-root empty panes. The standard `closePane` op
///      handles the binary-tree-deletion rebalance.
///   5. Defensive: if the root is itself an empty pane, replace the
///      whole tree with `defaultLayoutForTabs`. Shouldn't happen in
///      practice (orphan placement covers it) but a hand-edited file
///      could land here.
export function validateAndRepairLayout(
  persisted: LayoutPersisted,
  tabs: TabConfig[],
): LayoutState {
  const validTabIds = new Set(tabs.map((t) => t.id));

  // 0. Sanitize split ratios before anything else touches the tree.
  const sanitized = sanitizeSplitRatios(persisted.tree);

  // 1. Drop unknown tab ids per pane, deduping across the whole tree
  // (walkPanes visits panes in document order, so "first occurrence
  // wins" is deterministic).
  const seenTabIds = new Set<string>();
  let tree: LayoutNode = walkPanes(sanitized, (pane) => {
    const filtered = pane.tab_ids.filter((id) => {
      if (!validTabIds.has(id) || seenTabIds.has(id)) return false;
      seenTabIds.add(id);
      return true;
    });
    // Early-out only when nothing was dropped AND the active tab is actually one
    // of this pane's tabs. Skipping the membership check left a pane whose
    // `active_tab_id` isn't in its own `tab_ids` untouched → blank display.
    if (
      filtered.length === pane.tab_ids.length &&
      pane.active_tab_id !== null &&
      filtered.includes(pane.active_tab_id)
    ) {
      return pane;
    }
    let active = pane.active_tab_id;
    if (active === null || !filtered.includes(active)) {
      active = filtered[0] ?? null;
    }
    if (
      filtered.length === pane.tab_ids.length &&
      active === pane.active_tab_id
    ) {
      return pane;
    }
    return { ...pane, tab_ids: filtered, active_tab_id: active };
  });

  // 3. Validate focused_pane_id (do this before orphan placement so
  // the orphans land in a real pane).
  let focused: PaneId = persisted.focused_pane_id;
  if (!hasPane(tree, focused)) {
    focused = leftmostLeafPaneId(tree);
  }

  // 2. Place orphans at the end of the focused pane.
  const placed = new Set<string>();
  for (const pane of eachPane(tree)) {
    for (const id of pane.tab_ids) placed.add(id);
  }
  const orphans = tabs
    .map((t) => t.id)
    .filter((id) => !placed.has(id));
  if (orphans.length > 0) {
    // Guard against duplicate pane ids in a corrupt file: append the
    // orphans to the *first* pane matching `focused` only — appending
    // to every match would manufacture cross-pane duplicate tab ids.
    let orphansPlaced = false;
    tree = walkPanes(tree, (pane) => {
      if (orphansPlaced || pane.id !== focused) return pane;
      orphansPlaced = true;
      const tab_ids = [...pane.tab_ids, ...orphans];
      const active_tab_id = pane.active_tab_id ?? orphans[0] ?? null;
      return { ...pane, tab_ids, active_tab_id };
    });
  }

  // 4. Collapse non-root empty panes. Iterate because each closePane
  // can change ids further down — taking the first empty pane each
  // pass is simpler than threading an iterator through the rebalance.
  while (tree.type === 'split') {
    const emptyId = findFirstEmptyNonRootPane(tree);
    if (!emptyId) break;
    const { tree: collapsed } = closePane(tree, emptyId);
    if (collapsed === tree) break; // closePane returned unchanged → bail
    tree = collapsed;
    if (!hasPane(tree, focused)) {
      focused = leftmostLeafPaneId(tree);
    }
  }

  // 5. Defensive: empty root pane → rebuild defaults. If the root is
  // itself a single empty pane (no orphans, no tabs at all), build
  // from the live tab list. With the orphan-placement step above this
  // can only fire when `tabs` is itself empty — nothing in the backend
  // guarantees a non-empty tab list (a hand-edited settings file can
  // persist `"tabs": []`), so handle it here rather than render a
  // blank app.
  if (tree.type === 'pane' && tree.tab_ids.length === 0) {
    if (tabs.length === 0) {
      return { tree, focused_pane_id: tree.id };
    }
    return defaultLayoutForTabs(tabs);
  }

  return { tree, focused_pane_id: focused };
}

/// True if any pane in `root` has the given id.
function hasPane(root: LayoutNode, id: PaneId): boolean {
  for (const pane of eachPane(root)) {
    if (pane.id === id) return true;
  }
  return false;
}

/// Install the eager save subscription. Subscribes to the layout store
/// and invokes `save_layout` immediately on every mutation.
///
/// V0.6+ change: pre-V0.6 used a 250ms front-end debounce that left a
/// closing-race window where a layout edit in the last 250ms before
/// `beforeunload` was silently dropped (the IPC promise resolved after
/// the WebView had already torn down). The backend already debounces
/// settings persistence by 500ms, so the front-end debounce was double
/// rate-limiting; removing it closes the race without adding disk
/// writes — the backend still coalesces.
///
/// The very first emission is swallowed: Svelte writables fire on
/// subscribe with the current value, and we don't want to round-trip
/// the just-hydrated layout back to the backend.
///
/// Returns an unsubscribe function.
export function installLayoutPersistence(): Unsubscriber {
  let firstEmission = true;
  return layout.subscribe((state) => {
    if (firstEmission) {
      firstEmission = false;
      return;
    }
    void saveLayout(serializeLayout(state)).catch((e) => {
      console.error('save_layout failed', e);
    });
  });
}

