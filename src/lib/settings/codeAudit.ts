// Extractable logic for the Code Audit settings section. The Svelte component
// (`SettingsApp.svelte`) is not unit-tested in this repo, so the bits that can
// be tested live here, mirroring the `checksEditor.ts` split.
//
// # What left in V38 Phase E
//
// Most of this file was per-tool metadata and per-tool row plumbing:
// `AUDIT_TOOL_META` (display name, one-line role, the built-in ruleset default
// for the three tools that have one), `auditToolRows`/`auditToolGroups` (rows
// over `settings.code_audit.tools`), `toolNotApplicable`, and
// `toolMatchesGlobal` (the per-tool global/local scope badge).
//
// All of it is gone because its subject is gone. The fourteen built-in scanners
// are embedded plugin manifests now, so:
//
// * the display name and role text ARE the manifest's `label` and
//   `description`, and the ruleset default is a declared variable's `default` —
//   read from the shipped definition rather than restated here, where the two
//   could disagree and only the user would find out;
// * their configuration lives in `tool_plugins` and is rendered by the Tool
//   Plugins pane, which already knows how to render a tool;
// * that container is machine scope by construction, so there is no
//   project-versus-global ambiguity left for a badge to indicate.
//
// What remains is the Detect probe's display state, which is UI state rather
// than tool metadata: the pane tracks a probe per tool key, and this decides
// what the inline label says.

import type { AuditDetectResult } from './types';

/// The per-tool Detect probe state the pane tracks: `undefined` before any
/// probe, `'probing'` while the IPC is in flight, or a result.
export type AuditDetectState = AuditDetectResult | 'probing' | undefined;

/// What the inline Detect label shows, decoupled from styling.
export interface DetectDisplay {
  kind: 'idle' | 'probing' | 'found' | 'not-found';
  text: string;
}

/// Map a Detect state to its inline label. Display-only — the
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
