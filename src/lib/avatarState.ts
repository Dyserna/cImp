import { writable } from 'svelte/store';
import { listen, type UnlistenFn } from '@tauri-apps/api/event';

export type AvatarState = 'Idle' | 'Listening' | 'Thinking' | 'Speaking' | 'Error';

export const avatarState = writable<AvatarState>('Idle');

// In-memory only for M4 — persistence lands in M6 along with the rest of
// the settings store.
export const avatarVisible = writable<boolean>(true);

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
