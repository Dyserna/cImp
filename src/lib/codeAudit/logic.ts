// V23 Phase C: extractable (pure, unit-tested) logic for the Code Audit tab —
// the snapshot→row flattening, sort/filter, selection, status-chip mapping,
// markdown copy formatting, and the per-tool event-merge reducer. The Svelte
// component (`CodeAuditView.svelte`) is not unit-tested in this repo, so every
// piece of logic that can be tested lives here (mirrors the `checksEditor.ts` /
// `codeAudit.ts` split).

import type {
  AuditCategory,
  AuditCensus,
  AuditFinding,
  AuditSeverity,
  AuditSnapshot,
  AuditToolId,
  AuditToolRef,
  AuditToolState,
  AuditToolStatus,
} from './types';

// ── Tool → category / order / applicability ───────────────────────────────
//
// SINGLE frontend source of truth for the built-in tool metadata the split UI
// needs, mirroring cImp's own embedded manifest
// (`src-tauri/src/plugins/builtin/cimp-audit.json`):
//   - a tool's `kind` → `AUDIT_TOOL_CATEGORY` (security ⇒ Security, audit ⇒ Quality)
//   - its `applicability` → `AUDIT_TOOL_APPLICABILITY`
//   - the manifest's tool order → `AUDIT_TOOL_ORDER`
//   - `enabled_by_default: false` → `AUDIT_TOOL_DEFAULT_OFF`
// `builtin_audit_tool_ids_are_mirrored_in_the_frontend_union` (Rust) reads that
// manifest and checks every id appears in the union below; the maps themselves
// are kept in lockstep by hand, which is why each is a small closed table with
// its Rust counterpart named.
//
// These are for the PANEL, not for settings: a built-in scanner is configured in
// the Tool Plugins pane like any other tool, and the roster a scan would run is
// `audit_effective_roster`'s answer, not a re-derivation from these maps.

/// Which tab each tool belongs to. Security = the V23 trio (Code Audit tab);
/// Quality = the V25 linters (the Code Audit tab's Quality sub-tab).
export const AUDIT_TOOL_CATEGORY: Record<AuditToolId, AuditCategory> = {
  'osv-scanner': 'security',
  gitleaks: 'security',
  semgrep: 'security',
  oxlint: 'quality',
  'golangci-lint': 'quality',
  ruff: 'quality',
  cppcheck: 'quality',
  typos: 'quality',
  eslint: 'quality',
  pmd: 'quality',
  'dotnet-analyzers': 'quality',
  knip: 'quality',
  'cargo-machete': 'quality',
  'semgrep-quality': 'quality',
};

/// Canonical display order across BOTH categories — security first, then the
/// quality tools, matching the embedded manifest's own tool order. Drives the chip list, the sort tiebreak, and the merge
/// re-order; each tab renders only its own category's slice (`toolsInCategory`).
export const AUDIT_TOOL_ORDER: readonly AuditToolId[] = [
  // Security (V23) — the Security sub-tab.
  'osv-scanner',
  'gitleaks',
  'semgrep',
  // Quality (V25) — the Quality sub-tab.
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
];

/// The tools of one category, in canonical order — the per-tab chip list and
/// tool-toggle set.
export function toolsInCategory(category: AuditCategory): AuditToolId[] {
  return AUDIT_TOOL_ORDER.filter((id) => AUDIT_TOOL_CATEGORY[id] === category);
}

/// A tool's language gate (mirror of Rust `Applicability`): the file extensions
/// and marker tokens the project must contain for the tool to apply. BOTH lists
/// empty = always applicable (the security trio, typos, semgrep-quality).
export const AUDIT_TOOL_APPLICABILITY: Record<
  AuditToolId,
  { extensions: readonly string[]; markers: readonly string[] }
