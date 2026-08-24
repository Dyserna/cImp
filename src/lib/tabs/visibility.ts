// UI-only tab visibility (the status bar's eye button). A hidden tab is
// removed from the layout tree exactly as if it were closed — its tab-bar
// slot disappears, a pane emptied by the hide collapses, and the surviving
// panes take over the freed space — but unlike a real close, the tab stays
// in the tabs store and its PTY / backend feed keeps running. Un-hiding
// re-inserts the tab into the focused pane and activates it, so its
// still-live content is immediately visible. The hidden set is a *view*
// preference, deliberately decoupled from whether a feature is enabled, so it
// is not part of settings; since V42 Phase C it lives in the per-project
// `.cimp/ui_state.json` (see `../uiState.ts`) rather than in localStorage —
// hiding a tab in one checkout no longer hides it in every checkout.
//
// Invariant: a tab is hidden ⇔ it is absent from the layout tree (while
// still present in the tabs store). Everything that renders or indexes
// pane.tab_ids therefore needs no hidden-awareness at all.
//
// This module owns the RUNTIME half of that invariant — hide, show, show-all,
// reveal, and the lifecycle prunes. The boot-time half (a persisted tree does
// not contain hidden tabs, but they are in settings.tabs and so look like
// orphans to the layout repair) moved into the backend in V42 Phase B: the
// repair reads this same set from `.cimp/ui_state.json` and never places them.
// See `src-tauri/src/settings/layout.rs`.

import { get, writable, type Writable } from 'svelte/store';
import {
  applyTabClosedFromLayout,
  focusedActiveTabId,
  restoreTabToLayout,
  setFocusedPaneActiveTab,
} from '../layout/store';
import { HIDDEN_TABS_KEY, getUiValue, setUiValue } from '../uiState';
import { switchTab } from './state';
import { tabMeta } from './store';
import type { TabId } from './types';

function load(): ReadonlySet<TabId> {
  try {
    const raw = getUiValue(HIDDEN_TABS_KEY);
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
  setUiValue(HIDDEN_TABS_KEY, JSON.stringify([...set]));
}

/// The hidden tab ids. Subscribed by the popover (checkbox state + count
/// pip). Stale ids (tabs closed while hidden) are pruned by the tab-closed
/// lifecycle hook via `forgetHiddenTab`; any that predate that hook are
/// harmless — the popover lists only live tabs and the restore path checks
/// the tabs store before touching the layout.
///
/// Starts EMPTY and is filled by [`hydrateHiddenTabs`]. It used to be
/// `writable(load())` — resolved at module-import time, which worked only
/// because localStorage answers synchronously from the very first line of the
/// bundle. The project file does not; see [`hydrateHiddenTabs`] for the
/// ordering that replaces it.
export const hiddenTabs: Writable<ReadonlySet<TabId>> = writable(new Set());

/// Fill the hidden set from the hydrated UI-state cache.
///
/// **Ordering invariant**, weaker since V42 Phase B. The hidden set used to
/// have to be filled before App's `onMount` ran `stripHiddenTabsFromLayout()`,
/// because layout hydration re-added every hidden tab as an orphan and that
/// call was what took them back out; an empty set at that moment un-hid
/// everything and the popover's next write then persisted the emptiness. The
/// backend reads the same set straight from `.cimp/ui_state.json` now and hands
/// the main window a tree the hidden tabs were never in, so that failure mode
/// is gone.
///
/// What remains: App's `onMount` calls `isTabHidden` on the fresh-install path
/// (no persisted tree exists for the backend to have repaired), and the popover
/// renders from this store. `main.ts` still orders it — `hydrateUiState()` then
/// this, both awaited before `mount(App)`. Calling this twice is harmless: it
/// is a pure read of the cache.
export function hydrateHiddenTabs(): void {
  hiddenTabs.set(load());
}

export function isTabHidden(id: TabId): boolean {
  return get(hiddenTabs).has(id);
}

function mark(id: TabId, hide: boolean): void {
  hiddenTabs.update((old) => {
    if (hide === old.has(id)) return old;
    const next = new Set(old);
    if (hide) next.add(id);
    else next.delete(id);
    persist(next);
    return next;
  });
}

/// Mirror a layout-driven change of the focused pane's active tab to the
/// backend, so audio/avatar/compose routing follows — same contract as a
/// tab click. `before` is the focused active id captured before the layout
/// mutation.
function mirrorFocusedSwitch(before: TabId | null): void {
  const after = get(focusedActiveTabId);
  if (after !== null && after !== before) void switchTab(after);
}

/// Hide or show one tab.
///
///   * Hide: remove the tab from whichever pane holds it via the same
///     lifecycle op a real close uses — the pane re-points its active tab
///     at a neighbor, and a pane left empty collapses so its space is
///     redistributed. The PTY / terminal host are untouched.
///   * Show: re-insert the tab at the end of the focused pane and activate
///     it (the whole point of un-hiding is to look at it) — the terminal
///     host re-attaches and re-fits on activation, so the live content is
///     visible immediately.
export function setTabHidden(id: TabId, hide: boolean): void {
  if (hide === isTabHidden(id)) return;
  const before = get(focusedActiveTabId);
  mark(id, hide);
  if (hide) {
    applyTabClosedFromLayout(id);
  } else if (tabMeta(id)) {
    restoreTabToLayout(id);
  }
  mirrorFocusedSwitch(before);
}

/// Clear the whole hidden set (the popover's "Show all"). Every live hidden
/// tab is restored into the focused pane in set order; stale ids are simply
/// dropped.
export function showAllTabs(): void {
  const before = get(focusedActiveTabId);
  for (const id of get(hiddenTabs)) {
    if (tabMeta(id)) restoreTabToLayout(id);
  }
  const empty = new Set<TabId>();
  hiddenTabs.set(empty);
  persist(empty);
  mirrorFocusedSwitch(before);
}

/// Bring a tab on-screen: un-hide it if hidden (which re-inserts and
/// activates it), otherwise activate it in whichever pane holds it. Used by
/// explicit activation affordances (Note button, Workbench diff badge) that
/// must work whether or not the user has hidden their tab.
export function revealTab(id: TabId): void {
  if (isTabHidden(id)) setTabHidden(id, false);
  else setFocusedPaneActiveTab(id);
}

/// Drop a tab's hidden flag without touching the layout. Runtime lifecycle
/// hook, called on:
///   * tab-created — the create path has just placed the tab in the layout,
///     so a lingering flag would break the hidden ⇔ not-in-layout invariant
///     (builtin tabs re-materialized from Settings reuse their stable id);
///   * tab-closed — prune, so a future tab reusing the id doesn't start
///     life invisibly hidden.
export function forgetHiddenTab(id: TabId): void {
  mark(id, false);
}
