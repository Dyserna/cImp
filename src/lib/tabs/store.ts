// Runtime tabs store. Backend is the source of truth: the state manager
// emits `TabCreated` for every launch-seed tab on startup, plus runtime
// `TabCreated`/`TabClosed`/`TabRenamed` events as user-managed Shell tabs
// are added/removed/renamed. The store rebuilds from those events alone —
// no static frontend list mirrors the backend launch seed.
//
// Per-tab cached maps (avatar state, errors, closed state) elsewhere in
// the codebase subscribe to `tabs` and add/remove keys as the order
// changes; they do NOT seed from `ALL_TABS` constants the way M1 did.

import { derived, get, writable, type Readable, type Writable } from 'svelte/store';
import { type TabId, type TabKind, type TabMeta } from './types';

export interface TabCreatedEvent {
  tab: TabId;
  kind: TabKind;
  name: string;
  builtin: boolean;
  position: number;
}

/// Live tab order. Mutated only by event handlers. Iterating this store is
/// the canonical way to render the tab bar.
export const tabs: Writable<TabMeta[]> = writable<TabMeta[]>([]);

/// Live IDs in render order — convenient for `{#each}` blocks that key by
/// id and for per-tab map subscriptions that only need to know which tabs
/// exist (not what they're called).
export const tabIds: Readable<TabId[]> = derived(tabs, ($t) => $t.map((m) => m.id));

/// Synchronously look up a tab's meta. Returns `undefined` if the id isn't
/// in the live order — e.g., a stale event or a not-yet-loaded startup.
export function tabMeta(id: TabId): TabMeta | undefined {
  return get(tabs).find((m) => m.id === id);
}

/// Apply a `TabCreated` event. Inserts at the indicated position;
/// idempotent on duplicate ids (existing entry's name/kind are updated to
/// match the latest event so backend-side renames from `reconfigure_shell_
/// tab` reflect immediately).
export function applyTabCreated(e: TabCreatedEvent): void {
  tabs.update((arr) => {
    const existingIdx = arr.findIndex((m) => m.id === e.tab);
    if (existingIdx >= 0) {
      const next = arr.slice();
      next[existingIdx] = { id: e.tab, kind: e.kind, name: e.name, builtin: e.builtin };
      return next;
    }
    const next = arr.slice();
    const insertAt = Math.min(Math.max(e.position, 0), next.length);
    next.splice(insertAt, 0, { id: e.tab, kind: e.kind, name: e.name, builtin: e.builtin });
    return next;
  });
}

/// Apply a `TabClosed` event. No-op if the id isn't in the store.
export function applyTabClosed(tab: TabId): void {
  tabs.update((arr) => arr.filter((m) => m.id !== tab));
}

/// Apply a `TabRenamed` event.
export function applyTabRenamed(tab: TabId, name: string): void {
  tabs.update((arr) =>
    arr.map((m) => (m.id === tab ? { ...m, name } : m)),
  );
}
