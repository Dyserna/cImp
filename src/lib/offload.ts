// V8-01 local task offload — frontend IPC wrappers + live status store.
// The backend supervisor owns the `llama-server` child; these drive its
// lifecycle from the Settings UI and mirror its `offload-state` events.

import { writable, type Writable } from 'svelte/store';
import { invoke } from '@tauri-apps/api/core';
import { listen } from '@tauri-apps/api/event';

/// Mirror of Rust `OffloadState` (serde tag = "state").
export type OffloadState =
  | { state: 'disabled' }
  | { state: 'stopped' }
  | { state: 'starting' }
  | { state: 'ready'; n_ctx: number | null; slots: number; in_flight: number }
  | { state: 'error'; message: string };

export const offloadState: Writable<OffloadState> = writable({ state: 'disabled' });

let initialized = false;

/// Fetch the current status and subscribe to live `offload-state` events.
/// Idempotent; safe to call on Settings mount.
export async function initOffloadStatus(): Promise<void> {
  if (initialized) return;
  initialized = true;
  try {
    offloadState.set(await offloadStatus());
  } catch (e) {
    console.warn('offload_status failed', e);
  }
  await listen<OffloadState>('offload-state', (event) => {
    offloadState.set(event.payload);
  });
}

export async function offloadStatus(): Promise<OffloadState> {
  return invoke('offload_status');
}

export async function offloadServerStart(): Promise<void> {
  await invoke('offload_server_start');
}

export async function offloadServerStop(): Promise<void> {
  await invoke('offload_server_stop');
}

export async function offloadServerRestart(): Promise<void> {
  await invoke('offload_server_restart');
}

/// Run a canned (or custom) offload task and return the synthesized answer.
export async function offloadTest(instructions: string): Promise<string> {
  return invoke('offload_test', { instructions });
}

/// One-line human-readable summary of a status for the Settings readout.
export function describeOffloadState(s: OffloadState): string {
  switch (s.state) {
    case 'disabled':
      return 'Disabled';
    case 'stopped':
      return 'Stopped (starts on first offload, or click Start)';
    case 'starting':
      return 'Starting — loading model…';
    case 'ready': {
      const ctx = s.n_ctx ? `${s.n_ctx.toLocaleString()} ctx` : 'ctx unknown';
      return `Ready — ${ctx}, ${s.in_flight}/${s.slots} slots in use`;
    }
    case 'error':
      return `Error — ${s.message}`;
  }
}
