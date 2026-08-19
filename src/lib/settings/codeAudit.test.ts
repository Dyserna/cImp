// What survived of this suite after V38 Phase E.
//
// It used to cover `AUDIT_TOOL_META`, `auditToolRows`/`auditToolGroups`,
// `toolNotApplicable` and `toolMatchesGlobal` — all of it about the per-tool
// rows the Code Audit section rendered over `settings.code_audit.tools`. That
// array is gone: the fourteen built-in scanners are embedded plugin manifests,
// configured through the Tool Plugins pane like every other tool, and their
// display metadata is read from the shipped definition rather than restated in
// TypeScript. `toolPlugins.test.ts` covers the rows now.
//
// The Detect probe's display state is UI state rather than tool metadata, so it
// stayed — and so did its tests.

import { describe, expect, test } from 'vitest';

import { formatDetect } from './codeAudit';

describe('formatDetect', () => {
  test('idle before any probe', () => {
    expect(formatDetect(undefined)).toEqual({ kind: 'idle', text: '' });
  });

  test('probing', () => {
    expect(formatDetect('probing')).toEqual({ kind: 'probing', text: 'Detecting…' });
  });

  test('found renders version + path', () => {
    const d = formatDetect({
      found: true,
      version: 'osv-scanner v1.9.2',
      path: 'C:\\tools\\osv-scanner.exe',
      error: null,
    });
    expect(d.kind).toBe('found');
    expect(d.text).toBe('✓ osv-scanner v1.9.2 — C:\\tools\\osv-scanner.exe');
  });

  test('found with no version falls back to "found"', () => {
    const d = formatDetect({ found: true, version: null, path: '/usr/bin/gitleaks', error: null });
    expect(d.text).toBe('✓ found — /usr/bin/gitleaks');
  });

  test('found with no path omits the dash', () => {
    const d = formatDetect({ found: true, version: 'v1', path: null, error: null });
    expect(d.text).toBe('✓ v1');
  });

  test('not-found surfaces the backend error', () => {
    const d = formatDetect({
      found: false,
      version: null,
      path: null,
      error: 'not found on PATH or ebin',
    });
    expect(d).toEqual({ kind: 'not-found', text: 'not found on PATH or ebin' });
  });

  test('not-found without an error string uses the default message', () => {
    const d = formatDetect({ found: false, version: null, path: null, error: null });
    expect(d).toEqual({ kind: 'not-found', text: 'not found on PATH or ebin' });
  });
});
