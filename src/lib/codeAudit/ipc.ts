// V23 Phase C: IPC wrappers for the Code Audit runner (Phase B backend). The
// detect probe (`audit_detect_tool`) lives with the settings IPC
// (`../settings/ipc`) since the Settings section owns it.

import { invoke } from '@tauri-apps/api/core';
import type { AuditSnapshot } from './types';

/// Start a scan of the project root with all enabled + resolvable tools,
/// concurrently. Returns immediately; progress streams via the `audit-status`
/// event. Rejects (typed error string the UI surfaces) when a scan is already
/// in flight or no tool is enabled.
export async function auditStartScan(): Promise<void> {
  await invoke('audit_start_scan');
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
