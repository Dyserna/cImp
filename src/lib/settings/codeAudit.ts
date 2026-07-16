// V23 Phase A: extractable logic for the Code Audit settings section. The
// Svelte component (`SettingsApp.svelte`) is not unit-tested in this repo, so
// the row-selection and Detect-result formatting live here where Vitest can
// exercise them, mirroring the `checksEditor.ts` split.

import type { AuditCategory, AuditCensus } from '../codeAudit/types';
import { AUDIT_TOOL_CATEGORY, censusIsEmpty, isToolApplicable } from '../codeAudit/logic';
import type { AuditDetectResult, AuditToolConfig, AuditToolId, Settings } from './types';

/// Static per-tool presentation for the Code Audit section: display name +
/// one-line role text, in display order (Security trio first, then the V25
/// Quality tools). The `role` strings are the spec's one-liners.
export const AUDIT_TOOL_META: { id: AuditToolId; name: string; role: string }[] = [
  // Security (V23).
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
  // Quality (V25) — language-gated linters / dead-code / spell-check.
  { id: 'oxlint', name: 'oxlint', role: 'JS/TS linter — single Rust binary, zero-config' },
  { id: 'golangci-lint', name: 'golangci-lint', role: 'Go meta-linter' },
  { id: 'ruff', name: 'ruff', role: 'Python linter — single Rust binary' },
  { id: 'cppcheck', name: 'cppcheck', role: 'C/C++ static analysis' },
  { id: 'typos', name: 'typos', role: 'source spell-checker — applies to every project' },
  { id: 'eslint', name: 'eslint', role: 'JS/TS linter — uses the project-local config' },
  { id: 'pmd', name: 'PMD', role: 'Java static analysis — needs a JRE' },
  { id: 'knip', name: 'knip', role: 'unused files / exports / dependencies (Node)' },
  { id: 'cargo-machete', name: 'cargo-machete', role: 'unused Rust dependencies' },
  {
    id: 'dotnet-analyzers',
    name: 'Roslyn analyzers',
    role: 'runs a real .NET build (writes obj/bin) — default-disabled, longer timeout',
  },
  {
    id: 'semgrep-quality',
    name: 'semgrep (quality)',
    role: 'best-practices rulesets — default-disabled, needs network',
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

/// V25 Phase D: the per-tool rows split into the Security and Quality groups the
/// Settings section renders under separate headers. Non-applicable tools are NOT
/// hidden here (Settings is global config) — the section shows a census-based
/// hint instead (`toolNotApplicable`).
export interface AuditToolGroups {
  security: AuditToolRow[];
  quality: AuditToolRow[];
}

export function auditToolGroups(settings: Settings): AuditToolGroups {
  const security: AuditToolRow[] = [];
  const quality: AuditToolRow[] = [];
  for (const row of auditToolRows(settings)) {
    (AUDIT_TOOL_CATEGORY[row.meta.id] === 'quality' ? quality : security).push(row);
  }
  return { security, quality };
}

/// Whether the Settings section should show the "not applicable to the current
/// project" hint for `id`: the latest scan's census gates it off. An empty
/// census (before any scan) shows no hint — nothing is known to gate on yet.
export function toolNotApplicable(id: AuditToolId, census: AuditCensus): boolean {
  return !censusIsEmpty(census) && !isToolApplicable(id, census);
}

export type { AuditCategory };

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
