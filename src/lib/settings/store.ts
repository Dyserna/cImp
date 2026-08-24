// Settings store: a single Svelte writable holding the full Settings object,
// kept in sync with the backend via the `settings-changed` Tauri event. The
// store is shared between the main window and the settings window — both
// listen on the same event, so a change made in either window is reflected
// in the other (and persisted to disk by the backend's debounced saver).

import { writable, derived, get, type Readable } from 'svelte/store';
import { listenEvent, SETTINGS_CHANGED } from '../events';
import { settingsGet, settingsUpdate } from './ipc';
import { defaultSettings, type Settings } from './types';

export const settings = writable<Settings>(defaultSettings());

let initPromise: Promise<void> | null = null;

/// Initialize the store by fetching the current backend value and subscribing
/// to live updates. Idempotent: subsequent calls return the same promise.
/// The listener is process-scoped — there's no teardown path because the
/// settings store survives for the lifetime of the window.
export function initSettings(): Promise<void> {
  if (initPromise) return initPromise;
  initPromise = (async () => {
    // Register the listener BEFORE the initial get so a change broadcast that
    // lands while the get is in flight isn't missed. The `gotEvent` guard then
    // prevents the (possibly older) get snapshot from clobbering a newer event
    // value that already arrived.
    let gotEvent = false;
    await listenEvent(SETTINGS_CHANGED, (event) => {
      gotEvent = true;
      settings.set(event.payload);
    });
    try {
      const initial = await settingsGet();
      if (!gotEvent) settings.set(initial);
    } catch (e) {
      console.warn('settings_get failed; using defaults', e);
    }
  })();
  return initPromise;
}

/// Push a full updated Settings struct to the backend. The backend will
/// broadcast the change back to all windows (including this one), so callers
/// don't need to update the local store optimistically — the event-driven
/// path handles it. We DO update locally first for snappy UI, then let the
/// broadcast reconcile.
export async function applySettings(updated: Settings): Promise<void> {
  const previous = get(settings);
  settings.set(updated);
  try {
    await settingsUpdate(updated);
  } catch (e) {
    // Roll back the optimistic update — otherwise the UI shows a change the
    // backend rejected, and the divergence is silently lost on next restart.
    console.error('settings_update failed; rolling back optimistic update', e);
    settings.set(previous);
  }
}

/// Convenience: derived stores per section so components can subscribe at
/// the granularity they actually need (avoiding unnecessary re-renders when
/// an unrelated section changes).
export const tts: Readable<Settings['tts']> = derived(settings, (s) => s.tts);
export const stt: Readable<Settings['stt']> = derived(settings, (s) => s.stt);
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
export const offload: Readable<Settings['offload']> = derived(settings, (s) => s.offload);
export const processing: Readable<Settings['processing']> = derived(
  settings,
  (s) => s.processing,
);
export const terminal: Readable<Settings['terminal']> = derived(
  settings,
  (s) => s.terminal,
);
