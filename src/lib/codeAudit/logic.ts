// V23 Phase C: extractable (pure, unit-tested) logic for the Code Audit tab —
// the snapshot→row flattening, sort/filter, selection, status-chip mapping,
// markdown copy formatting, and the per-tool event-merge reducer. The Svelte
// component (`CodeAuditView.svelte`) is not unit-tested in this repo, so every
// piece of logic that can be tested lives here (mirrors the `checksEditor.ts` /
// `codeAudit.ts` split).

import type { CodeAuditSettings } from '../settings/types';
import type {
  AuditFinding,
  AuditSeverity,
  AuditSnapshot,
  AuditToolId,
  AuditToolState,
  AuditToolStatus,
} from './types';

// ── Tool + severity ordering ──────────────────────────────────────────────

/// Canonical display order for the tools (matches `AUDIT_TOOL_META` in
/// `../settings/codeAudit`). Used for the chip list and the sort tiebreak.
export const AUDIT_TOOL_ORDER: readonly AuditToolId[] = ['osv-scanner', 'gitleaks', 'semgrep'];

/// Severity ordering for the default sort + the threshold filter: error is the
/// most severe. A row passes a threshold iff its rank is ≥ the threshold's.
export const SEVERITY_RANK: Record<AuditSeverity, number> = { error: 3, warning: 2, note: 1 };

/// The three severities, most-severe first — for the threshold `<select>`.
export const SEVERITIES: readonly AuditSeverity[] = ['error', 'warning', 'note'];

function toolRank(id: AuditToolId): number {
  const i = AUDIT_TOOL_ORDER.indexOf(id);
  return i < 0 ? AUDIT_TOOL_ORDER.length : i;
}

// ── Findings rows ───────────────────────────────────────────────────────────

/// One flattened finding for the table. `id` is stable across snapshot updates
/// (a completed tool's findings array never changes), so per-row selection
/// survives live event refreshes.
export interface FindingRow {
  /// `${tool}#${indexWithinTool}` — stable, unique within a snapshot.
  id: string;
  tool: AuditToolId;
  severity: AuditSeverity;
  /// SARIF rule id, or `null`.
  code: string | null;
  message: string;
  /// Project-relative path.
  file: string;
  /// 0 when the tool reported no line.
  line: number;
  col: number | null;
}

/// Flatten every tool's findings into one list of rows (unsorted, unfiltered),
/// with stable ids. Tools are visited in snapshot order; each tool's findings
/// keep their original index in the id.
export function flattenFindings(snapshot: AuditSnapshot): FindingRow[] {
  const rows: FindingRow[] = [];
  for (const tool of snapshot.tools) {
    tool.findings.forEach((f: AuditFinding, i: number) => {
      rows.push({
        id: `${tool.id}#${i}`,
        tool: tool.id,
        severity: f.diag.severity,
        code: f.diag.code,
        message: f.diag.message,
        file: f.diag.file,
        line: f.diag.line,
        col: f.diag.col,
      });
    });
  }
  return rows;
}

// ── Filtering + sorting ─────────────────────────────────────────────────────

export interface AuditFilters {
  /// Severity threshold — rows at or above this rank are shown.
  severity: AuditSeverity;
  /// Per-tool visibility. A tool absent from the map is treated as visible.
  tools: Partial<Record<AuditToolId, boolean>>;
  /// Case-insensitive substring over message / file / rule id / tool.
  text: string;
}

/// The default filter state: everything visible (note threshold shows all
/// severities), no text filter.
export function defaultFilters(): AuditFilters {
  return { severity: 'note', tools: {}, text: '' };
}

function toolVisible(filters: AuditFilters, id: AuditToolId): boolean {
  return filters.tools[id] !== false;
}

function matchesText(row: FindingRow, needle: string): boolean {
  if (!needle) return true;
  const hay = `${row.message}\n${row.file}\n${row.code ?? ''}\n${row.tool}`.toLowerCase();
  return hay.includes(needle.toLowerCase());
}

/// Apply the severity threshold, per-tool toggle, and text filter. Order is
/// preserved; sort separately with `sortFindings`.
export function filterFindings(rows: FindingRow[], filters: AuditFilters): FindingRow[] {
  const threshold = SEVERITY_RANK[filters.severity];
  const needle = filters.text.trim().toLowerCase();
  return rows.filter(
    (r) =>
      SEVERITY_RANK[r.severity] >= threshold &&
      toolVisible(filters, r.tool) &&
      matchesText(r, needle),
  );
}

