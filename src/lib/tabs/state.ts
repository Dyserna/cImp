// Active-tab state. The store reflects backend truth: `switchTab(id)` calls
// `tab_activate` on the backend, and the backend broadcasts an
// ActiveTabChanged event that drives the store. We do NOT optimistically
// flip locally — the frontend's view of "which tab is active" must match
// what the backend's TabRegistry says, otherwise PTY routing diverges.

import { writable, type Writable } from 'svelte/store';
import { tabActivate } from '../ipc';
import { type TabId } from './types';

export const activeTab: Writable<TabId> = writable('claude');

/// Request a tab switch. The store updates when the backend broadcasts
/// ActiveTabChanged. If the IPC fails the local store does not flip — the
/// user sees the old tab remain selected, which is the correct UX for a
/// failed activation.
export async function switchTab(tab: TabId): Promise<void> {
  try {
    await tabActivate(tab);
  } catch (e) {
    console.error('tab_activate failed:', e);
  }
}
