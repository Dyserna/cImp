// Active-tab state. The store reflects backend truth: `switchTab(id)` calls
// `set_active_tab` on the backend (which both activates and persists the id
// to settings.session.active_tab_id), and the backend broadcasts an
// ActiveTabChanged event that drives the store. We do NOT optimistically
// flip locally — the frontend's view of "which tab is active" must match
// what the backend's TabRegistry says, otherwise PTY routing diverges.

import { writable, type Writable } from 'svelte/store';
import { setActiveTab } from '../settings/ipc';
import { type TabId } from './types';

export const activeTab: Writable<TabId> = writable('claude');

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
