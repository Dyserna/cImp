// Cross-component handle for "focus the active tab's xterm.js terminal."
// Each Terminal instance registers its tab-keyed focus function on mount
// and clears it on destroy. Callers (compose overlay, tab-switch handler)
// invoke the active tab's focuser without touching xterm directly.

import { get } from 'svelte/store';
import { activeTab } from './tabs/state';
import type { TabId } from './tabs/types';

const focusers: Partial<Record<TabId, () => void>> = {};

export function setTerminalFocuser(tab: TabId, fn: (() => void) | null): void {
  if (fn) {
    focusers[tab] = fn;
  } else {
    delete focusers[tab];
  }
}

/// Focus the currently-active tab's terminal. No-op if the active tab has
/// no registered focuser (e.g. before its Terminal component has mounted).
export function focusTerminal(): void {
  const tab = get(activeTab);
  focusers[tab]?.();
}
