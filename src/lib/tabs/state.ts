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
export const activeTab: Writable<TabId> = writable(defaultTabId(get(harnesses)));

// The registry arrives after mount, so re-seed the placeholder once — but ONLY
// while nothing else has set it. A backend broadcast that already landed is the
// truth and must not be walked back (the store reflects backend truth; see the
// header).
let seeded = false;
harnesses.subscribe((list) => {
  if (seeded || list.length === 0) return;
  seeded = true;
  const fallback = defaultTabId(list);
  activeTab.update((cur) => (cur === '' && fallback !== '' ? fallback : cur));
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
