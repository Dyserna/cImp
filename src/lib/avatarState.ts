import { writable, derived, get, type Readable } from 'svelte/store';
import { listen, type UnlistenFn } from '@tauri-apps/api/event';
import { settings, applySettings } from './settings/store';
import { activeTab } from './tabs/state';
import { ALL_TABS, type TabId } from './tabs/types';

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
}

// Backend wire format for the `avatar-state` event.
type StateEvent =
  | { type: 'state-changed'; tab: TabId; state: AvatarState }
  | { type: 'active-tab-changed'; tab: TabId }
  | { type: 'awaiting-permission-changed'; tab: TabId; awaiting: boolean }
  | { type: 'done-while-away-changed'; tab: TabId; done: boolean }
  | { type: 'tab-closed-state-changed'; tab: TabId; closed: boolean; exit_code: number | null };

function defaultRecord<V>(value: V): Record<TabId, V> {
  const out: Record<TabId, V> = {} as Record<TabId, V>;
  for (const t of ALL_TABS) out[t] = value;
  return out;
}

// Per-tab avatar state cache. The displayed avatar is a derived view over
// (this map, activeTab) — switching tabs immediately re-renders without an
// extra backend round-trip.
const perTabState = writable<Record<TabId, AvatarState>>(defaultRecord<AvatarState>('Idle'));

/// Per-tab AwaitingPermission flag. Driven by backend permission detection.
/// Exposed for the TabBar's per-tab indicator rendering. Always false for
/// Shell tabs (the detector never runs for them).
export const perTabAwaitingPermission = writable<Record<TabId, boolean>>(
  defaultRecord<boolean>(false),
);

/// Per-tab DoneWhileAway flag. Set by the backend when a tab transitions to
/// Idle while inactive; cleared on tab activation.
export const perTabDoneWhileAway = writable<Record<TabId, boolean>>(
  defaultRecord<boolean>(false),
);

/// Per-tab Shell-only closed state. Driven by the backend's
/// `tab-closed-state-changed` event; the ClosedShellOverlay component
/// subscribes to render the "Shell exited (code N)" message.
export const perTabClosedState = writable<Record<TabId, TabClosedState>>(
  defaultRecord<TabClosedState>({ closed: false, exit_code: null }),
);

/// Per-tab avatar state map exposed for TabBar (it needs all tabs at once,
/// not just the active one).
export const perTabAvatarState: Readable<Record<TabId, AvatarState>> = derived(
  perTabState,
  (s) => s,
);

/// Active tab's avatar state. Components that show the avatar subscribe to
/// this; it recomputes whenever either the per-tab cache or the active tab
/// changes.
export const avatarState: Readable<AvatarState> = derived(
  [perTabState, activeTab],
  ([s, t]) => s[t],
);

/// Per-tab error info. Like `avatarState`, the displayed banner is the
/// active tab's error.
const perTabError = writable<Record<TabId, AvatarErrorInfo | null>>(
  defaultRecord<AvatarErrorInfo | null>(null),
);

export const avatarError: Readable<AvatarErrorInfo | null> = derived(
  [perTabError, activeTab],
  ([s, t]) => s[t],
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

let unlistenStatePromise: Promise<UnlistenFn> | null = null;
let unlistenErrorPromise: Promise<UnlistenFn> | null = null;

/// Subscribe to backend state broadcasts. Idempotent.
export function startAvatarStateListener(): Promise<UnlistenFn> {
  if (!unlistenStatePromise) {
    unlistenStatePromise = listen<StateEvent>('avatar-state', (event) => {
      const e = event.payload;
      if (e.type === 'state-changed') {
        perTabState.update((m) => ({ ...m, [e.tab]: e.state }));
        if (e.state !== 'Error') {
          perTabError.update((m) => ({ ...m, [e.tab]: null }));
        }
      } else if (e.type === 'active-tab-changed') {
        activeTab.set(e.tab);
      } else if (e.type === 'awaiting-permission-changed') {
        perTabAwaitingPermission.update((m) => ({ ...m, [e.tab]: e.awaiting }));
      } else if (e.type === 'done-while-away-changed') {
        perTabDoneWhileAway.update((m) => ({ ...m, [e.tab]: e.done }));
      } else if (e.type === 'tab-closed-state-changed') {
        perTabClosedState.update((m) => ({
          ...m,
          [e.tab]: { closed: e.closed, exit_code: e.exit_code },
        }));
      }
    });
  }
  if (!unlistenErrorPromise) {
    unlistenErrorPromise = listen<AvatarErrorInfo>('avatar-error', (event) => {
      const info = event.payload;
      perTabError.update((m) => ({ ...m, [info.tab]: info }));
    });
  }
  return unlistenStatePromise;
}
