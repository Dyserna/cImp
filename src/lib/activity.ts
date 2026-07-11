// The unified, persistent tool-activity store (backend `crate::activity`) —
// invoke wrappers for the Tool Activity tab. Entries cover graph/context tool
// calls AND completed offload_task runs, survive app restarts, and each keeps
// a (truncated) copy of the actual request/response for the detail popup.
import { invoke } from '@tauri-apps/api/core';

/// One activity, without payloads. Mirror of Rust `activity::ActivityEntry`.
export interface ActivityEntry {
  /// Stable id (unique across restarts) — delete/detail key on it.
  id: number;
  ts_ms: number;
  /// `graph` = a graph/context tool call; `offload` = an offload_task run.
  kind: 'graph' | 'offload';
  /// Canonicalized project root the call ran against ('' when unknown).
  root: string;
  /// Agent (claude/opencode/offload/read_advisor/auto_check) for graph
  /// entries; the backend name for offload entries.
  source: string;
  tool: string;
  target: string;
  chars: number;
  ms: number;
  ok: boolean;
}

/// The full record: entry + captured payloads. Mirror of Rust
/// `activity::ActivityRecord` (which flattens the entry).
export interface ActivityRecord extends ActivityEntry {
  request: string;
  response: string;
}

/// The feed (graph + offload), newest first, payload-free. Pass `sinceTs` to
/// fetch only entries newer than a high-water mark; omit it for the full
/// list (the Tool Activity tab polls the full list — it needs an
/// authoritative snapshot to reflect deletions).
export function activityList(sinceTs?: number): Promise<ActivityEntry[]> {
  return invoke<ActivityEntry[]>('activity_list', { sinceTs: sinceTs ?? null });
}

/// One activity's full record for the detail popup. Resolves null when the
/// entry vanished (deleted / aged out) between the list poll and the click.
export function activityDetail(id: number): Promise<ActivityRecord | null> {
  return invoke<ActivityRecord | null>('activity_detail', { id });
}

/// Delete one entry (persists immediately).
export function activityDelete(id: number): Promise<void> {
  return invoke<void>('activity_delete', { id });
}

/// Clear the whole history (persists immediately).
export function activityClear(): Promise<void> {
  return invoke<void>('activity_clear');
}
