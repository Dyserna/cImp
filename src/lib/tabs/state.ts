// Active-tab state. The store reflects backend truth: `switchTab(id)` calls
// `set_active_tab` on the backend (which both activates and persists the id
// to settings.session.active_tab_id), and the backend broadcasts an
// ActiveTabChanged event that drives the store. We do NOT optimistically
// flip locally — the frontend's view of "which tab is active" must match
// what the backend's TabRegistry says, otherwise PTY routing diverges.

import { get, writable, type Writable } from 'svelte/store';
import { setActiveTab } from '../settings/ipc';
import { defaultTabId, harnesses } from '../harness';
import { type TabId } from './types';

/// The tab the store reports before the backend has broadcast anything.
///
/// V40 Phase F (locked decision 27): the registry's default — the first
/// reserved tab of the first registered harness — rather than one harness's id
/// written here. It is a placeholder in the strictest sense: the backend's
/// `ActiveTabChanged` overwrites it within the first frames, and until then no
/// PTY routing depends on it.
const active = writable<TabId>(defaultTabId(get(harnesses)));

/// True once anything AUTHORITATIVE has written the store — a backend
/// `ActiveTabChanged` broadcast, or a restored `session.active_tab_id`. The
/// placeholder re-seed below is inert from that moment on, whatever the
/// ordering.
let claimed = false;

/// The active tab, as the backend reports it.
///
/// Writes go through this wrapper so the store can tell "the placeholder" from
/// "something that knows" (V40 review findings L-18 and the ordering hazard
/// beside it). The re-seed used to guard on `cur === ''`, which
/// `defaultTabId(get(harnesses))` can never produce — `reservedAiTabIds` has a
/// bootstrap fallback — so the correction was dead code AND, had the bootstrap
/// ever gone away, it could have fired after a restored tab id had already
/// landed and yanked the user back to the first harness's tab.
export const activeTab: Writable<TabId> = {
  subscribe: active.subscribe,
  set: (v) => {
    claimed = true;
    active.set(v);
  },
  update: (fn) => {
    claimed = true;
    active.update(fn);
  },
};

// The registry arrives after mount, so correct the placeholder once — but ONLY
// while nothing authoritative has spoken. The store reflects backend truth (see
// the header), and a value that came from the backend is never walked back.
let seeded = false;
harnesses.subscribe((list) => {
  if (seeded || list.length === 0) return;
  seeded = true;
  if (claimed) return;
  const fallback = defaultTabId(list);
  if (fallback !== '' && get(active) !== fallback) active.set(fallback);
});

/// Request a tab switch. The store updates when the backend broadcasts
/// ActiveTabChanged. The id is also persisted to settings.session so the
/// last-active tab is restored on next launch (debounced on the backend).
/// If the IPC fails the local store does not flip — the user sees the old
/// tab remain selected, which is the correct UX for a failed activation.
export async function switchTab(tab: TabId): Promise<void> {
  try {
    await setActiveTab(tab);
  } catch (e) {
    console.error('set_active_tab failed:', e);
  }
}
