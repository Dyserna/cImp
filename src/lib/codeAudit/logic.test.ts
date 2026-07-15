import { describe, expect, test } from 'vitest';

import type { CodeAuditSettings } from '../settings/types';
import type { AuditSnapshot, AuditToolState } from './types';
import {
  chipToolStates,
  configuredToolStates,
  defaultFilters,
  deselectAll,
  filterFindings,
  flattenFindings,
  formatCoverageLine,
  formatFindingsMarkdown,
  formatScanTimestamp,
  mergeAuditSnapshot,
  scannedArtifacts,
  selectAllVisible,
  selectedRows,
  sortFindings,
  toggleSelected,
  toolChip,
  visibleFindings,
} from './logic';

// ── Fixtures ─────────────────────────────────────────────────────────────────

function tool(overrides: Partial<AuditToolState> & Pick<AuditToolState, 'id'>): AuditToolState {
  return {
    status: 'done',
    findings: [],
    duration_ms: 0,
    error: null,
    resolved: null,
    scanned_artifacts: [],
    ...overrides,
  };
}

function snapshot(overrides: Partial<AuditSnapshot> = {}): AuditSnapshot {
  return {
    root: 'P:\\proj',
    scanning: false,
    last_scan_at: 1752600000000,
    tools: [],
    total_findings: 0,
    truncated: false,
    ...overrides,
  };
}

const OSV = tool({
  id: 'osv-scanner',
  status: 'done',
  duration_ms: 1234,
  resolved: 'C:\\ebin\\osv-scanner.exe',
  findings: [
    {
      tool: 'osv-scanner',
      diag: {
        severity: 'warning',
        code: 'GHSA-r8w9-5wcg-vfj7',
        message: 'tokio 1.38.0 vulnerable',
        file: 'Cargo.lock',
        line: 0,
        col: null,
      },
    },
  ],
});

const GITLEAKS = tool({
  id: 'gitleaks',
  status: 'done',
  duration_ms: 42,
  findings: [
    {
      tool: 'gitleaks',
      diag: {
        severity: 'error',
        code: 'generic-api-key',
        message: 'possible API key',
        file: 'src/lib/foo.ts',
        line: 42,
        col: 7,
      },
    },
  ],
});

const SEMGREP = tool({
  id: 'semgrep',
  status: 'done',
  duration_ms: 9000,
  findings: [
    {
      tool: 'semgrep',
      diag: {
        severity: 'note',
        code: 'js.audit.xss',
        message: 'possible xss',
        file: 'src/SettingsApp.svelte',
        line: 1291,
        col: null,
      },
    },
  ],
});

const FULL = snapshot({ tools: [OSV, GITLEAKS, SEMGREP], total_findings: 3 });

// ── flatten + sort + filter ──────────────────────────────────────────────────

describe('flattenFindings', () => {
  test('assigns stable per-tool ids', () => {
    const rows = flattenFindings(FULL);
    expect(rows.map((r) => r.id)).toEqual([
      'osv-scanner#0',
      'gitleaks#0',
      'semgrep#0',
    ]);
  });
});

describe('sortFindings', () => {
  test('severity desc, then tool order', () => {
    const rows = sortFindings(flattenFindings(FULL));
    expect(rows.map((r) => `${r.severity}/${r.tool}`)).toEqual([
      'error/gitleaks', // error first
      'warning/osv-scanner', // then warning
      'note/semgrep', // then note
    ]);
  });

  test('same severity breaks ties by tool order', () => {
    const s = snapshot({
      tools: [
        tool({
          id: 'gitleaks',
          findings: [
            { tool: 'gitleaks', diag: { severity: 'error', code: null, message: 'g', file: 'b.ts', line: 1, col: null } },
          ],
        }),
        tool({
          id: 'osv-scanner',
          findings: [
            { tool: 'osv-scanner', diag: { severity: 'error', code: null, message: 'o', file: 'a.lock', line: 0, col: null } },
          ],
        }),
      ],
    });
    // osv-scanner ranks before gitleaks even though it arrived second.
    expect(sortFindings(flattenFindings(s)).map((r) => r.tool)).toEqual([
      'osv-scanner',
      'gitleaks',
    ]);
  });
});

describe('filterFindings', () => {
  test('severity threshold hides lower severities', () => {
    const rows = flattenFindings(FULL);
    const warn = filterFindings(rows, { ...defaultFilters(), severity: 'warning' });
    expect(warn.map((r) => r.severity).sort()).toEqual(['error', 'warning']);
    const err = filterFindings(rows, { ...defaultFilters(), severity: 'error' });
    expect(err.map((r) => r.severity)).toEqual(['error']);
  });

  test('per-tool toggle (absent = visible, false = hidden)', () => {
    const rows = flattenFindings(FULL);
    const out = filterFindings(rows, { ...defaultFilters(), tools: { gitleaks: false } });
    expect(out.map((r) => r.tool)).toEqual(['osv-scanner', 'semgrep']);
  });

  test('text filter matches message / file / rule / tool, case-insensitively', () => {
    const rows = flattenFindings(FULL);
    expect(filterFindings(rows, { ...defaultFilters(), text: 'TOKIO' }).map((r) => r.tool)).toEqual([
      'osv-scanner',
    ]);
    expect(filterFindings(rows, { ...defaultFilters(), text: 'foo.ts' }).map((r) => r.tool)).toEqual([
      'gitleaks',
    ]);
    expect(filterFindings(rows, { ...defaultFilters(), text: 'GHSA' }).map((r) => r.tool)).toEqual([
      'osv-scanner',
    ]);
  });
});

