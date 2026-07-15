// V23 Phase A: extractable logic for the Code Audit settings section. The
// Svelte component (`SettingsApp.svelte`) is not unit-tested in this repo, so
// the row-selection and Detect-result formatting live here where Vitest can
// exercise them, mirroring the `checksEditor.ts` split.

import type { AuditDetectResult, AuditToolConfig, AuditToolId, Settings } from './types';

/// Static per-tool presentation for the Code Audit section: display name +
/// one-line role text, in display order. The `role` strings are the spec's
/// (Phase A) one-liners.
export const AUDIT_TOOL_META: { id: AuditToolId; name: string; role: string }[] = [
  {
    id: 'osv-scanner',
    name: 'osv-scanner',
    role: 'dependency vulnerabilities + known-malicious',
  },
  { id: 'gitleaks', name: 'gitleaks', role: 'secrets' },
  {
    id: 'semgrep',
    name: 'semgrep',
    role: 'SAST — requires Python, Windows support beta',
  },
];

/// One rendered per-tool row: the metadata, the matching config entry, and its
/// index into `settings.code_audit.tools` (for the component's `bind:` paths).
export interface AuditToolRow {
  meta: (typeof AUDIT_TOOL_META)[number];
  tool: AuditToolConfig;
  index: number;
}

/// The per-tool rows to render, in metadata (display) order. A tool with no
/// matching config entry is skipped, and any unknown id the backend kept in the
/// list is ignored (it never matches a `meta`). This is the same selection the
/// section's `{#each AUDIT_TOOL_META}` + `findIndex` performs.
export function auditToolRows(settings: Settings): AuditToolRow[] {
  const rows: AuditToolRow[] = [];
  for (const meta of AUDIT_TOOL_META) {
    const index = settings.code_audit.tools.findIndex((t) => t.id === meta.id);
    if (index >= 0) rows.push({ meta, tool: settings.code_audit.tools[index], index });
  }
  return rows;
}

/// The per-tool Detect probe state the section tracks: `undefined` before any
/// probe, `'probing'` while the IPC is in flight, or a result.
export type AuditDetectState = AuditDetectResult | 'probing' | undefined;

/// What the inline Detect label shows, decoupled from styling.
export interface DetectDisplay {
  kind: 'idle' | 'probing' | 'found' | 'not-found';
  text: string;
}

/// Map a Detect state to its inline label. Display-only — mirrors the spec's
/// `✓ v2.4.0 — C:\...\osv-scanner.exe` (found) and `not found on PATH or ebin`
/// (not-found) forms. Never mutates the stored path.
export function formatDetect(state: AuditDetectState): DetectDisplay {
  if (state === undefined) return { kind: 'idle', text: '' };
  if (state === 'probing') return { kind: 'probing', text: 'Detecting…' };
  if (state.found) {
    const version = state.version ?? 'found';
    const path = state.path ?? '';
    return { kind: 'found', text: path ? `✓ ${version} — ${path}` : `✓ ${version}` };
  }
  return { kind: 'not-found', text: state.error ?? 'not found on PATH or ebin' };
}