/// Default sort: severity descending (error > warning > note), then tool (in
/// `AUDIT_TOOL_ORDER`), then file then line for stability. Non-mutating.
export function sortFindings(rows: FindingRow[]): FindingRow[] {
  return [...rows].sort((a, b) => {
    const sev = SEVERITY_RANK[b.severity] - SEVERITY_RANK[a.severity];
    if (sev !== 0) return sev;
    const tool = toolRank(a.tool) - toolRank(b.tool);
    if (tool !== 0) return tool;
    const file = a.file.localeCompare(b.file);
    if (file !== 0) return file;
    return a.line - b.line;
  });
}

/// The visible, sorted rows — what the table renders and what "Select all"
/// selects. Composes filter then sort.
export function visibleFindings(rows: FindingRow[], filters: AuditFilters): FindingRow[] {
  return sortFindings(filterFindings(rows, filters));
}

// ── Selection ───────────────────────────────────────────────────────────────

/// Add every currently-visible (filtered) row to the selection, preserving any
/// already-selected rows that are currently filtered out (spec: "Select all
/// selects the visible set" — it's additive over the visible rows).
export function selectAllVisible(
  selected: ReadonlySet<string>,
  visible: FindingRow[],
): Set<string> {
  const next = new Set(selected);
  for (const r of visible) next.add(r.id);
  return next;
}

/// Clear the whole selection.
export function deselectAll(): Set<string> {
  return new Set();
}

/// Toggle one row's membership.
export function toggleSelected(selected: ReadonlySet<string>, id: string): Set<string> {
  const next = new Set(selected);
  if (next.has(id)) next.delete(id);
  else next.add(id);
  return next;
}

/// The selected rows that still exist in `allRows`, in `allRows` order. Stale
/// ids (a prior snapshot's rows that a rescan removed) are dropped.
export function selectedRows(allRows: FindingRow[], selected: ReadonlySet<string>): FindingRow[] {
  return allRows.filter((r) => selected.has(r.id));
}

// ── Status chips ────────────────────────────────────────────────────────────

export interface ChipDisplay {
  /// The tool's status — drives the chip's CSS class + icon.
  kind: AuditToolStatus;
  /// The chip's text (icon rendered separately by the component).
  label: string;
  /// Whether to show the running spinner.
  spinner: boolean;
  /// Hover text for `failed` / `not-installed` (the error string); else `null`.
  tooltip: string | null;
}

/// Map one tool's runtime state to its chip. Pure — the component adds the ✓/✗
/// glyphs and the "not installed → Settings" link based on `kind`.
export function toolChip(tool: AuditToolState): ChipDisplay {
  switch (tool.status) {
    case 'idle':
      return { kind: 'idle', label: 'idle', spinner: false, tooltip: null };
    case 'running':
      return { kind: 'running', label: 'running', spinner: true, tooltip: null };
    case 'done': {
      const n = tool.findings.length;
      return { kind: 'done', label: `${n} finding${n === 1 ? '' : 's'}`, spinner: false, tooltip: null };
    }
    case 'failed':
      return { kind: 'failed', label: 'error', spinner: false, tooltip: tool.error ?? 'failed' };
    case 'not-installed':
      return {
        kind: 'not-installed',
        label: 'not installed',
        spinner: false,
        tooltip: tool.error ?? 'not found on PATH or ebin',
      };
  }
}

// ── Scan coverage ─────────────────────────────────────────────────────────────

/// The lockfiles / manifests osv-scanner reported *scanning* this run — its
/// `scanned_artifacts`, but only once it has run to completion (`done`). A
/// still-running or failed osv-scanner, or the pre-scan/other tools, yield `[]`
/// so the coverage line stays hidden until it's meaningful. This is the honesty
/// signal: a "0 findings" run over an unscannable ecosystem shows an empty (or
/// short) coverage line rather than reading as a clean bill of health.
export function scannedArtifacts(snapshot: AuditSnapshot): string[] {
  const osv = snapshot.tools.find((t) => t.id === 'osv-scanner');
  return osv && osv.status === 'done' ? osv.scanned_artifacts : [];
}

/// Render the coverage paths as `Cargo.lock ✓ · package-lock.json ✓`. Empty in
/// → empty string out (the component hides the line).
export function formatCoverageLine(paths: readonly string[]): string {
  return paths.map((p) => `${p} ✓`).join(' · ');
}

// ── Pre-scan configured tool list ────────────────────────────────────────────

