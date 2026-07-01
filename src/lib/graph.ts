// V9-01 code knowledge graph — frontend IPC wrappers for the Settings panel.
// The backend `GraphService` owns the on-disk index; these drive a rebuild and
// read its status. The live `graph-status` event (per-root build transitions)
// is consumed by the Phase-I monitor tab, not here.

import { invoke } from '@tauri-apps/api/core';
import { listen, type UnlistenFn } from '@tauri-apps/api/event';

/// Mirror of Rust `graph::index::LangCount` — indexed file count for one
/// language. Only languages with at least one file are present.
export interface LangCount {
  lang: string;
  files: number;
}

/// Mirror of Rust `graph::GraphStatus`.
export interface GraphStatus {
  root: string;
  state: 'idle' | 'building' | 'ready' | 'error';
  building: boolean;
  files_indexed: number;
  files: number;
  symbols: number;
  edges: number;
  // Indexed files grouped by language, biggest first.
  langs: LangCount[];
  last_error: string | null;
  // Whether file-watch re-indexing is currently paused (global toggle).
  watch_paused: boolean;
  // Phase G semantic-search status.
  semantic_enabled: boolean;
  embedder_configured: boolean;
  embedder_ready: boolean;
  embed_state: 'off' | 'idle' | 'embedding' | 'degraded' | 'error';
  embedded: number;
  embed_total: number;
  embed_pending: number;
  embed_error: string | null;
}

/// Known per-root graph statuses (empty before the first build).
export function graphStatus(): Promise<GraphStatus[]> {
  return invoke<GraphStatus[]>('graph_status');
}

/// Trigger a full rebuild of the project's graph. `root` defaults (backend
/// side) to the app's launch directory. Returns immediately; progress lands on
/// the `graph-status` event.
export function graphRebuild(root?: string): Promise<void> {
  return invoke<void>('graph_rebuild', { root: root ?? null });
}

/// Force a full re-embed of the project's doc chunks (Phase G). No-op when
/// semantic search is off.
export function graphRebuildEmbeddings(root?: string): Promise<void> {
  return invoke<void>('graph_rebuild_embeddings', { root: root ?? null });
}

/// Pause/resume the incremental fs-watcher re-indexing. Returns the new state.
export function graphSetWatchPaused(paused: boolean): Promise<boolean> {
  return invoke<boolean>('graph_set_watch_paused', { paused });
}

/// Result of an on-demand embedder reachability probe. Mirror of Rust
/// `graph::EmbedderProbe`.
export interface EmbedderProbe {
  ok: boolean;
  dim: number | null;
  message: string;
}

/// Probe the configured embedding endpoint without running a backfill — drives
/// the monitor tab's "Test connection" action. Always resolves; `ok` is false
/// (with `message` set) when the endpoint is unreachable or misconfigured.
export function graphTestEmbedder(): Promise<EmbedderProbe> {
  return invoke<EmbedderProbe>('graph_test_embedder');
}

/// One recorded graph tool call. Mirror of Rust `graph::GraphCall`.
export interface GraphCall {
  ts_ms: number;
  source: 'claude' | 'offload';
  tool: string;
  target: string;
  chars: number;
  ms: number;
  ok: boolean;
}

/// Recent graph tool calls (cloud Claude + offload worker), newest first.
export function graphHistory(): Promise<GraphCall[]> {
  return invoke<GraphCall[]>('graph_history');
}

/// Subscribe to live per-root status transitions (emitted as one status at a
/// time). Returns an unlisten fn.
export function onGraphStatus(cb: (status: GraphStatus) => void): Promise<UnlistenFn> {
  return listen<GraphStatus>('graph-status', (e) => cb(e.payload));
}