> = {
  'osv-scanner': { extensions: [], markers: [] },
  gitleaks: { extensions: [], markers: [] },
  semgrep: { extensions: [], markers: [] },
  oxlint: { extensions: ['js', 'ts', 'jsx', 'tsx', 'mjs', 'cjs'], markers: [] },
  'golangci-lint': { extensions: ['go'], markers: ['go.mod'] },
  ruff: { extensions: ['py'], markers: [] },
  cppcheck: { extensions: ['c', 'cc', 'cpp', 'cxx', 'h', 'hpp'], markers: [] },
  typos: { extensions: [], markers: [] },
  eslint: { extensions: [], markers: ['eslint.config', '.eslintrc'] },
  pmd: { extensions: ['java'], markers: [] },
  'dotnet-analyzers': { extensions: [], markers: ['*.sln', '*.csproj'] },
  knip: { extensions: [], markers: ['package.json'] },
  'cargo-machete': { extensions: [], markers: ['Cargo.toml'] },
  'semgrep-quality': { extensions: [], markers: [] },
};

/// The quality tools whose FACTORY default is disabled (mirror of the
/// `enabled_by_default: false` tools in the embedded manifest,
/// `src-tauri/src/plugins/builtin/cimp-audit.json` — `dotnet-analyzers` runs a
/// real build, `semgrep-quality` downloads network rulesets; pinned Rust-side
/// by `the_heavyweight_tools_stay_opt_in`). Quality auto-selection keeps these
/// opt-in even when applicable.
export const AUDIT_TOOL_DEFAULT_OFF: ReadonlySet<AuditToolId> = new Set([
  'dotnet-analyzers',
  'semgrep-quality',
]);

/// Every built-in QUALITY tool paired with the `enabled` auto-selection wants
/// for it: its factory default AND applicable to `census`. Security tools are
/// never in scope, and neither is a user plugin's tool — auto-selection is a
/// statement about the roster cImp ships and knows the shape of.
///
/// Mirror of Rust `audit::runner::auto_select_quality`; the Settings button
/// uses it for an instant client-side apply, and the backend re-applies the
/// same rule on every census refresh. Callers must skip an empty census
/// (`censusIsEmpty`) — "no languages seen yet" must not deselect everything.
///
/// Returns the full desired state rather than a diff: the caller writes into a
/// keyed settings container where "absent" already means "the manifest's
/// default", so it needs to know what every tool should be, not only what
/// changed.
export function qualityAutoSelection(
  census: AuditCensus,
): { id: AuditToolId; enabled: boolean }[] {
  return toolsInCategory('quality').map((id) => ({
    id,
    enabled: !AUDIT_TOOL_DEFAULT_OFF.has(id) && isToolApplicable(id, census),
  }));
}

/// A census with neither extensions nor markers — the pre-first-scan state.
/// Treated as "unknown, hide nothing" by the chip-gating logic.
export function censusIsEmpty(census: AuditCensus): boolean {
  return census.extensions.length === 0 && census.markers.length === 0;
}

/// Whether `id` applies to a project with the given census (mirror of Rust
/// `Adapter::applicable`): always true for an unguarded tool, else true iff ANY
/// gate extension OR ANY gate marker was seen. Callers must special-case an
/// empty census (`censusIsEmpty`) — with nothing seen a guarded tool reads as
/// not-applicable, which for chip visibility means "unknown", not "hide".
/// Whether a tool applies to a project with this census.
///
/// V38: an id this build has no applicability metadata for is a PLUGIN tool,
/// whose gate lives in its manifest and is evaluated backend-side — the snapshot
/// already says `skipped-not-applicable` when it did not apply. Answering
/// "applicable" here is therefore not a guess but a deferral: the frontend
/// hides nothing it was not told to hide.
export function isToolApplicable(id: AuditToolRef, census: AuditCensus): boolean {
  const a = AUDIT_TOOL_APPLICABILITY[id as AuditToolId];
  if (!a) return true;
  if (a.extensions.length === 0 && a.markers.length === 0) return true;
  return (
    a.extensions.some((e) => census.extensions.includes(e)) ||
    a.markers.some((m) => census.markers.includes(m))
  );
}

