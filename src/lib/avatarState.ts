import { writable, derived, get, type Readable } from 'svelte/store';
import { listen, type UnlistenFn } from '@tauri-apps/api/event';
import { settings, applySettings } from './settings/store';
import { activeTab } from './tabs/state';
import type { TabId } from './tabs/types';

export type AvatarState = 'Idle' | 'Listening' | 'Thinking' | 'Speaking' | 'Error';

export type AvatarErrorKind = 'subprocess-exited' | 'tts-error' | 'audio-error';

export interface AvatarErrorInfo {
  tab: TabId;
  kind: AvatarErrorKind;
  message: string;
}

// Backend wire format for the `avatar-state` event. Two shapes:
// - StateChanged: { type: 'state-changed', tab, state }
// - ActiveTabChanged: { type: 'active-tab-changed', tab }
type StateEvent =
  | { type: 'state-changed'; tab: TabId; state: AvatarState }
  | { type: 'active-tab-changed'; tab: TabId };

// Per-tab avatar state cache. The displayed avatar is a derived view over
// (this map, activeTab) — switching tabs immediately re-renders without an
// extra backend round-trip.
const perTabState = writable<Record<TabId, AvatarState>>({
  claude: 'Idle',
  aider: 'Idle',
});

/// Active tab's avatar state. Components that show the avatar subscribe to
/// this; it recomputes whenever either the per-tab cache or the active tab
/// changes.
export const avatarState: Readable<AvatarState> = derived(
  [perTabState, activeTab],
  ([s, t]) => s[t],
);

/// Per-tab error info. Like `avatarState`, the displayed banner is the
/// active tab's error.
const perTabError = writable<Record<TabId, AvatarErrorInfo | null>>({
  claude: null,
  aider: null,
});

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
