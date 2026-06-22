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

/// V8-02: per-backend status row (mirror of Rust `BackendStatus`).
export interface BackendStatus {
  name: string;
  kind: 'local' | 'lan' | 'cloud';
  tier: 'fast' | 'quality';
  enabled: boolean;
  cloud_blocked: boolean;
  state: 'disabled' | 'stopped' | 'starting' | 'ready' | 'unreachable' | 'blocked' | 'error';
  n_ctx: number | null;
  slots: number;
  in_flight: number;
  tool_scope: string;
}

/// Per-backend status for the whole pool (Local process+health and Remote
/// health-probe). Drives the backends editor's status rows.
export async function offloadStatuses(): Promise<BackendStatus[]> {
  return invoke('offload_statuses');
}

export async function offloadBackendStart(name: string): Promise<void> {
  await invoke('offload_backend_start', { name });
}

export async function offloadBackendStop(name: string): Promise<void> {
  await invoke('offload_backend_stop', { name });
}

export async function offloadBackendRestart(name: string): Promise<void> {
  await invoke('offload_backend_restart', { name });
}

/// One-line summary of a per-backend status for the editor row.
export function describeBackendStatus(s: BackendStatus): string {
  const ctx = s.n_ctx ? `${s.n_ctx.toLocaleString()} ctx` : 'ctx unknown';
  switch (s.state) {
    case 'ready':
      return `Ready — ${ctx}, ${s.in_flight}/${s.slots} slots`;
    case 'starting':
      return 'Starting…';
    case 'stopped':
      return 'Stopped';
    case 'unreachable':
      return 'Unreachable';
    case 'blocked':
      return 'Needs cloud consent';
    case 'disabled':
      return 'Disabled';
    case 'error':
      return 'Error';
    default:
      return s.state;
  }
}

/// Run a canned (or custom) offload task and return the synthesized answer.
export async function offloadTest(instructions: string): Promise<string> {
  return invoke('offload_test', { instructions });
}

/// V8-03: per-MCP-server health row (mirror of Rust `McpServerHealth`).
export interface McpServerHealth {
  name: string;
  transport: 'stdio' | 'http';
  connected: boolean;
  healthy: boolean;
  tool_count: number;
  error: string | null;
}

/// V8-03: aggregate offload-service status (mirror of Rust `ServiceStatus`).
/// `global_in_flight` is now honest — the long-lived app sees every offload
/// across all Claude tabs, so the warm-pool spill/fail-over works.
export interface ServiceStatus {
  global_in_flight: number;
  global_cap: number;
  mcp_servers: McpServerHealth[];
}

/// Fetch the warm-pool service status (global in-flight + per-MCP-server
/// health). Returns `null` when the service isn't reachable (offload off or
/// app mid-launch) so the caller can render a neutral state.
export async function offloadServiceStatus(): Promise<ServiceStatus | null> {
  try {
    return await invoke<ServiceStatus>('offload_service_status');
  } catch (e) {
    console.warn('offload_service_status failed', e);
    return null;
  }
}

/// One-line summary of an MCP-server health row for the Settings list.
export function describeMcpServerHealth(s: McpServerHealth): string {
  if (s.healthy) {
    return `Healthy — ${s.tool_count} tool${s.tool_count === 1 ? '' : 's'} (${s.transport})`;
  }
  if (s.connected) {
    return `Connected, no tools (${s.transport})`;
  }
  return s.error ? `Down — ${s.error}` : 'Down';
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
