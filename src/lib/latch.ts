import { invoke } from '@tauri-apps/api/core';
import { writable, derived, type Readable } from 'svelte/store';
import type { TabId } from './tabs/types';

/// V32 Phase F (locked decisions 14 + 15): the per-tab taint-latch state that
/// drives the tab-chrome badge and its override popover.
///
/// The same rows are on the loopback's `GET /status`, but every loopback route
/// is bearer-token authenticated — the webview has no token and must not get
/// one — so this goes through the `latch_status` IPC command, which reads the
/// backend's in-process registry directly.

/// One tab's containment state. Mirror of Rust `LatchStatus` + the flattened
/// `LatchView` (`offload/loopback.rs`).
export interface LatchRow {
  /// `claude` / `opencode` — the normalized agent vocabulary.
  consumer: string;
  /// The cImp tab id this latch belongs to.
  tab: TabId;
  /// The harness session the latch is scoped to; null while the V28 registry
  /// withholds one.
  session: string | null;
  /// `open` | `external` | `local`.
  latch: string;
  /// Whether external content has entered this conversation at all. Survives
  /// every override; only a tab restart (session rotation) clears it.
  contaminated: boolean;
  /// Whether "switch to local" applies right now (EXTERNAL-latched only).
  can_flip_local: boolean;
  /// Whether "restore full access" applies right now (anything but open).
  can_unlatch: boolean;
}

/// The two user-initiated moves. Mirror of Rust `LatchOverride`.
export type LatchAction = 'flip_local' | 'unlatch';

export async function fetchLatchStatus(): Promise<LatchRow[]> {
  return invoke<LatchRow[]>('latch_status');
}

/// Apply an override and return the tab's new state. Rejects with the
/// backend's message (unknown action, nothing latched, illegal transition) —
/// the popover shows it verbatim rather than failing silently.
export async function applyLatchOverride(
  tab: TabId,
  consumer: string,
  action: LatchAction,
): Promise<void> {
  await invoke('latch_override', { tab, consumer, action });
}

/// Every latch row the backend knows about, keyed by tab id. A tab absent from
/// the map has never had a gated call — no badge.
export const latchByTab = writable<Partial<Record<TabId, LatchRow>>>({});

/// Whether a tab should show the taint badge at all: it is latched in either
/// direction, or its conversation is contaminated. An `open` + uncontaminated
/// row (a tab that only ever used TRUSTED tools) shows nothing — the badge has
/// to mean something when it appears.
export function isTainted(row: LatchRow | undefined): boolean {
  return !!row && (row.latch !== 'open' || row.contaminated);
}

/// Tab ids currently showing a badge. Exposed mostly for tests and debugging;
/// the tab bar reads `latchByTab` directly so it can render the row's detail.
export const taintedTabs: Readable<TabId[]> = derived(latchByTab, ($m) =>
  Object.entries($m)
    .filter(([, row]) => isTainted(row as LatchRow))
    .map(([tab]) => tab as TabId),
);

/// How often the badge state is refreshed.
///
/// Polled rather than evented: the latch moves inside the loopback's request
/// handlers, which hold no `AppHandle` to emit from, and the read is a mutex
/// plus a handful of map entries — no I/O, no allocation worth naming. 4s is
/// well under the time it takes a user to notice a fetch happened and far
/// above any cost worth optimizing.
const POLL_MS = 4000;

/// Start polling `latch_status`. Returns a stop function; safe to call the
/// stop more than once. An individual failed poll is swallowed (the app may
/// still be starting) — the store keeps its last known value rather than
/// flickering every badge off.
export function startLatchPolling(): () => void {
  let stopped = false;
  const tick = async (): Promise<void> => {
    try {
      const rows = await fetchLatchStatus();
      if (stopped) return;
      const next: Partial<Record<TabId, LatchRow>> = {};
      for (const row of rows) {
        // One tab id belongs to exactly one agent in practice. If two rows
        // ever collide, the latched/contaminated one wins: under-reporting
        // taint is the failure this whole surface exists to prevent.
        const prev = next[row.tab];
        if (!prev || (!isTainted(prev) && isTainted(row))) next[row.tab] = row;
      }
      latchByTab.set(next);
    } catch {
      /* app still starting, or the command is unavailable — keep last state */
    }
  };
  void tick();
  const handle = setInterval(() => void tick(), POLL_MS);
  return () => {
    stopped = true;
    clearInterval(handle);
  };
}