// ── selection ────────────────────────────────────────────────────────────────

describe('selection', () => {
  test('select all selects the currently VISIBLE (filtered) set only', () => {
    const rows = flattenFindings(FULL);
    const filters = { ...defaultFilters(), severity: 'warning' as const }; // hides the note row
    const visible = visibleFindings(rows, filters);
    const sel = selectAllVisible(new Set(), visible);
    expect([...sel].sort()).toEqual(['gitleaks#0', 'osv-scanner#0']);
    // The filtered-out note row is not selected.
    expect(sel.has('semgrep#0')).toBe(false);
  });

  test('select all is additive over an existing selection', () => {
    const rows = flattenFindings(FULL);
    const visibleErrOnly = visibleFindings(rows, { ...defaultFilters(), severity: 'error' });
    // Pre-select the (currently filtered-out) note row, then select-all under
    // the error filter — the pre-selected row survives.
    const sel = selectAllVisible(new Set(['semgrep#0']), visibleErrOnly);
    expect([...sel].sort()).toEqual(['gitleaks#0', 'semgrep#0']);
  });

  test('deselect all clears; toggle flips one', () => {
    expect(deselectAll().size).toBe(0);
    const a = toggleSelected(new Set(), 'gitleaks#0');
    expect(a.has('gitleaks#0')).toBe(true);
    const b = toggleSelected(a, 'gitleaks#0');
    expect(b.has('gitleaks#0')).toBe(false);
  });

  test('selectedRows returns existing rows in order, dropping stale ids', () => {
    const rows = sortFindings(flattenFindings(FULL));
    const sel = new Set(['semgrep#0', 'gitleaks#0', 'ghost#9']);
    const picked = selectedRows(rows, sel);
    // In display (sorted) order: error/gitleaks before note/semgrep; ghost gone.
    expect(picked.map((r) => r.id)).toEqual(['gitleaks#0', 'semgrep#0']);
  });
});

// ── status chips ─────────────────────────────────────────────────────────────

describe('toolChip', () => {
  test('maps each status to its chip', () => {
    expect(toolChip(tool({ id: 'osv-scanner', status: 'idle' }))).toEqual({
      kind: 'idle',
      label: 'idle',
      spinner: false,
      tooltip: null,
    });
    expect(toolChip(tool({ id: 'osv-scanner', status: 'running' }))).toEqual({
      kind: 'running',
      label: 'running',
      spinner: true,
      tooltip: null,
    });
    expect(toolChip(OSV)).toEqual({ kind: 'done', label: '1 finding', spinner: false, tooltip: null });
    expect(toolChip(tool({ id: 'gitleaks', status: 'done', findings: [] })).label).toBe('0 findings');
    expect(toolChip(tool({ id: 'semgrep', status: 'failed', error: 'boom' }))).toEqual({
      kind: 'failed',
      label: 'error',
      spinner: false,
      tooltip: 'boom',
    });
    expect(toolChip(tool({ id: 'semgrep', status: 'not-installed', error: 'not found on PATH or ebin' }))).toEqual({
      kind: 'not-installed',
      label: 'not installed',
      spinner: false,
      tooltip: 'not found on PATH or ebin',
    });
  });
});

// ── configured pre-scan chips ────────────────────────────────────────────────

describe('configuredToolStates / chipToolStates', () => {
  const settings: CodeAuditSettings = {
    enabled: true,
    timeout_secs: 600,
    tools: [
      { id: 'semgrep', enabled: false, path: '', extra_args: [] },
      { id: 'osv-scanner', enabled: true, path: '', extra_args: [] },
      { id: 'gitleaks', enabled: true, path: '', extra_args: [] },
    ],
  };

  test('renders configured tools as idle, in canonical order (incl. disabled)', () => {
    const states = configuredToolStates(settings);
    expect(states.map((s) => s.id)).toEqual(['osv-scanner', 'gitleaks', 'semgrep']);
    expect(states.every((s) => s.status === 'idle')).toBe(true);
  });

  test('chipToolStates prefers the runtime snapshot once it has tools', () => {
    expect(chipToolStates(snapshot({ tools: [] }), settings).map((s) => s.id)).toEqual([
      'osv-scanner',
      'gitleaks',
      'semgrep',
    ]);
    expect(chipToolStates(FULL, settings)).toBe(FULL.tools);
  });
});

// ── event-merge reducer ──────────────────────────────────────────────────────