/// Before the first scan the backend snapshot has `tools: []`; render the
/// configured tool list from settings as idle chips (in canonical order,
/// including disabled tools — spec). Once a scan starts the backend's `tools`
/// is authoritative and this is unused.
export function configuredToolStates(settings: CodeAuditSettings): AuditToolState[] {
  const byId = new Map<AuditToolId, boolean>();
  for (const t of settings.tools) byId.set(t.id, t.enabled);
  const out: AuditToolState[] = [];
  for (const id of AUDIT_TOOL_ORDER) {
    if (!byId.has(id)) continue;
    out.push({
      id,
      status: 'idle',
      findings: [],
      duration_ms: 0,
      error: null,
      resolved: null,
      scanned_artifacts: [],
    });
  }
  return out;
}

/// The tool states to render as chips: the snapshot's own list once a scan has
/// produced one, else the configured fallback.
export function chipToolStates(
  snapshot: AuditSnapshot,
  settings: CodeAuditSettings,
): AuditToolState[] {
  return snapshot.tools.length > 0 ? snapshot.tools : configuredToolStates(settings);
}

// ── Event-merge reducer ─────────────────────────────────────────────────────

/// Merge an incoming `audit-status` snapshot into the current one, keyed by
/// tool id so the result is independent of the order tools transition/arrive
/// in. Each `audit-status` event carries a full snapshot, so `next` is
/// authoritative for its tools and its scalar fields (`scanning`,
/// `total_findings`, `truncated`); tools present only in `prev` are preserved
/// (defensive against a partial event). The resulting `tools` are re-ordered
/// into `AUDIT_TOOL_ORDER`.
///
/// NOTE: a `truncated` `next` carries per-tool findings capped at 500 — the
/// view fetches the full set via `audit_snapshot` and feeds THAT through this
/// reducer (uncapped), so the merged state converges on the complete findings.
export function mergeAuditSnapshot(
  prev: AuditSnapshot | null,
  next: AuditSnapshot,
): AuditSnapshot {
  if (!prev) return { ...next, tools: orderToolStates(next.tools) };
  const byId = new Map<AuditToolId, AuditToolState>();
  for (const t of prev.tools) byId.set(t.id, t);
  for (const t of next.tools) byId.set(t.id, t);
  return {
    root: next.root || prev.root,
    scanning: next.scanning,
    last_scan_at: next.last_scan_at ?? prev.last_scan_at,
    tools: orderToolStates([...byId.values()]),
    total_findings: next.total_findings,
    truncated: next.truncated,
  };
}

function orderToolStates(tools: AuditToolState[]): AuditToolState[] {
  return [...tools].sort((a, b) => toolRank(a.id) - toolRank(b.id));
}

// ── Markdown copy ───────────────────────────────────────────────────────────

/// Format a local `YYYY-MM-DD HH:MM` timestamp from epoch millis (the header's
/// "scanned …"). Pure but timezone-local; the markdown formatter takes the
/// pre-formatted string so it stays deterministic under test.
export function formatScanTimestamp(ms: number | null): string {
  if (ms === null) return 'never';
  const d = new Date(ms);
  const p = (n: number) => String(n).padStart(2, '0');
  return `${d.getFullYear()}-${p(d.getMonth() + 1)}-${p(d.getDate())} ${p(d.getHours())}:${p(d.getMinutes())}`;
}

function locationSuffix(row: FindingRow): string {
  // A located finding reads "… — file:line"; a line-less one (line 0, e.g. a
  // lockfile-wide dependency CVE) reads "… (file)" — matches the spec example.
  return row.line > 0 ? ` — ${row.file}:${row.line}` : ` (${row.file})`;
}

/// The clipboard markdown for the selected findings — agent-ready to paste into
/// a Claude Code / OpenCode prompt. Uses the REAL Diag severity names
/// (error|warning|note), project-relative paths, and the true total (`M`) in
/// the header even though only `N` rows are copied. `rows` should already be in
/// display order (pass `selectedRows(sortedVisible, …)`).
export function formatFindingsMarkdown(opts: {
  rows: FindingRow[];
  totalFindings: number;
  root: string;
  scannedAt: string;
}): string {
  const { rows, totalFindings, root, scannedAt } = opts;
  const header = `## Code audit findings (${rows.length} of ${totalFindings} selected) — ${root}, scanned ${scannedAt}`;
  const bullets = rows.map((r) => {
    const rule = r.code ? `${r.tool} ${r.code}` : r.tool;
    return `- [${r.severity}] ${rule}: ${r.message}${locationSuffix(r)}`;
  });
  return [header, ...bullets].join('\n');
}
