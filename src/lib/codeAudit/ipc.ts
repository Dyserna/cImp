// V23 Phase C: IPC wrappers for the Code Audit runner (Phase B backend). The
// detect probe (`audit_detect_tool`) lives with the settings IPC
// (`../settings/ipc`) since the Settings section owns it.

import { invoke } from '@tauri-apps/api/core';
import type { AuditCategory, AuditSnapshot } from './types';

/// Start a scan of the project root with the enabled + applicable + resolvable
/// tools of `category` (`'security'` / `'quality'`), concurrently. Returns
/// immediately; progress streams via the `audit-status` event. Rejects (typed
/// error string the UI surfaces) when a scan is already in flight (one at a time
/// globally, either category) or no tool of `category` is enabled.
export async function auditStartScan(category: AuditCategory): Promise<void> {
  await invoke('audit_start_scan', { category });
}

/// Cancel the in-flight scan (kills the running tool children; already-completed
/// tools keep their findings). Errors when nothing is running.
export async function auditCancelScan(): Promise<void> {
  await invoke('audit_cancel_scan');
}

/// The full (uncapped) runner snapshot — read on mount and to fetch the complete
/// findings set after a `truncated` event.
export async function auditSnapshot(): Promise<AuditSnapshot> {
  return invoke('audit_snapshot');
}

/// Take (or reuse, ≤60s cache) the project's language census outside a scan,
/// apply quality auto-selection when it's on, and return the full snapshot —
/// so tab mount and the Settings section know applicability before the first
/// scan. Passthrough (no walk) while the feature is disabled or a scan runs.
export async function auditRefreshCensus(): Promise<AuditSnapshot> {
  return invoke('audit_refresh_census');
}