/// Severity ordering for the default sort + the threshold filter: error is the
/// most severe. A row passes a threshold iff its rank is ≥ the threshold's.
export const SEVERITY_RANK: Record<AuditSeverity, number> = { error: 3, warning: 2, note: 1 };

/// The three severities, most-severe first — for the threshold `<select>`.
export const SEVERITIES: readonly AuditSeverity[] = ['error', 'warning', 'note'];

/// Sort rank: the built-in order, then everything else (plugin tools) after it,
/// stable in arrival order. Deliberately not alphabetical among plugins — the
/// backend emits them in registry order (plugin key, then manifest order), which
/// is the order the settings pane lists them in.
function toolRank(id: AuditToolRef): number {
  const i = (AUDIT_TOOL_ORDER as readonly string[]).indexOf(id);
  return i < 0 ? AUDIT_TOOL_ORDER.length : i;
}

// ── Findings rows ───────────────────────────────────────────────────────────

/// One flattened finding for the table. `id` is stable across snapshot updates
/// (a completed tool's findings array never changes), so per-row selection
/// survives live event refreshes.
export interface FindingRow {
  /// `${tool}#${indexWithinTool}` — stable, unique within a snapshot.
  id: string;
  tool: AuditToolRef;
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
/// keep their original index in the id. When `category` is given, only that
/// category's tools contribute — the split UI passes its own category so a
/// merged snapshot carrying BOTH categories (see `mergeAuditSnapshot`) renders
/// only this tab's findings.
export function flattenFindings(snapshot: AuditSnapshot, category?: AuditCategory): FindingRow[] {
  const rows: FindingRow[] = [];
  for (const tool of snapshot.tools) {
    if (category && tool.category !== category) continue;
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
  /// Keyed by the WIRE id, so a plugin tool's toggle works like any other's.
  tools: Partial<Record<string, boolean>>;
  /// Case-insensitive substring over message / file / rule id / tool.
  text: string;
}

/// The default filter state: everything visible (note threshold shows all
/// severities), no text filter.
export function defaultFilters(): AuditFilters {
  return { severity: 'note', tools: {}, text: '' };
}

function toolVisible(filters: AuditFilters, id: AuditToolRef): boolean {
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
    case 'path-invalid':
      // Distinct from not-installed: a path IS configured but doesn't
      // resolve (stale per-project path, moved binary). The component reuses
      // the not-installed Settings link for the fix-it affordance.
      return {
        kind: 'path-invalid',
        label: 'bad path',
        spinner: false,
        tooltip: tool.error ?? 'configured path not found — fix it in Settings',
      };
    case 'skipped-not-applicable':
      // V25 Phase C: enabled but gated off by the project census. The Security
      // trio is always applicable so this never arises in this tab; Phase D's
      // Quality tab hides these chips outright. A neutral display keeps the
      // mapping total meanwhile.
      return {
        kind: 'skipped-not-applicable',
        label: 'not applicable',
        spinner: false,
        tooltip: 'this tool does not apply to the current project',
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

// ── Pre-scan chips ─────────────────────────────────────────────────────────

/// The `category` tool states to render as chips, in preference order:
///
/// 1. the snapshot's OWN (category-filtered) list, once a scan of this category
///    has produced one — authoritative, it is what actually ran;
/// 2. `roster`, the backend's `audit_effective_roster` answer — what a scan
///    *would* run right now, built-ins **and** plugin tools.
///
/// There is no third fallback since V38 Phase E. There used to be
/// `configuredToolStates`, a built-ins-only derivation from
/// `settings.code_audit.tools`; that array is gone, and re-deriving the roster
/// in the browser from the settings container would mean re-implementing the
/// manifest ⋈ user-state ⋈ project join that `audit_effective_roster` exists to
/// answer — which is exactly how two lists start disagreeing. Before the IPC
/// answers the chip strip is empty for a moment, which is honest.
///
/// A merged snapshot may carry both categories' tools (see
/// `mergeAuditSnapshot`), so this always filters by category first. `roster` is
/// likewise filtered rather than trusted: the panel holds one category and a
/// stale answer for the other one must not leak into it.
export function chipToolStates(
  snapshot: AuditSnapshot,
  category: AuditCategory,
  roster: readonly AuditToolState[] | null = null,
): AuditToolState[] {
  const own = snapshot.tools.filter((t) => t.category === category);
  if (own.length > 0) return own;
  return (roster ?? []).filter((t) => t.category === category);
}

/// The chip-visibility split for one tab. A tool is HIDDEN when it's
/// `skipped-not-applicable` (the backend gated it out during a scan) or, for a
/// pre-scan idle fallback against a KNOWN census, when it doesn't apply to the
/// project. An empty census (`censusIsEmpty`) hides nothing — before the first
/// scan every configured tool shows. `hiddenCount` drives the muted "n tools
/// hidden — not applicable to this project" line.
export interface ChipPartition {
  visible: AuditToolState[];
  hiddenCount: number;
}

export function partitionChips(
  states: readonly AuditToolState[],
  census: AuditCensus,
): ChipPartition {
  const known = !censusIsEmpty(census);
  const visible: AuditToolState[] = [];
  let hiddenCount = 0;
  for (const s of states) {
    const gatedOff =
      s.status === 'skipped-not-applicable' || (known && !isToolApplicable(s.id, census));
    if (gatedOff) hiddenCount++;
    else visible.push(s);
  }
  return { visible, hiddenCount };
}

/// Total findings across one category's tools in a (possibly merged, both-
/// category) snapshot. The wire `total_findings` reflects only the LAST scanned
/// category, so each tab computes its own from its filtered tools.
export function categoryFindingsCount(snapshot: AuditSnapshot, category: AuditCategory): number {
  return snapshot.tools
    .filter((t) => t.category === category)
    .reduce((n, t) => n + t.findings.length, 0);
}

/// The cross-tab scan-lock state for one tab, from the GLOBAL scanning flag and
/// which category's scan is in flight (`activeCategory`, tracked from the raw
/// event/snapshot whose tools all share one category). One scan runs at a time
/// globally, so while the OTHER tab scans this tab's Scan button is disabled
/// with a "waiting — <other> scan running" note.
export interface ScanLock {
  /// Show the Cancel button (this category's own scan is running).
  showCancel: boolean;
  /// Disable the Scan button (the other category is running).
  scanDisabled: boolean;
  /// The muted status-line note while waiting, or `null`.
  waiting: string | null;
}

export function scanLock(
  scanning: boolean,
  activeCategory: AuditCategory | null,
  myCategory: AuditCategory,
): ScanLock {
  if (!scanning) return { showCancel: false, scanDisabled: false, waiting: null };
  if (activeCategory === myCategory) return { showCancel: true, scanDisabled: false, waiting: null };
  const other = activeCategory ? `${activeCategory} scan running` : 'scan running';
  return { showCancel: false, scanDisabled: true, waiting: `waiting — ${other}` };
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
  const byId = new Map<AuditToolRef, AuditToolState>();
  for (const t of prev.tools) byId.set(t.id, t);
  for (const t of next.tools) byId.set(t.id, t);
  return {
    root: next.root || prev.root,
    scanning: next.scanning,
    last_scan_at: next.last_scan_at ?? prev.last_scan_at,
    tools: orderToolStates([...byId.values()]),
    // V25 Phase C: `next` is authoritative for the census (taken at scan start).
    census: next.census,
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
  /// Heading prefix — `'Code audit'` (default, Security tab) / `'Code quality'`
  /// (Quality tab). Renders as `## <label> findings (…)`.
  label?: string;
}): string {
  const { rows, totalFindings, root, scannedAt, label = 'Code audit' } = opts;
  const header = `## ${label} findings (${rows.length} of ${totalFindings} selected) — ${root}, scanned ${scannedAt}`;
  const bullets = rows.map((r) => {
    const rule = r.code ? `${r.tool} ${r.code}` : r.tool;
    return `- [${r.severity}] ${rule}: ${r.message}${locationSuffix(r)}`;
  });
  return [header, ...bullets].join('\n');
}
