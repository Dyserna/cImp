import { writable, derived, get, type Readable } from 'svelte/store';
import { listen, type UnlistenFn } from '@tauri-apps/api/event';
import { settings, applySettings } from './settings/store';

export type AvatarState = 'Idle' | 'Listening' | 'Thinking' | 'Speaking' | 'Error';

export const avatarState = writable<AvatarState>('Idle');

/// Visibility is now backed by `settings.avatar.visible`. The toggle button
/// and the settings UI are two views of the same source of truth — toggling
/// either updates the persisted setting, which broadcasts back to all
/// subscribers (including this derived store).
export const avatarVisible: Readable<boolean> = derived(
  settings,
  (s) => s.avatar.visible,
);

/// Flip the visibility flag. Used by the toggle button next to the avatar.
export function toggleAvatarVisible(): void {
  const s = get(settings);
  void applySettings({ ...s, avatar: { ...s.avatar, visible: !s.avatar.visible } });
}

let unlistenPromise: Promise<UnlistenFn> | null = null;

/// Subscribe the store to backend state broadcasts. Idempotent: a second
/// call returns the same teardown function the first call did, so any
/// component (including HMR-mounted ones) can call it safely.
export function startAvatarStateListener(): Promise<UnlistenFn> {
  if (!unlistenPromise) {
    unlistenPromise = listen<AvatarState>('avatar-state', (event) => {
      avatarState.set(event.payload);
    });
  }
  return unlistenPromise;
}