describe('mergeAuditSnapshot', () => {
  test('per-tool arrival order is independent', () => {
    // Two "one tool finished" events (partial snapshots) applied in both
    // orders must converge on the same merged tool set.
    const base = snapshot({ scanning: true, tools: [], total_findings: 0 });
    const evOsv = snapshot({ scanning: true, tools: [OSV], total_findings: 1 });
    const evGit = snapshot({ scanning: true, tools: [GITLEAKS], total_findings: 1 });

    const a = mergeAuditSnapshot(mergeAuditSnapshot(base, evOsv), evGit);
    const b = mergeAuditSnapshot(mergeAuditSnapshot(base, evGit), evOsv);

    const ids = (s: AuditSnapshot) => s.tools.map((t) => t.id);
    expect(ids(a)).toEqual(ids(b));
    expect(ids(a)).toEqual(['osv-scanner', 'gitleaks']); // canonical order
  });

  test('scalar fields come from the incoming (authoritative) snapshot', () => {
    const prev = snapshot({ scanning: true, tools: [OSV], total_findings: 1, last_scan_at: 111 });
    const next = snapshot({ scanning: false, tools: [OSV, GITLEAKS], total_findings: 2, last_scan_at: 222 });
    const merged = mergeAuditSnapshot(prev, next);
    expect(merged.scanning).toBe(false);
    expect(merged.total_findings).toBe(2);
    expect(merged.last_scan_at).toBe(222);
  });

  test('null prev returns next, reordered', () => {
    const next = snapshot({ tools: [GITLEAKS, OSV], total_findings: 2 });
    expect(mergeAuditSnapshot(null, next).tools.map((t) => t.id)).toEqual(['osv-scanner', 'gitleaks']);
  });

  test('carries per-tool scanned_artifacts through the merge', () => {
    const base = snapshot({ scanning: true, tools: [], total_findings: 0 });
    const osvScanned = tool({
      id: 'osv-scanner',
      status: 'done',
      scanned_artifacts: ['Cargo.lock', 'package-lock.json'],
    });
    const merged = mergeAuditSnapshot(mergeAuditSnapshot(base, snapshot({ tools: [GITLEAKS] })), snapshot({ tools: [osvScanned] }));
    const osv = merged.tools.find((t) => t.id === 'osv-scanner');
    expect(osv?.scanned_artifacts).toEqual(['Cargo.lock', 'package-lock.json']);
  });
});

// ── scan coverage ────────────────────────────────────────────────────────────

describe('scannedArtifacts / formatCoverageLine', () => {
  test('reads osv-scanner artifacts only once it is done', () => {
    const done = tool({ id: 'osv-scanner', status: 'done', scanned_artifacts: ['Cargo.lock'] });
    expect(scannedArtifacts(snapshot({ tools: [done] }))).toEqual(['Cargo.lock']);
    // A still-running osv-scanner is not yet authoritative → empty.
    const running = tool({ id: 'osv-scanner', status: 'running', scanned_artifacts: [] });
    expect(scannedArtifacts(snapshot({ tools: [running] }))).toEqual([]);
    // Other tools never contribute a coverage line.
    expect(scannedArtifacts(snapshot({ tools: [GITLEAKS] }))).toEqual([]);
  });

  test('formats paths with check marks', () => {
    expect(formatCoverageLine(['Cargo.lock', 'package-lock.json'])).toBe('Cargo.lock ✓ · package-lock.json ✓');
    expect(formatCoverageLine([])).toBe('');
  });
});

// ── markdown copy ────────────────────────────────────────────────────────────

describe('formatFindingsMarkdown', () => {
  test('produces the exact agent-ready markdown', () => {
    const rows = selectedRows(
      sortFindings(flattenFindings(FULL)),
      new Set(['gitleaks#0', 'osv-scanner#0']),
    );
    const md = formatFindingsMarkdown({
      rows,
      totalFindings: 3,
      root: 'P:\\proj',
      scannedAt: '2026-07-15 14:02',
    });
    expect(md).toBe(
      [
        '## Code audit findings (2 of 3 selected) — P:\\proj, scanned 2026-07-15 14:02',
        '- [error] gitleaks generic-api-key: possible API key — src/lib/foo.ts:42',
        '- [warning] osv-scanner GHSA-r8w9-5wcg-vfj7: tokio 1.38.0 vulnerable (Cargo.lock)',
      ].join('\n'),
    );
  });

  test('omits the rule id when the finding has no code', () => {
    const md = formatFindingsMarkdown({
      rows: [
        { id: 'gitleaks#0', tool: 'gitleaks', severity: 'error', code: null, message: 'secret', file: 'a.ts', line: 3, col: null },
      ],
      totalFindings: 1,
      root: '/r',
      scannedAt: 'T',
    });
    expect(md).toBe(
      ['## Code audit findings (1 of 1 selected) — /r, scanned T', '- [error] gitleaks: secret — a.ts:3'].join('\n'),
    );
  });
});

describe('formatScanTimestamp', () => {
  test('null → never', () => {
    expect(formatScanTimestamp(null)).toBe('never');
  });
  test('formats as YYYY-MM-DD HH:MM', () => {
    expect(formatScanTimestamp(1752600000000)).toMatch(/^\d{4}-\d{2}-\d{2} \d{2}:\d{2}$/);
  });
});
