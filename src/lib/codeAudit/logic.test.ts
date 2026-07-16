import { describe, expect, test } from 'vitest';

import type { CodeAuditSettings } from '../settings/types';
import type { AuditSnapshot, AuditToolId, AuditToolState } from './types';
import {
  AUDIT_TOOL_CATEGORY,
  autoSelectQuality,
  categoryFindingsCount,
  censusIsEmpty,
  chipToolStates,
  configuredToolStates,
  defaultFilters,
  deselectAll,
  filterFindings,
  flattenFindings,
  formatCoverageLine,
  formatFindingsMarkdown,
  formatScanTimestamp,
  isToolApplicable,
  mergeAuditSnapshot,
  partitionChips,
  scanLock,
  scannedArtifacts,
  selectAllVisible,
  selectedRows,
  sortFindings,
  toggleSelected,
  toolChip,
  toolsInCategory,
  visibleFindings,
} from './logic';

// ── Fixtures ─────────────────────────────────────────────────────────────────

function tool(overrides: Partial<AuditToolState> & Pick<AuditToolState, 'id'>): AuditToolState {
  return {
    category: 'security',
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
    census: { extensions: [], markers: [] },
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
    quality_auto_select: true,
    expose_claude: true,
    expose_opencode: true,
    expose_offload: true,
    tools: [
      { id: 'semgrep', enabled: false, path: '', extra_args: [], timeout_secs: null },
      { id: 'osv-scanner', enabled: true, path: '', extra_args: [], timeout_secs: null },
      { id: 'gitleaks', enabled: true, path: '', extra_args: [], timeout_secs: null },
    ],
  };

  test('renders configured tools as idle, in canonical order (incl. disabled)', () => {
    const states = configuredToolStates(settings, 'security');
    expect(states.map((s) => s.id)).toEqual(['osv-scanner', 'gitleaks', 'semgrep']);
    expect(states.every((s) => s.status === 'idle')).toBe(true);
    expect(states.every((s) => s.category === 'security')).toBe(true);
  });

  test('chipToolStates prefers the runtime snapshot once it has tools of the category', () => {
    expect(chipToolStates(snapshot({ tools: [] }), settings, 'security').map((s) => s.id)).toEqual([
      'osv-scanner',
      'gitleaks',
      'semgrep',
    ]);
    // FULL has only security tools, so the (filtered) list is the same array
    // contents — chipToolStates filters by category, so it is a new array.
    expect(chipToolStates(FULL, settings, 'security').map((s) => s.id)).toEqual([
      'osv-scanner',
      'gitleaks',
      'semgrep',
    ]);
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

// ── V25 Phase D: category mapping + language gating ───────────────────────────

const OXLINT = tool({
  id: 'oxlint',
  category: 'quality',
  status: 'done',
  findings: [
    {
      tool: 'oxlint',
      diag: { severity: 'warning', code: 'no-unused-vars', message: 'unused x', file: 'src/a.ts', line: 3, col: 1 },
    },
  ],
});

const TYPOS = tool({
  id: 'typos',
  category: 'quality',
  status: 'done',
  findings: [
    {
      tool: 'typos',
      diag: { severity: 'note', code: null, message: '`teh` should be `the`', file: 'README.md', line: 1, col: null },
    },
  ],
});

// A merged snapshot carrying BOTH categories at once — the state each tab sees
// after a Security scan then a Quality scan (mergeAuditSnapshot merges by id).
const MIXED = snapshot({
  tools: [OSV, GITLEAKS, OXLINT, TYPOS],
  total_findings: 2, // wire total reflects only the last-scanned category
  census: { extensions: ['ts'], markers: ['package.json'] },
});

describe('AUDIT_TOOL_CATEGORY / toolsInCategory', () => {
  test('the security trio is security; every other tool is quality', () => {
    expect(toolsInCategory('security')).toEqual(['osv-scanner', 'gitleaks', 'semgrep']);
    const quality = toolsInCategory('quality');
    expect(quality).toContain('oxlint');
    expect(quality).toContain('typos');
    expect(quality).toContain('semgrep-quality');
    expect(quality).not.toContain('semgrep'); // the SAST semgrep stays security
    for (const id of quality) expect(AUDIT_TOOL_CATEGORY[id]).toBe('quality');
  });
});

describe('isToolApplicable / censusIsEmpty', () => {
  test('empty census reads as empty', () => {
    expect(censusIsEmpty({ extensions: [], markers: [] })).toBe(true);
    expect(censusIsEmpty({ extensions: ['ts'], markers: [] })).toBe(false);
  });

  test('always-applicable tools apply against any census', () => {
    const empty = { extensions: [], markers: [] };
    expect(isToolApplicable('typos', empty)).toBe(true);
    expect(isToolApplicable('osv-scanner', empty)).toBe(true);
    expect(isToolApplicable('semgrep-quality', empty)).toBe(true);
  });

  test('extension gate: pmd needs .java, ruff needs .py', () => {
    const rustJs = { extensions: ['rs', 'ts'], markers: ['Cargo.toml'] };
    expect(isToolApplicable('pmd', rustJs)).toBe(false);
    expect(isToolApplicable('ruff', rustJs)).toBe(false);
    expect(isToolApplicable('oxlint', rustJs)).toBe(true); // ts
    expect(isToolApplicable('cargo-machete', rustJs)).toBe(true); // Cargo.toml marker
  });

  test('marker gate: eslint / knip / dotnet key off markers', () => {
    expect(isToolApplicable('eslint', { extensions: [], markers: ['eslint.config'] })).toBe(true);
    expect(isToolApplicable('knip', { extensions: [], markers: ['package.json'] })).toBe(true);
    expect(isToolApplicable('dotnet-analyzers', { extensions: [], markers: ['*.csproj'] })).toBe(true);
    expect(isToolApplicable('knip', { extensions: ['ts'], markers: [] })).toBe(false);
  });
});

describe('autoSelectQuality', () => {
  const rustTs = { extensions: ['ts', 'rs'], markers: ['Cargo.toml', 'package.json'] };
  const cfg = (id: AuditToolId, enabled: boolean) => ({ id, enabled });

  test('quality enabled follows applicability; heavyweights stay opt-in; security untouched', () => {
    // Everything flipped from its automatic value, so each rule below proves
    // the function rewrote (or deliberately skipped) it.
    const tools = [
      cfg('osv-scanner', false), // security — out of scope, stays false
      cfg('oxlint', false), // applicable (.ts) → on
      cfg('ruff', true), // not applicable → off
      cfg('cargo-machete', false), // applicable (Cargo.toml) → on
      cfg('typos', false), // ungated → on
      cfg('semgrep-quality', true), // ungated BUT default-off heavyweight → off
      cfg('dotnet-analyzers', true), // default-off heavyweight → off
    ];
    const out = autoSelectQuality(tools, rustTs);
    const enabled = (id: AuditToolId) => out.find((t) => t.id === id)!.enabled;
    expect(enabled('osv-scanner')).toBe(false);
    expect(enabled('oxlint')).toBe(true);
    expect(enabled('ruff')).toBe(false);
    expect(enabled('cargo-machete')).toBe(true);
    expect(enabled('typos')).toBe(true);
    expect(enabled('semgrep-quality')).toBe(false);
    expect(enabled('dotnet-analyzers')).toBe(false);
    // Input untouched (patch-path safety: the caller assigns the new array).
    expect(tools.find((t) => t.id === 'oxlint')!.enabled).toBe(false);
  });

  test('already-automatic flags come back unchanged by reference', () => {
    const tools = [cfg('oxlint', true), cfg('ruff', false)];
    const out = autoSelectQuality(tools, rustTs);
    expect(out[0]).toBe(tools[0]);
    expect(out[1]).toBe(tools[1]);
  });
});

describe('flattenFindings by category', () => {
  test('filters a mixed snapshot to one tab', () => {
    expect(flattenFindings(MIXED, 'security').map((r) => r.tool).sort()).toEqual([
      'gitleaks',
      'osv-scanner',
    ]);
    expect(flattenFindings(MIXED, 'quality').map((r) => r.tool).sort()).toEqual(['oxlint', 'typos']);
    // No category arg → everything (back-compat with the un-split callers/tests).
    expect(flattenFindings(MIXED).length).toBe(4);
  });
});

describe('categoryFindingsCount', () => {
  test('counts only the category tools, independent of wire total_findings', () => {
    expect(categoryFindingsCount(MIXED, 'security')).toBe(2);
    expect(categoryFindingsCount(MIXED, 'quality')).toBe(2);
  });
});

describe('chipToolStates falls back per category', () => {
  const settings: CodeAuditSettings = {
    enabled: true,
    timeout_secs: 600,
    quality_auto_select: true,
    expose_claude: true,
    expose_opencode: true,
    expose_offload: true,
    tools: [
      { id: 'oxlint', enabled: true, path: '', extra_args: [], timeout_secs: null },
      { id: 'ruff', enabled: true, path: '', extra_args: [], timeout_secs: null },
      { id: 'typos', enabled: true, path: '', extra_args: [], timeout_secs: null },
    ],
  };

  test('a quality tab over a security-only snapshot renders configured quality idle chips', () => {
    const secOnly = snapshot({ tools: [OSV, GITLEAKS] });
    const states = chipToolStates(secOnly, settings, 'quality');
    expect(states.map((s) => s.id)).toEqual(['oxlint', 'ruff', 'typos']);
    expect(states.every((s) => s.status === 'idle' && s.category === 'quality')).toBe(true);
  });

  test('prefers the snapshot once the category has scanned tools', () => {
    expect(chipToolStates(MIXED, settings, 'quality').map((s) => s.id)).toEqual(['oxlint', 'typos']);
  });
});

describe('partitionChips (chip gating + hidden count)', () => {
  test('empty census hides nothing', () => {
    const states = configuredToolStates(
      { enabled: true, timeout_secs: 600, quality_auto_select: true, expose_claude: true, expose_opencode: true, expose_offload: true, tools: toolsInCategory('quality').map((id) => ({ id, enabled: true, path: '', extra_args: [], timeout_secs: null })) },
      'quality',
    );
    const p = partitionChips(states, { extensions: [], markers: [] });
    expect(p.hiddenCount).toBe(0);
    expect(p.visible.length).toBe(states.length);
  });

  test('known census hides not-applicable idle chips and counts them', () => {
    // A Rust + JS project: no .py, no .java, no .go, no .cs, no .c.
    const census = { extensions: ['rs', 'ts'], markers: ['Cargo.toml', 'package.json'] };
    const states = configuredToolStates(
      { enabled: true, timeout_secs: 600, quality_auto_select: true, expose_claude: true, expose_opencode: true, expose_offload: true, tools: toolsInCategory('quality').map((id) => ({ id, enabled: true, path: '', extra_args: [], timeout_secs: null })) },
      'quality',
    );
    const p = partitionChips(states, census);
    const visibleIds = p.visible.map((s) => s.id);
    // oxlint (ts), typos (always), cargo-machete (Cargo.toml), knip (package.json),
    // semgrep-quality (always) apply; ruff/cppcheck/pmd/golangci-lint/dotnet do not.
    expect(visibleIds).toContain('oxlint');
    expect(visibleIds).toContain('typos');
    expect(visibleIds).toContain('cargo-machete');
    expect(visibleIds).toContain('knip');
    expect(visibleIds).not.toContain('ruff');
    expect(visibleIds).not.toContain('pmd');
    expect(p.hiddenCount).toBe(states.length - p.visible.length);
    expect(p.hiddenCount).toBeGreaterThan(0);
  });

  test('a skipped-not-applicable status chip is always hidden', () => {
    const skipped = tool({ id: 'pmd', category: 'quality', status: 'skipped-not-applicable' });
    const p = partitionChips([OXLINT, skipped], { extensions: ['ts', 'java'], markers: [] });
    // Even though .java is present, the backend already gated pmd out.
    expect(p.visible.map((s) => s.id)).toEqual(['oxlint']);
    expect(p.hiddenCount).toBe(1);
  });
});

describe('scanLock (cross-tab one-scan-at-a-time)', () => {
  test('idle: both actions available', () => {
    expect(scanLock(false, null, 'security')).toEqual({ showCancel: false, scanDisabled: false, waiting: null });
  });

  test('own category running → show Cancel', () => {
    expect(scanLock(true, 'security', 'security')).toEqual({ showCancel: true, scanDisabled: false, waiting: null });
  });

  test('other category running → disabled Scan + waiting note naming the other', () => {
    expect(scanLock(true, 'security', 'quality')).toEqual({
      showCancel: false,
      scanDisabled: true,
      waiting: 'waiting — security scan running',
    });
    expect(scanLock(true, 'quality', 'security').waiting).toBe('waiting — quality scan running');
  });

  test('scanning but unknown active category → generic waiting', () => {
    expect(scanLock(true, null, 'security')).toEqual({
      showCancel: false,
      scanDisabled: true,
      waiting: 'waiting — scan running',
    });
  });
});

describe('formatFindingsMarkdown label', () => {
  test('quality label swaps the heading', () => {
    const md = formatFindingsMarkdown({
      rows: [{ id: 'oxlint#0', tool: 'oxlint', severity: 'warning', code: 'no-unused-vars', message: 'unused x', file: 'src/a.ts', line: 3, col: 1 }],
      totalFindings: 1,
      root: '/r',
      scannedAt: 'T',
      label: 'Code quality',
    });
    expect(md.startsWith('## Code quality findings (1 of 1 selected) — /r, scanned T')).toBe(true);
  });
});
