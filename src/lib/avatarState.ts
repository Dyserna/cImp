import { writable, derived, get, type Readable } from 'svelte/store';
import { listen, type UnlistenFn } from '@tauri-apps/api/event';
import { settings, applySettings } from './settings/store';

export type AvatarState = 'Idle' | 'Listening' | 'Thinking' | 'Speaking' | 'Error';

export type AvatarErrorKind =
  | 'subprocess-exited'
  | 'tts-error'
  | 'audio-error';

export interface AvatarErrorInfo {
  kind: AvatarErrorKind;
  message: string;
}

export const avatarState = writable<AvatarState>('Idle');
export const avatarError = writable<AvatarErrorInfo | null>(null);

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
let unlistenErrorPromise: Promise<UnlistenFn> | null = null;

/// Subscribe the store to backend state broadcasts. Idempotent: a second
/// call returns the same teardown function the first call did, so any
/// component (including HMR-mounted ones) can call it safely.
///
/// Also wires the companion `avatar-error` listener so the banner has the
/// kind+message context for the recovery action. The error info is cleared
/// automatically when state leaves Error.
export function startAvatarStateListener(): Promise<UnlistenFn> {
  if (!unlistenPromise) {
    unlistenPromise = listen<AvatarState>('avatar-state', (event) => {
      avatarState.set(event.payload);
      if (event.payload !== 'Error') avatarError.set(null);
    });
  }
  if (!unlistenErrorPromise) {
    unlistenErrorPromise = listen<AvatarErrorInfo>('avatar-error', (event) => {
      avatarError.set(event.payload);
    });
  }
  return unlistenPromise;
}
