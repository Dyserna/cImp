import { describe, expect, test } from 'vitest';

import { defaultSettings } from './types';
import type { AuditToolConfig, AuditToolId, Settings } from './types';
import {
  AUDIT_TOOL_META,
  auditToolGroups,
  auditToolRows,
  formatDetect,
  toolMatchesGlobal,
  toolNotApplicable,
} from './codeAudit';

function tool(id: AuditToolId, over: Partial<AuditToolConfig> = {}): AuditToolConfig {
  return { id, enabled: true, path: '', extra_args: [], ruleset: '', timeout_secs: null, ...over };
}

function settingsWith(tools: AuditToolConfig[]): Settings {
  const s = defaultSettings();
  s.code_audit = {
    enabled: false,
    tools,
    timeout_secs: 600,
    quality_auto_select: true,
    expose_claude: true,
    expose_opencode: true,
    expose_offload: true,
  };
  return s;
}

describe('auditToolRows', () => {
  test('default settings render one row per v1 tool, in display order', () => {
    const rows = auditToolRows(defaultSettings());
    // V25: the Security trio + the ten Quality tools, in AUDIT_TOOL_META order.
    expect(rows.map((r) => r.meta.id)).toEqual([
      'osv-scanner',
      'gitleaks',
      'semgrep',
      'oxlint',
      'golangci-lint',
      'ruff',
      'cppcheck',
      'typos',
      'eslint',
      'pmd',
      'knip',
      'cargo-machete',
      'dotnet-analyzers',
      'semgrep-quality',
    ]);
    // Each row carries its live config entry and the index into the array.
    for (const r of rows) {
      expect(r.tool.id).toBe(r.meta.id);
      expect(r.index).toBeGreaterThanOrEqual(0);
    }
  });

  test('a tool absent from settings is skipped', () => {
    const rows = auditToolRows(settingsWith([tool('gitleaks'), tool('semgrep')]));
    expect(rows.map((r) => r.meta.id)).toEqual(['gitleaks', 'semgrep']);
  });

  test('rows follow metadata order regardless of stored order', () => {
    const rows = auditToolRows(settingsWith([tool('semgrep'), tool('osv-scanner')]));
    expect(rows.map((r) => r.meta.id)).toEqual(['osv-scanner', 'semgrep']);
    // Index points at the real position in the stored (unsorted) array.
    expect(rows.find((r) => r.meta.id === 'semgrep')?.index).toBe(0);
    expect(rows.find((r) => r.meta.id === 'osv-scanner')?.index).toBe(1);
  });

  test('an unknown backend-kept id never produces a row', () => {
    const stored = [tool('gitleaks'), { ...tool('gitleaks'), id: 'guarddog' as AuditToolId }];
    const rows = auditToolRows(settingsWith(stored));
    expect(rows.map((r) => r.meta.id)).toEqual(['gitleaks']);
  });

  test('every meta role string is non-empty', () => {
    for (const m of AUDIT_TOOL_META) expect(m.role.length).toBeGreaterThan(0);
  });
});

describe('MCP exposure defaults (V26)', () => {
  test('all three expose toggles default true', () => {
    const ca = defaultSettings().code_audit;
    expect(ca.expose_claude).toBe(true);
    expect(ca.expose_opencode).toBe(true);
    expect(ca.expose_offload).toBe(true);
  });
});

describe('auditToolGroups (V25 Security / Quality split)', () => {
  test('default settings split into the Security trio and the ten Quality tools', () => {
    const g = auditToolGroups(defaultSettings());
    expect(g.security.map((r) => r.meta.id)).toEqual(['osv-scanner', 'gitleaks', 'semgrep']);
    expect(g.quality.map((r) => r.meta.id)).toEqual([
      'oxlint',
      'golangci-lint',
      'ruff',
      'cppcheck',
      'typos',
      'eslint',
      'pmd',
      'knip',
      'cargo-machete',
      'dotnet-analyzers',
      'semgrep-quality',
    ]);
    // Rows keep their real index into the stored tools array (for bind: paths).
    for (const r of [...g.security, ...g.quality]) expect(r.tool.id).toBe(r.meta.id);
  });

  test('a project subset only groups the tools it configures', () => {
    const g = auditToolGroups(settingsWith([tool('gitleaks'), tool('oxlint'), tool('typos')]));
    expect(g.security.map((r) => r.meta.id)).toEqual(['gitleaks']);
    expect(g.quality.map((r) => r.meta.id)).toEqual(['oxlint', 'typos']);
  });
});

describe('toolMatchesGlobal (per-tool scope badge)', () => {
  const globalTools = [
    tool('gitleaks'),
    tool('oxlint', { enabled: false, extra_args: ['--fix'], timeout_secs: 90 }),
  ];

  test('matching enabled/extra_args/timeout_secs → global', () => {
    expect(toolMatchesGlobal(tool('gitleaks'), globalTools)).toBe(true);
    expect(
      toolMatchesGlobal(
        tool('oxlint', { enabled: false, extra_args: ['--fix'], timeout_secs: 90 }),
        globalTools,
      ),
    ).toBe(true);
  });

  test('any project-scoped field difference → local', () => {
    expect(toolMatchesGlobal(tool('gitleaks', { enabled: false }), globalTools)).toBe(false);
    expect(toolMatchesGlobal(tool('gitleaks', { extra_args: ['-v'] }), globalTools)).toBe(false);
    expect(toolMatchesGlobal(tool('gitleaks', { timeout_secs: 30 }), globalTools)).toBe(false);
    expect(toolMatchesGlobal(tool('gitleaks', { ruleset: 'p/default' }), globalTools)).toBe(false);
  });

  test('a missing ruleset field (pre-upgrade global snapshot) equals empty', () => {
    const legacy = [tool('gitleaks')] as AuditToolConfig[];
    delete (legacy[0] as Partial<AuditToolConfig>).ruleset;
    expect(toolMatchesGlobal(tool('gitleaks'), legacy)).toBe(true);
  });

  test('path differences never make a tool local (machine-scope)', () => {
    expect(
      toolMatchesGlobal(tool('gitleaks', { path: 'C:\\ebin\\gitleaks.exe' }), globalTools),
    ).toBe(true);
  });

  test('a tool the global file does not know is local', () => {
    expect(toolMatchesGlobal(tool('ruff'), globalTools)).toBe(false);
  });
});

describe('toolNotApplicable (settings census hint)', () => {
  test('empty census (no scan yet) → no hint for anything', () => {
    const empty = { extensions: [], markers: [] };
    expect(toolNotApplicable('pmd', empty)).toBe(false);
    expect(toolNotApplicable('ruff', empty)).toBe(false);
  });

  test('a known census flags the tools the project gates off, not the applicable ones', () => {
    const rustJs = { extensions: ['rs', 'ts'], markers: ['Cargo.toml', 'package.json'] };
    expect(toolNotApplicable('pmd', rustJs)).toBe(true); // no .java
    expect(toolNotApplicable('ruff', rustJs)).toBe(true); // no .py
    expect(toolNotApplicable('oxlint', rustJs)).toBe(false); // ts present
    expect(toolNotApplicable('typos', rustJs)).toBe(false); // always applicable
    expect(toolNotApplicable('osv-scanner', rustJs)).toBe(false); // security, always
  });
});

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
