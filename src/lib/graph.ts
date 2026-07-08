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
  // V11 Phase G/F: code-embedding coverage + cached-digest count.
  code_embedded: number;
  code_embed_total: number;
  digests: number;
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

/// Mirror of Rust `graph::LangCensus` — one language present in the project.
/// `supported && enabled` = green (indexed); `supported && !enabled` = yellow
/// (indexable but off); `!supported` = red (known-unsupported language, or the
/// `"other"` catch-all bucket).
export interface LangCensus {
  key: string;
  label: string;
  files: number;
  supported: boolean;
  enabled: boolean;
}

/// The project's language census for the Code Graph tab's language buttons.
/// Walks the tree fresh, so call it on tab open and after a rebuild — not on a
/// tight poll. `root` defaults (backend side) to the launch directory.
export function graphLanguageCensus(root?: string): Promise<LangCensus[]> {
  return invoke<LangCensus[]>('graph_language_census', { root: root ?? null });
}

/// Add (`enabled=true`) or remove (`false`) a language from the graph's index
/// set, then kick a rebuild so it's indexed/embedded (or its rows dropped).
/// Only supported language tags are accepted. `root` defaults to the launch dir.
export function graphSetLanguageEnabled(
  lang: string,
  enabled: boolean,
  root?: string,
): Promise<void> {
  return invoke<void>('graph_set_language_enabled', { lang, enabled, root: root ?? null });
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
  source: 'claude' | 'opencode' | 'offload';
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

/// V10 (Analyses): one candidate dead export. Mirror of Rust `DeadExportRow`.
export interface DeadExportRow {
  name: string;
  kind: string;
  file: string;
  line: number;
  signature: string;
}

/// Candidate unused public symbols (public/exported defs with no reference and
/// no inbound call edge). Candidates only — dynamic dispatch, an external API,
/// macros, or reflection can produce false positives. `root` defaults to the
/// launch directory. On-demand (no polling).
export function graphDeadExports(root?: string): Promise<DeadExportRow[]> {
  return invoke<DeadExportRow[]>('graph_dead_exports', { root: root ?? null });
}

/// Import cycles between files — each entry is a loop of two or more files that
/// transitively import one another. `root` defaults to the launch directory.
export function graphCycles(root?: string): Promise<string[][]> {
  return invoke<string[][]>('graph_cycles', { root: root ?? null });
}

/// V10 (Memory): one file in a session's working set. Mirror of Rust
/// `graph::memory::WorkingSetEntry`.
export interface WorkingSetEntry {
  path: string;
  touches: number;
  last_kind: string;
  last_ms: number;
  top_symbols: string[];
}

/// A remembered note. Mirror of Rust `graph::memory::MemNote`.
export interface MemNote {
  note_id: string;
  session_id: string;
  text: string;
  ts_ms: number;
  pinned: boolean;
}

/// A session summary row. Mirror of Rust `graph::memory::SessionInfo`.
export interface SessionInfo {
  session_id: string;
  agent: string;
  started_ms: number;
  last_ms: number;
  events: number;
}

/// The project's full memory readout. Mirror of Rust
/// `graph::memory::MemorySnapshot`.
export interface MemorySnapshot {
  current_session: string | null;
  working_set: WorkingSetEntry[];
  notes: MemNote[];
  sessions: SessionInfo[];
}

/// The project's session/action memory. `root` defaults to the launch directory.
export function graphMemory(root?: string): Promise<MemorySnapshot> {
  return invoke<MemorySnapshot>('graph_memory', { root: root ?? null });
}

/// Clear one session's memory (`session` = its id) or the whole project's
/// (`session` omitted).
export function graphMemoryClear(session?: string, root?: string): Promise<void> {
  return invoke<void>('graph_memory_clear', { root: root ?? null, session: session ?? null });
}

/// Pin/unpin a note (pinned notes survive session eviction, show project-wide).
export function graphNoteSetPinned(noteId: string, pinned: boolean, root?: string): Promise<void> {
  return invoke<void>('graph_note_set_pinned', { root: root ?? null, noteId, pinned });
}

/// V10 (Context): the result of a context retrieval. Mirror of Rust
/// `graph::context::RetrieveResult`.
export interface RetrieveResult {
  context_md: string;
  files_used: string[];
  chars: number;
  tokens_est: number;
}

/// Preview what context injection WOULD prepend for `prompt` (bypasses the
/// injection toggle, so it works while injection is off). `root` defaults to the
/// launch directory.
export function graphContextPreview(prompt: string, root?: string): Promise<RetrieveResult> {
  return invoke<RetrieveResult>('graph_context_preview', { root: root ?? null, prompt });
}
