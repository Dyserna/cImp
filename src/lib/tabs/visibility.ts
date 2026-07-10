// UI-only tab visibility (the status bar's eye button). Hiding a tab removes
// it from every pane's TAB BAR and nothing else: the tab keeps its slot in
// the layout tree and the tabs store, its PTY / backend feed keeps running,
// and the Settings toggles that materialize builtin tabs are untouched — so
// un-hiding shows an up-to-date tab, not a restarted one. The set lives in
// localStorage rather than settings because it's a per-machine *view*
// preference, deliberately decoupled from whether a feature is enabled.

import { get, writable, type Writable } from 'svelte/store';
import { layout } from '../layout/store';
import type { LayoutNode, PaneNode } from '../layout/types';
import { switchTab } from './state';
import type { TabId } from './types';

const STORAGE_KEY = 'cimp.hidden-tabs.v1';

function load(): ReadonlySet<TabId> {
  try {
    const raw = localStorage.getItem(STORAGE_KEY);
    if (!raw) return new Set();
    const arr: unknown = JSON.parse(raw);
    return new Set(
      Array.isArray(arr) ? arr.filter((x): x is string => typeof x === 'string') : [],
    );
  } catch {
    return new Set();
  }
}

function persist(set: ReadonlySet<TabId>): void {
  try {
    localStorage.setItem(STORAGE_KEY, JSON.stringify([...set]));
  } catch {
    // A quota/serialization failure loses persistence, never breaks the UI.
  }
}

/// The hidden tab ids. Subscribed by TabBar (render filter), the Ctrl+N
/// shortcut handler (index over visible tabs only), and the popover
/// (checkbox state). Stale ids (tabs closed while hidden) are harmless —
/// they simply never match a live tab again.
export const hiddenTabs: Writable<ReadonlySet<TabId>> = writable(load());

export function isTabHidden(id: TabId): boolean {
  return get(hiddenTabs).has(id);
}

function mapPanes(root: LayoutNode, fn: (p: PaneNode) => PaneNode): LayoutNode {
  if (root.type === 'pane') return fn(root);
  const first = mapPanes(root.first, fn);
  const second = mapPanes(root.second, fn);
  if (first === root.first && second === root.second) return root;
  return { ...root, first, second };
}

/// Re-point every pane whose active tab is hidden at its nearest visible
/// neighbor (rightward first, then leftward; `null` when the whole pane is
/// hidden — the pane then renders empty but its tabs keep running). Focus is
/// left alone: hiding a tab in a background pane must not steal it. When the
/// FOCUSED pane's active tab changes, the switch is mirrored to the backend
/// so audio/avatar/compose routing follows — same contract as a tab click.
export function reconcileActiveTabsWithHidden(): void {
  const hidden = get(hiddenTabs);
  let focusedSwitch: TabId | null = null;
  layout.update((state) => {
    let changed = false;
    const tree = mapPanes(state.tree, (pane) => {
      const active = pane.active_tab_id;
      if (active === null || !hidden.has(active)) return pane;
      const idx = pane.tab_ids.indexOf(active);
      const after = pane.tab_ids.slice(idx + 1).find((id) => !hidden.has(id));
      const before = pane.tab_ids
        .slice(0, Math.max(idx, 0))
        .reverse()
        .find((id) => !hidden.has(id));
      const next = after ?? before ?? null;
      changed = true;
      if (pane.id === state.focused_pane_id && next !== null) focusedSwitch = next;
      return { ...pane, active_tab_id: next };
    });
    return changed ? { ...state, tree } : state;
  });
  if (focusedSwitch !== null) void switchTab(focusedSwitch);
}

/// Hide or show one tab in the UI. Hiding also re-points any pane that had
/// it active (see `reconcileActiveTabsWithHidden`) — synchronously, so
/// Pane.svelte's reveal-on-explicit-activation effect never sees the
/// transient hidden-but-active state and can't fight the hide.
export function setTabHidden(id: TabId, hide: boolean): void {
  hiddenTabs.update((old) => {
    if (hide === old.has(id)) return old;
    const next = new Set(old);
    if (hide) next.add(id);
    else next.delete(id);
    persist(next);
    return next;
  });
  if (hide) reconcileActiveTabsWithHidden();
}

/// Clear the whole hidden set (the popover's "Show all").
export function showAllTabs(): void {
  const empty = new Set<TabId>();
  hiddenTabs.set(empty);
  persist(empty);
}

/// Translate a reorder insert index computed over the RENDERED (visible)
/// tabs into an index into the pane's full `tab_ids`. The dnd hit-tester
/// measures DOM rects, and hidden tabs have no rect — without this, a drop
/// with hidden tabs present would land shifted left by however many hidden
/// tabs precede it.
export function fullReorderIndex(paneId: string, visibleIndex: number): number {
  const hidden = get(hiddenTabs);
  if (hidden.size === 0) return visibleIndex;
  let pane: PaneNode | null = null;
  mapPanes(get(layout).tree, (p) => {
    if (p.id === paneId) pane = p;
    return p;
  });
  if (pane === null) return visibleIndex;
  const tabIds: readonly TabId[] = (pane as PaneNode).tab_ids;
  const visible = tabIds.filter((id) => !hidden.has(id));
  if (visibleIndex >= visible.length) return tabIds.length;
  const idx = tabIds.indexOf(visible[visibleIndex]);
  return idx < 0 ? tabIds.length : idx;
}
