import { writable, derived, get, type Readable } from 'svelte/store';
import { listen, type UnlistenFn } from '@tauri-apps/api/event';
import { settings, applySettings } from './settings/store';
import { activeTab } from './tabs/state';
import {
  applyTabClosed,
  applyTabCreated,
  applyTabRenamed,
  type TabCreatedEvent,
} from './tabs/store';
import {
  applyTabClosedFromLayout,
  applyTabCreatedToLayout,
} from './layout/store';
import { createTerminal, destroyTerminal } from './terminals';
import { destroyAppView } from './appViews';
import { forgetHiddenTab } from './tabs/visibility';
import { clearTabError } from './tabs/errorState';
import { type TabId } from './tabs/types';

export type AvatarState = 'Idle' | 'Listening' | 'Thinking' | 'Speaking' | 'Error';

export type AvatarErrorKind = 'subprocess-exited' | 'tts-error' | 'audio-error';

export interface AvatarErrorInfo {
  tab: TabId;
  kind: AvatarErrorKind;
  message: string;
}

export interface TabClosedState {
  closed: boolean;
  exit_code: number | null;
  /// Custom overlay message (set by the backend on a launch failure like
  /// command-not-found). When non-null, the overlay shows this in place
  /// of the standard "Shell exited (code N)" line and Enter routes to
  /// the Configure dialog instead of restart.
  closed_message: string | null;
}

// Backend wire format for the `avatar-state` event. Includes the runtime
// tab-lifecycle events (`tab-created`, `tab-closed`, `tab-renamed`); the
// listener fans them out to both the per-tab caches here and the `tabs`
// store in `tabs/store.ts` so a single subscription drives everything.
type StateEvent =
  | { type: 'state-changed'; tab: TabId; state: AvatarState }
  | { type: 'active-tab-changed'; tab: TabId }
  | { type: 'awaiting-permission-changed'; tab: TabId; awaiting: boolean }
  | { type: 'done-while-away-changed'; tab: TabId; done: boolean }
  | {
      type: 'tab-closed-state-changed';
      tab: TabId;
      closed: boolean;
      exit_code: number | null;
      closed_message: string | null;
    }
  | ({ type: 'tab-created' } & TabCreatedEvent)
  | { type: 'tab-closed'; tab: TabId }
  | { type: 'tab-renamed'; tab: TabId; name: string };

// Per-tab avatar state cache. The displayed avatar is a derived view over
// (this map, activeTab) — switching tabs immediately re-renders without an
// extra backend round-trip.
const perTabState = writable<Partial<Record<TabId, AvatarState>>>({});

/// Per-tab AwaitingPermission flag. Driven by backend permission detection.
/// Exposed for the TabBar's per-tab indicator rendering. Always false for
/// Shell tabs (the detector never runs for them).
export const perTabAwaitingPermission = writable<Partial<Record<TabId, boolean>>>({});

/// Per-tab DoneWhileAway flag. Set by the backend when a tab transitions to
/// Idle while inactive; cleared on tab activation.
export const perTabDoneWhileAway = writable<Partial<Record<TabId, boolean>>>({});

/// Per-tab Shell-only closed state. Driven by the backend's
/// `tab-closed-state-changed` event; the ClosedShellOverlay component
/// subscribes to render the "Shell exited (code N)" message.
export const perTabClosedState = writable<Partial<Record<TabId, TabClosedState>>>({});

/// Default per-tab values used when a `TabCreated` event arrives. Per-tab
/// records here key by id, so we just `set` the entry instead of merging
/// in a fresh map (preserves any concurrent updates that arrived first).
/// Exposed for `App.svelte`'s startup snapshot path so the per-tab caches
/// have entries before any event arrives.
export function seedPerTabEntries(tab: TabId): void {
  perTabState.update((m) => (tab in m ? m : { ...m, [tab]: 'Idle' }));
  perTabAwaitingPermission.update((m) => (tab in m ? m : { ...m, [tab]: false }));
  perTabDoneWhileAway.update((m) => (tab in m ? m : { ...m, [tab]: false }));
  perTabClosedState.update((m) =>
    tab in m
      ? m
      : { ...m, [tab]: { closed: false, exit_code: null, closed_message: null } },
  );
}

function dropPerTabEntries(tab: TabId): void {
  const drop = (m: Record<TabId, unknown>) => {
    if (!(tab in m)) return m;
    const next = { ...m };
    delete next[tab];
    return next;
  };
  perTabState.update(
    drop as (m: Partial<Record<TabId, AvatarState>>) => Partial<Record<TabId, AvatarState>>,
  );
  perTabAwaitingPermission.update(
    drop as (m: Partial<Record<TabId, boolean>>) => Partial<Record<TabId, boolean>>,
  );
  perTabDoneWhileAway.update(
    drop as (m: Partial<Record<TabId, boolean>>) => Partial<Record<TabId, boolean>>,
  );
  perTabClosedState.update(
    drop as (m: Partial<Record<TabId, TabClosedState>>) => Partial<Record<TabId, TabClosedState>>,
  );
}

/// Per-tab avatar state map exposed for TabBar (it needs all tabs at once,
/// not just the active one). Entries are added on `TabCreated` and removed
/// on `TabClosed`; readers should fall back to `Idle` when an id is absent
/// during the brief window between mount and the first event arrival.
export const perTabAvatarState: Readable<Partial<Record<TabId, AvatarState>>> = derived(
  perTabState,
  (s) => s,
);

