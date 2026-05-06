// Per-tab spawn/exit error store. Drives the in-tab error overlay so the
// user sees a clear "X failed to start" message with a Retry button instead
// of an opaque terminal write.

import { writable } from 'svelte/store';
import type { TabId } from './types';

export interface TabError {
  // One-line user-facing summary, e.g. "Aider failed to start." or
  // "Aider exited unexpectedly."
  headline: string;
  // Raw error text from the backend or PTY exit payload.
  raw: string;
  // Optional follow-up text (installation instructions, link to docs, etc.).
  hint?: string;
}

type ErrorMap = Partial<Record<TabId, TabError | null>>;

/// Starts empty; entries are added on `setTabError` and removed on
/// `clearTabError` or when the owning tab is closed (the avatarState
/// listener calls `clearTabError` on `tab-closed`).
export const tabErrors = writable<ErrorMap>({});

export function setTabError(tab: TabId, err: TabError) {
  tabErrors.update((m) => ({ ...m, [tab]: err }));
}

export function clearTabError(tab: TabId) {
  tabErrors.update((m) => {
    if (!(tab in m)) return m;
    const next = { ...m };
    delete next[tab];
    return next;
  });
}
