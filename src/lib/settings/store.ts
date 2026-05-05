// Settings store: a single Svelte writable holding the full Settings object,
// kept in sync with the backend via the `settings-changed` Tauri event. The
// store is shared between the main window and the settings window — both
// listen on the same event, so a change made in either window is reflected
// in the other (and persisted to disk by the backend's debounced saver).

import { writable, derived, type Readable } from 'svelte/store';
import { listen, type UnlistenFn } from '@tauri-apps/api/event';
import { settingsGet, settingsUpdate } from './ipc';
import { defaultSettings, type Settings } from './types';

export const settings = writable<Settings>(defaultSettings());

let unlisten: UnlistenFn | null = null;
let initPromise: Promise<void> | null = null;

/// Initialize the store by fetching the current backend value and subscribing
/// to live updates. Idempotent: subsequent calls return the same promise.
export function initSettings(): Promise<void> {
  if (initPromise) return initPromise;
  initPromise = (async () => {
    try {
      const initial = await settingsGet();
      settings.set(initial);
    } catch (e) {
      console.warn('settings_get failed; using defaults', e);
    }
    unlisten = await listen<Settings>('settings-changed', (event) => {
      settings.set(event.payload);
    });
  })();
  return initPromise;
}

export function teardownSettings(): void {
  if (unlisten) {
    unlisten();
    unlisten = null;
  }
  initPromise = null;
}

/// Push a full updated Settings struct to the backend. The backend will
/// broadcast the change back to all windows (including this one), so callers
/// don't need to update the local store optimistically — the event-driven
/// path handles it. We DO update locally first for snappy UI, then let the
/// broadcast reconcile.
export async function applySettings(updated: Settings): Promise<void> {
  settings.set(updated);
  try {
    await settingsUpdate(updated);
  } catch (e) {
    console.error('settings_update failed', e);
  }
}

/// Convenience: derived stores per section so components can subscribe at
/// the granularity they actually need (avoiding unnecessary re-renders when
/// an unrelated section changes).
export const tts: Readable<Settings['tts']> = derived(settings, (s) => s.tts);
export const avatar: Readable<Settings['avatar']> = derived(settings, (s) => s.avatar);
export const waveform: Readable<Settings['avatar']['waveform']> = derived(
  settings,
  (s) => s.avatar.waveform,
);
export const display: Readable<Settings['display']> = derived(settings, (s) => s.display);
export const behavior: Readable<Settings['behavior']> = derived(settings, (s) => s.behavior);
export const compose: Readable<Settings['compose']> = derived(settings, (s) => s.compose);
export const shortcuts: Readable<Settings['shortcuts']> = derived(
  settings,
  (s) => s.shortcuts,
);
export const tabs: Readable<Settings['tabs']> = derived(settings, (s) => s.tabs);
export const processing: Readable<Settings['processing']> = derived(
  settings,
  (s) => s.processing,
);
