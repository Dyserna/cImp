// The unified, persistent tool-activity store (backend `crate::activity`) —
// invoke wrappers for the Tool Activity tab. Entries cover graph/context tool
// calls, completed offload_task runs, Code Audit tool runs, AND proxied MCP
// tool calls; they survive app restarts, and each keeps a (truncated) copy of
// the actual request/response for the detail popup.
import { invoke } from '@tauri-apps/api/core';

/// One activity, without payloads. Mirror of Rust `activity::ActivityEntry`.
export interface ActivityEntry {
  /// Stable id (unique across restarts) — delete/detail key on it.
  id: number;
  ts_ms: number;
  /// `graph` = a graph/context tool call; `offload` = an offload_task run;
  /// `audit` = one Code Audit tool run (V23); `mcp` = one proxied MCP tool
  /// call (`<server>__<tool>` through the warm host); `injection_flag` = one
  /// V32 injection-containment event (SSRF screen, external-fetch budget,
  /// canary hit, taint-latch refusal, a memory-quarantine hold, or a
  /// surface-only detection flag).
  kind: 'graph' | 'offload' | 'audit' | 'mcp' | 'injection_flag';
  /// Canonicalized project root the call ran against ('' when unknown).
  root: string;
  /// Agent (claude/opencode/offload/read_advisor/auto_check) for graph
  /// entries; the backend name for offload entries. For `injection_flag` rows
  /// it names the SCREEN that fired: `ssrf` / `budget` / `canary` /
  /// `latch_refusal` / `memory_quarantine` / `signature` / `classifier`, plus
  /// `unscreened` for a result delivered after the detection surface did LESS
  /// than a full pass over it (a truncated or skipped scan — "not flagged" is
  /// not "clean"), `updater` for the V32 C3 detection auto-updater (whose
  /// `tool` is the component and whose `ok` is the outcome — `rejected` is the
  /// only false), `latch_override` for a user-applied latch move,
  /// `latch_beacon` for a native-web beacon engaging one, and `contamination`
  /// for the moment a tab's conversation stopped being clean (one row per tab,
  /// naming the tool and page that did it — #48 finding F-3). Rendered as free
  /// text, so a source this build does not know still reads correctly: it just
  /// gets no accent colour. Every row's request payload carries an
  /// `origin` (`internal` / `ipc` / `http`) naming who asked — `ipc` is the only
  /// one that means a human acted (#45) — and a `session` naming the harness
  /// conversation, when the writer knew it.
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