/// Active tab's avatar state. Components that show the avatar subscribe to
/// this; it recomputes whenever either the per-tab cache or the active tab
/// changes. Defaults to Idle if the active tab's entry hasn't been seeded
/// yet (TabCreated arrives slightly after the listener attaches).
export const avatarState: Readable<AvatarState> = derived(
  [perTabState, activeTab],
  ([s, t]) => s[t] ?? 'Idle',
);

/// Per-tab error info. Like `avatarState`, the displayed banner is the
/// active tab's error.
const perTabError = writable<Partial<Record<TabId, AvatarErrorInfo | null>>>({});

export const avatarError: Readable<AvatarErrorInfo | null> = derived(
  [perTabError, activeTab],
  ([s, t]) => s[t] ?? null,
);

export const avatarVisible: Readable<boolean> = derived(
  settings,
  (s) => s.avatar.visible,
);

export function toggleAvatarVisible(): void {
  const s = get(settings);
  void applySettings({ ...s, avatar: { ...s.avatar, visible: !s.avatar.visible } });
}

/// Clear the error info for a specific tab. Called by the error banner's
/// dismiss action; the backend `acknowledge_error` IPC drops the tab's
/// state out of Error on the next transition.
export function clearAvatarError(tab: TabId): void {
  perTabError.update((m) => ({ ...m, [tab]: null }));
}

let statePromise: Promise<UnlistenFn> | null = null;
let errorPromise: Promise<UnlistenFn> | null = null;

/// Tab ids closed this session. Per-tab events can arrive late (emitted
/// around the close); without this guard a stale `state-changed` /
/// `avatar-error` after `tab-closed` would resurrect a ghost cache entry
/// that nothing ever cleans up again.
const closedTabs = new Set<TabId>();

/// Subscribe to backend state broadcasts. Idempotent, and app-lifetime by
/// design: these events drive global stores (tabs, activeTab, layout,
/// terminals), not just the avatar overlay, so nothing may ever unsubscribe
/// them — which is why no UnlistenFn is exposed.
export function startAvatarStateListener(): Promise<void> {
  if (!statePromise) {
    statePromise = listen<StateEvent>('avatar-state', (event) => {
      const e = event.payload;
      if (e.type === 'state-changed') {
        if (closedTabs.has(e.tab)) return;
        perTabState.update((m) => ({ ...m, [e.tab]: e.state }));
        if (e.state !== 'Error') {
          perTabError.update((m) => ({ ...m, [e.tab]: null }));
        }
      } else if (e.type === 'active-tab-changed') {
        activeTab.set(e.tab);
      } else if (e.type === 'awaiting-permission-changed') {
        if (closedTabs.has(e.tab)) return;
        perTabAwaitingPermission.update((m) => ({ ...m, [e.tab]: e.awaiting }));
      } else if (e.type === 'done-while-away-changed') {
        if (closedTabs.has(e.tab)) return;
        perTabDoneWhileAway.update((m) => ({ ...m, [e.tab]: e.done }));
      } else if (e.type === 'tab-closed-state-changed') {
        if (closedTabs.has(e.tab)) return;
        perTabClosedState.update((m) => ({
          ...m,
          [e.tab]: {
            closed: e.closed,
            exit_code: e.exit_code,
            closed_message: e.closed_message,
          },
        }));
      } else if (e.type === 'tab-created') {
        // Seed per-tab caches BEFORE applying to the tabs store so any
        // subscriber that reacts to `tabs` changes (e.g., TabBar
        // rendering) finds non-stale per-tab data on first paint.
        closedTabs.delete(e.tab); // id reuse: the new tab is live again
        seedPerTabEntries(e.tab);
        applyTabCreated({
          tab: e.tab,
          kind: e.kind,
          name: e.name,
          builtin: e.builtin,
          position: e.position,
        });
        createTerminal(e.tab);
        applyTabCreatedToLayout(e.tab);
        // The tab is now IN the layout; a lingering hidden flag (builtin
        // re-materialized from Settings reuses its stable id) would
        // contradict the hidden ⇔ not-in-layout invariant.
        forgetHiddenTab(e.tab);
      } else if (e.type === 'tab-closed') {
        closedTabs.add(e.tab);
        applyTabClosed(e.tab);
        applyTabClosedFromLayout(e.tab);
        // Prune the hidden flag so a future tab reusing this id doesn't
        // start life invisibly hidden.
        forgetHiddenTab(e.tab);
        destroyTerminal(e.tab);
        // Keep-alive app views (Workbench, Graph View, …) are only truly
        // unmounted here — a plain hide/tab-switch just detaches them.
        destroyAppView(e.tab);
        dropPerTabEntries(e.tab);
        perTabError.update((m) => {
          if (!(e.tab in m)) return m;
          const next = { ...m };
          delete next[e.tab];
          return next;
        });
        // Also drop the in-tab error-overlay entry so a fresh tab
        // re-using the same id (unlikely with UUIDs, but possible if
        // M3 settings load mirrors a stable id) starts clean.
        clearTabError(e.tab);
      } else if (e.type === 'tab-renamed') {
        applyTabRenamed(e.tab, e.name);
      }
    });
  }
  if (!errorPromise) {
    errorPromise = listen<AvatarErrorInfo>('avatar-error', (event) => {
      const info = event.payload;
      if (closedTabs.has(info.tab)) return;
      perTabError.update((m) => ({ ...m, [info.tab]: info }));
    });
  }
  return statePromise.then(() => undefined);
}

/// Read-only access to `tabs` for callers that import from this module.
/// Re-exported here because most callers already import from this file.
export { tabs } from './tabs/store';
