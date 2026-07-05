// V8-01 local task offload — frontend IPC wrappers + live status store.
// The backend supervisor owns the `llama-server` child; these drive its
// lifecycle from the Settings UI and mirror its `offload-state` events.

import { writable, type Writable } from 'svelte/store';
import { invoke } from '@tauri-apps/api/core';
import { listen, type UnlistenFn } from '@tauri-apps/api/event';
import type { OpencodeLocalProvider } from './settings/types';

/// Mirror of Rust `OffloadState` (serde tag = "state").
export type OffloadState =
  | { state: 'disabled' }
  | { state: 'stopped' }
  | { state: 'starting' }
  | { state: 'ready'; n_ctx: number | null; slots: number; in_flight: number }
  | { state: 'error'; message: string };

export const offloadState: Writable<OffloadState> = writable({ state: 'disabled' });

let initialized = false;
let initInFlight: Promise<void> | null = null;

/// Fetch the current status and subscribe to live `offload-state` events.
/// Idempotent; safe to call on Settings mount.
export async function initOffloadStatus(): Promise<void> {
  if (initialized) return;
  // Dedupe concurrent callers (Settings mount + app mount) so we don't
  // subscribe twice while the first attempt is still in flight.
  if (initInFlight) return initInFlight;
  initInFlight = (async () => {
    // A failed status fetch is non-fatal — we still want the live listener.
    try {
      offloadState.set(await offloadStatus());
    } catch (e) {
      console.warn('offload_status failed', e);
    }
    await listen<OffloadState>('offload-state', (event) => {
      offloadState.set(event.payload);
    });
    // Mark initialized only after the subscription is established; if `listen`
    // throws, the flag stays false and the next call retries.
    initialized = true;
  })();
  try {
    await initInFlight;
  } catch (e) {
    console.warn('offload-state subscribe failed; will retry', e);
  } finally {
    initInFlight = null;
  }
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
  /// Failure reason when `state === 'error'` (e.g. a non-llama.cpp server on
  /// the configured port). `null` otherwise.
  error: string | null;
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
      return s.error ? `Error — ${s.error}` : 'Error';
    default:
      return s.state;
  }
}

/// Run a canned (or custom) offload task and return the synthesized answer.
export async function offloadTest(instructions: string): Promise<string> {
  return invoke('offload_test', { instructions });
}

/// V21: derive the OpenCode `local-llama` provider from a Local backend's
/// server command (the Settings "Add to OpenCode" button). Rejects with a
/// message naming the missing `--port`/model flag when the command is
/// incomplete; the caller persists the resolved snapshot via `settings_update`.
export async function offloadDeriveOpencodeProvider(
  serverCommand: string,
): Promise<OpencodeLocalProvider> {
  return invoke('offload_derive_opencode_provider', { serverCommand });
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
  /// Tasks waiting for a slot right now (app-wide queue depth).
  queue_depth: number;
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

/// Reconcile the warm MCP host against the just-saved settings and return the
/// fresh status. Called by the Settings MCP editor right after persisting an
/// add/remove/enable/disable so the server connects or drops live — no
/// restart. Returns `null` if the service isn't reachable.
export async function offloadReloadMcp(): Promise<ServiceStatus | null> {
  try {
    return await invoke<ServiceStatus>('offload_reload_mcp');
  } catch (e) {
    console.warn('offload_reload_mcp failed', e);
    return null;
  }
}

/// V8-03: a captured `llama-server` output line (mirror of Rust `ServerLogLine`).
export interface ServerLogLine {
  backend: string;
  line: string;
}

/// Buffered server output (model-load progress + logs) for a backend, or the
/// primary backend when `name` is omitted. The read-only log panel's initial
/// fill; subscribe to `onOffloadServerOutput` for live lines.
export async function offloadServerLog(name?: string): Promise<string[]> {
  return invoke('offload_server_log', { name: name ?? null });
}

/// Subscribe to live `llama-server` output lines. Returns an unlisten fn.
export function onOffloadServerOutput(
  cb: (line: ServerLogLine) => void,
): Promise<UnlistenFn> {
  return listen<ServerLogLine>('offload-server-output', (e) => cb(e.payload));
}

/// V8-03: Offload Server dashboard snapshot (mirror of Rust `ServerMetrics`).
export interface SlotMetric {
  id: number;
  processing: boolean;
  /// Prompt (input) tokens. Total context in use = n_prompt + n_decoded.
  n_prompt: number;
  n_decoded: number;
  n_ctx: number;
  tps: number | null;
}
export interface RequestRecord {
  slot: number;
  start_ms: number;
  end_ms: number;
  duration_s: number;
  /// Prompt (input) tokens. Total tokens = prompt_tokens + tokens.
  prompt_tokens: number;
  /// Generated (output) tokens.
  tokens: number;
  avg_tps: number;
}
/// One LLM call within an offload run (mirror of Rust `CallRecord`).
export interface CallRecord {
  step: number;
  /// 'planning' | 'ingestion' | 'final'.
  kind: string;
  thinking: boolean;
  prompt_tokens: number;
  output_tokens: number;
  duration_ms: number;
  tps: number;
  /// 'tool_calls(N)' | 'answer' | 'empty' | 'leaked' | 'error'.
  result: string;
}
/// One offload run grouping its LLM calls (mirror of Rust `RunRecord`).
export interface RunRecord {
  id: number;
  instructions: string;
  /// Initial thinking mode: 'on' | 'off' | 'auto'.
  thinking: string;
  started_ms: number;
  /// 0 while still running.
  ended_ms: number;
  /// 'running' | 'success' | 'recovered' | 'failed'.
  outcome: string;
  calls: CallRecord[];
}
export interface ServerMetrics {
  running: boolean;
  total_slots: number;
  n_ctx_per_slot: number | null;
  busy_slots: number;
  slots: SlotMetric[];
  kv_cache_pct: number | null;
  predicted_tps: number | null;
  prompt_tps: number | null;
  requests_deferred: number | null;
  aggregate_tps: number;
  global_in_flight: number;
  global_cap: number;
  /// App-wide tasks waiting for a slot right now (stamped by the poller).
  queue_depth: number;
  metrics_available: boolean;
  history: RequestRecord[];
  /// Offload runs (one per offload_task), newest first, each grouping calls.
  runs: RunRecord[];
}

/// One backend's dashboard card (mirror of Rust `BackendDashboard`). `kind`
/// drives the Local-vs-Remote grouping; `state` is the coarse lifecycle that
/// decides whether the live dashboard or a status line renders.
export interface BackendDashboard {
  name: string;
  kind: 'local' | 'lan' | 'cloud';
  state: 'ready' | 'stopped' | 'starting' | 'unreachable' | 'blocked' | 'disabled';
  metrics: ServerMetrics;
}

/// Latest dashboard snapshot — one row per enabled backend. Empty before the
/// first poll (or when offload is disabled).
export async function offloadServerMetrics(): Promise<BackendDashboard[]> {
  try {
    return await invoke<BackendDashboard[]>('offload_server_metrics');
  } catch (e) {
    console.warn('offload_server_metrics failed', e);
    return [];
  }
}

/// Subscribe to live dashboard snapshots (one row per backend). Returns an
/// unlisten fn.
export function onOffloadServerMetrics(
  cb: (rows: BackendDashboard[]) => void,
): Promise<UnlistenFn> {
  return listen<BackendDashboard[]>('offload-server-metrics', (e) => cb(e.payload));
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
