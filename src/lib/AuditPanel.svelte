<script lang="ts">
  // V25 Phase D: the shared, category-parameterized core behind the Code Audit
  // tab's two sub-tabs. `CodeAuditView.svelte` mounts it twice: with
  // `category="security"` (osv-scanner / gitleaks / semgrep — the V23
  // behavior) and with `category="quality"` (language-gated linters /
  // dead-code / spell-check). Both instances stay mounted; the inactive one is
  // just display-hidden, so a scan keeps streaming into a hidden sub-tab.
  //
  // Both panels subscribe to the ONE `audit-status` event stream and filter
  // every snapshot to their own category (a merged snapshot can carry both —
  // see `mergeAuditSnapshot`). Only one scan runs at a time globally, so while
  // the other sub-tab scans this one's Scan button shows a "waiting — <other>
  // scan running" note (`scanLock`). All testable logic lives in
  // `./codeAudit/logic`.
  import { onMount, onDestroy } from 'svelte';
  import { listen, type UnlistenFn } from '@tauri-apps/api/event';
  import { writeText as clipboardWriteText } from '@tauri-apps/plugin-clipboard-manager';
  import { settings } from './settings/store';
  import { openSettingsWindowToSection } from './settings/ipc';
  import { TOOL_ACTIVITY_TAB_ID } from './tabs/types';
  import { onAppViewShown } from './appViewVisibility';
  import { revealTab } from './tabs/visibility';
  import { revealFileInGraph } from './graphReveal';
  import { loadViewString, saveViewString, loadViewSet, saveViewSet } from './viewSection';
  import {
    auditStartScan,
    auditCancelScan,
    auditSnapshot,
    auditRefreshCensus,
    auditEffectiveRoster,
  } from './codeAudit/ipc';
  import type {
    AuditCategory,
    AuditSeverity,
    AuditSnapshot,
    AuditToolRef,
    AuditToolState,
  } from './codeAudit/types';
  import {
    SEVERITIES,
    categoryFindingsCount,
    chipToolStates,
    deselectAll,
    flattenFindings,
    formatCoverageLine,
    formatFindingsMarkdown,
    formatScanTimestamp,
    mergeAuditSnapshot,
    partitionChips,
    scanLock,
    scannedArtifacts,
    selectAllVisible,
    selectedRows,
    sortFindings,
    toggleSelected,
    toolChip,
    visibleFindings,
    type AuditFilters,
    type FindingRow,
  } from './codeAudit/logic';

  // ── Props ─────────────────────────────────────────────────────────────────
  // `category` selects which tools/findings this instance owns; `view` is the
  // localStorage key for its persisted filters (distinct per sub-tab). Both are
  // fixed for the lifetime of an instance (mounted once per sub-tab), so
  // reading their initial value to seed persisted filters below is intentional.
  let { category, view }: { category: AuditCategory; view: string } = $props();

  const isSecurity = $derived(category === 'security');
  // Sub-tab heading — the enclosing view carries the "Code Audit" title.
  const heading = $derived(isSecurity ? 'Security' : 'Quality');
  const mdLabel = $derived(isSecurity ? 'Code audit (security)' : 'Code audit (quality)');

  function emptySnapshot(): AuditSnapshot {
    return {
      root: '',
      scanning: false,
      last_scan_at: null,
      tools: [],
      census: { extensions: [], markers: [] },
      total_findings: 0,
      truncated: false,
    };
  }

  // ── Runtime state ───────────────────────────────────────────────────────
  let snapshot = $state<AuditSnapshot>(emptySnapshot());
  // V38: the backend's answer to "what would a scan of this category run?" —
  // built-ins AND plugin tools. `null` until the first `audit_effective_roster`
  // returns; `chipToolStates` prefers a scan's own tools and falls back to this.
  let roster = $state<AuditToolState[] | null>(null);
  let scanError = $state<string | null>(null);
  let copied = $state(false);
  let copyTimer: ReturnType<typeof setTimeout> | null = null;
  // Which category's scan is in flight (from the raw event/snapshot, whose tools
  // all share one category). Drives the cross-tab Scan lock. Null when idle.
  let activeCategory = $state<AuditCategory | null>(null);

  // ── Persisted UI state (filters) ──────────────────────────────────────────
  // These seed one-time from the (static) `view`; the initial-value read is
  // intentional.
  // svelte-ignore state_referenced_locally
  const savedSev = loadViewString(view, 'severity');
  let severity = $state<AuditSeverity>(
    savedSev && (SEVERITIES as readonly string[]).includes(savedSev) ? (savedSev as AuditSeverity) : 'note',
  );
  // svelte-ignore state_referenced_locally
  // V38: keyed by the WIRE id, so a plugin tool's toggle persists like a
  // built-in's. The stored set is no longer filtered against the built-in
  // roster — a plugin's key is not in it and would have been dropped on every
  // reload — only against emptiness, which is what a corrupted entry looks like.
  let hiddenTools = $state<Set<AuditToolRef>>(
    new Set(loadViewSet(view, 'hidden-tools').filter((t) => t.trim() !== '')),
  );
  // svelte-ignore state_referenced_locally
  let text = $state<string>(loadViewString(view, 'text') ?? '');

  // Selection is ephemeral (spec) — not persisted.
  let selected = $state<Set<string>>(new Set());

  $effect(() => saveViewString(view, 'severity', severity));
  $effect(() => saveViewSet(view, 'hidden-tools', hiddenTools));
  $effect(() => saveViewString(view, 'text', text));

  // ── Derived views ─────────────────────────────────────────────────────────
  const filters = $derived<AuditFilters>({
    severity,
    tools: Object.fromEntries([...hiddenTools].map((t) => [t, false])),
    text,
  });
  // Chips: this category's states (the snapshot's own, else the backend's
  // effective roster), then split into visible / hidden-because-not-applicable.
  const chipStates = $derived(chipToolStates(snapshot, category, roster));
  const chipPartition = $derived(partitionChips(chipStates, snapshot.census));
  const toolToggleIds = $derived(chipPartition.visible.map((t) => t.id));
  // Scan-coverage is osv-scanner-only, and osv-scanner is Security-only (V23).
  const coverage = $derived(isSecurity ? scannedArtifacts(snapshot) : []);
  const allRows = $derived(sortFindings(flattenFindings(snapshot, category)));
  const visible = $derived(visibleFindings(flattenFindings(snapshot, category), filters));
  const myTotal = $derived(categoryFindingsCount(snapshot, category));
  const selCount = $derived(selectedRows(allRows, selected).length);
  const lock = $derived(scanLock(snapshot.scanning, activeCategory, category));
  // The Graph view ⌖ jump only makes sense when the visualization is enabled
  // AND its host (the Tool Activity tab) can exist — otherwise `revealTab`
  // would silently no-op.
  const canJump = $derived(
    $settings.graph.enabled && $settings.graph.graph_viz && $settings.ui.tool_activity_tab,
  );

  // ── Live snapshot wiring ──────────────────────────────────────────────────
  let unlisten: UnlistenFn | null = null;
  let alive = true;

  // A raw (unmerged) snapshot/event: its tools all share the running scan's
  // category, so it's the reliable source for `activeCategory`.
  function noteActive(inc: AuditSnapshot): void {
    activeCategory = inc.scanning ? (inc.tools[0]?.category ?? activeCategory) : null;
  }

  /// `refreshCensus` (mount only): have the backend take the project census —
  /// bounded walk, ≤60s cache — so chip gating and quality auto-selection are
  /// live before the first scan; later pulls read the plain snapshot.
  async function pullFull(refreshCensus = false): Promise<void> {
    try {
      const full = refreshCensus ? await auditRefreshCensus() : await auditSnapshot();
      if (alive) {
        snapshot = mergeAuditSnapshot(snapshot, full);
        noteActive(full);
      }
    } catch {
      /* backend unavailable mid-teardown — keep what we have */
    }
    // V38: the PRE-SCAN roster (built-ins ∪ this project's runnable plugin
    // tools). Pulled alongside every snapshot refresh, and deliberately AFTER
    // the census one: the backend gates a plugin tool's chip on the census it
    // has stored, so asking before `auditRefreshCensus` returns would answer
    // against the previous walk. Cheap to re-ask (no walk, no resolution, no
    // spawn) and it MUST be re-asked — enabling a plugin tool or setting its
    // path happens in the Settings window, not here.
    void pullRoster();
  }

  /// The effective roster for THIS panel's category, or `null` until the first
  /// answer arrives. `chipToolStates` prefers a scan's own tool list and uses
  /// this only when there is none; there is no third source since V38 Phase E
  /// deleted the settings-derived built-ins list, so until this answers the
  /// idle chip row is empty rather than a roster the backend never confirmed.
  async function pullRoster(): Promise<void> {
    try {
      const next = await auditEffectiveRoster(category);
      if (alive) roster = next;
    } catch {
      /* leave the previous answer in place — an empty row beats a wrong one */
    }
  }

  // Keep-alive (appViews.ts): this component mounts ONCE per app lifetime, so
  // the mount-time census would go stale as the project gains/loses languages.
  // Re-take it on every hidden→visible transition of the host Tool Activity
  // tab (the audit panels live in its "Code audit" section since schema v27) —
  // fresh files (a new `.py`, a `package.json`) re-gate the chips and, in auto
  // mode, re-select the quality tools. The ≤60s backend cache makes rapid tab
  // switching free, and the backend skips the walk while a scan runs.
  const unsubShown = onAppViewShown(TOOL_ACTIVITY_TAB_ID, () => {
    void pullFull(true);
  });

  onMount(() => {
    void pullFull(true);
    void (async () => {
      const un = await listen<AuditSnapshot>('audit-status', (e) => {
        if (!alive) return;
        noteActive(e.payload);
        // A truncated event dropped findings to the per-tool cap — fetch the
        // full (uncapped) snapshot and merge THAT instead.
        if (e.payload.truncated) void pullFull();
        else snapshot = mergeAuditSnapshot(snapshot, e.payload);
      });
      if (alive) unlisten = un;
      else un();
    })();
  });

  onDestroy(() => {
    alive = false;
    unsubShown();
    if (unlisten) unlisten();
    if (copyTimer) clearTimeout(copyTimer);
  });

  // ── Actions ─────────────────────────────────────────────────────────────
  async function onScan(): Promise<void> {
    scanError = null;
    try {
      await auditStartScan(category);
      // Row ids (`tool#index`) are only stable within one scan — a retained
      // selection would silently re-target the new scan's findings.
      selected = new Set();
      // Optimistic: reflect scanning immediately; events carry the truth.
      activeCategory = category;
      snapshot = { ...snapshot, scanning: true };
    } catch (e) {
      scanError = String(e);
    }
    void pullFull();
  }

  async function onCancel(): Promise<void> {
    try {
      await auditCancelScan();
    } catch (e) {
      scanError = String(e);
    }
    void pullFull();
  }

  function toggleTool(id: AuditToolRef): void {
    const next = new Set(hiddenTools);
    if (next.has(id)) next.delete(id);
    else next.add(id);
    hiddenTools = next;
  }

  function toggleRow(id: string): void {
    selected = toggleSelected(selected, id);
  }

  function selectAll(): void {
    selected = selectAllVisible(selected, visible);
  }

  function clearSelection(): void {
    selected = deselectAll();
  }

  async function onCopy(): Promise<void> {
    const rows = selectedRows(allRows, selected);
    if (rows.length === 0) return;
    const md = formatFindingsMarkdown({
      rows,
      totalFindings: myTotal,
      root: snapshot.root,
      scannedAt: formatScanTimestamp(snapshot.last_scan_at),
      label: mdLabel,
    });
    try {
      await clipboardWriteText(md);
      copied = true;
      if (copyTimer) clearTimeout(copyTimer);
      copyTimer = setTimeout(() => (copied = false), 2000);
    } catch (e) {
      console.warn('copy audit findings to clipboard failed:', e);
    }
  }

  function jump(row: FindingRow): void {
    if (!canJump) return;
    revealFileInGraph(row.file.replace(/\\/g, '/'));
    revealTab(TOOL_ACTIVITY_TAB_ID);
  }

  function openSettings(): void {
    // Both tabs share the one Settings → Code Audit section.
    void openSettingsWindowToSection('code-audit');
  }

  const CHIP_ICON: Record<string, string> = {
    idle: '○',
    running: '',
    done: '✓',
    failed: '✗',
    'not-installed': '⚠',
    'path-invalid': '⚠',
  };
</script>

<div class="code-audit">
  <header>
    <div class="title">
      <h2>{heading}</h2>
      {#if snapshot.root}<span class="root" title={snapshot.root}>{snapshot.root}</span>{/if}
    </div>
    <div class="head-actions">
      {#if lock.waiting}
        <span class="waiting" title="One scan runs at a time across both sub-tabs">{lock.waiting}</span>
      {/if}
      <span class="scanned">
        {snapshot.last_scan_at !== null
          ? `Last scan ${formatScanTimestamp(snapshot.last_scan_at)}`
          : 'Never scanned'}
      </span>
      {#if lock.showCancel}
        <button type="button" class="btn cancel" onclick={onCancel}>Cancel</button>
      {:else}
        <button type="button" class="btn scan" onclick={onScan} disabled={lock.scanDisabled}>Scan</button>
      {/if}
    </div>
  </header>

  {#if scanError}
    <div class="scan-error">{scanError}</div>
  {/if}

  <!-- Per-tool status chips (only tools applicable to this project once the
       census is known). Tooltip: the tool's error for failed/not-installed,
       else the resolved binary path for done/running chips. -->
  <div class="chips">
    {#each chipPartition.visible as t (t.id)}
      {@const c = toolChip(t)}
      <span class="chip {c.kind}" title={c.tooltip ?? t.resolved ?? ''}>
        <span class="chip-name">{t.id}</span>
        {#if c.spinner}<span class="spinner" aria-hidden="true"></span>{/if}
        {#if CHIP_ICON[c.kind]}<span class="chip-icon">{CHIP_ICON[c.kind]}</span>{/if}
        <span class="chip-label">{c.label}</span>
        {#if c.kind === 'not-installed' || c.kind === 'path-invalid'}
          <button type="button" class="chip-link" onclick={openSettings}>Settings</button>
        {/if}
      </span>
    {/each}
  </div>

  {#if chipPartition.hiddenCount > 0}
    <div class="hidden-tools">
      {chipPartition.hiddenCount} tool{chipPartition.hiddenCount === 1 ? '' : 's'} hidden — not
      applicable to this project
    </div>
  {/if}

  <!-- Scan-coverage honesty line: what osv-scanner reported actually scanning,
       so a "0 findings" run over an unscannable ecosystem isn't read as clean. -->
  {#if coverage.length > 0}
    <div class="coverage" title="Lockfiles / manifests osv-scanner reported scanning this run">
      <span class="coverage-label">Scanned</span>
      {formatCoverageLine(coverage)}
    </div>
  {/if}

  <!-- Filters + selection actions -->
  <div class="controls">
    <label class="ctl">
      Severity ≥
      <select bind:value={severity}>
        {#each SEVERITIES as s (s)}<option value={s}>{s}</option>{/each}
      </select>
    </label>
    <div class="ctl tool-toggles">
      {#each toolToggleIds as id (id)}
        <label class="tog">
          <input type="checkbox" checked={!hiddenTools.has(id)} onchange={() => toggleTool(id)} />
          {id}
        </label>
      {/each}
    </div>
    <input class="ctl filter-text" type="text" placeholder="Filter…" bind:value={text} />
    <div class="sel-actions">
      <button type="button" class="btn ghost" onclick={selectAll} disabled={visible.length === 0}>Select all</button>
      <button type="button" class="btn ghost" onclick={clearSelection} disabled={selCount === 0}>Deselect all</button>
      <button type="button" class="btn ghost" onclick={onCopy} disabled={selCount === 0}>
        {copied ? 'Copied ✓' : `Copy selected (${selCount})`}
      </button>
    </div>
  </div>

  <div class="count-line">
    Showing {visible.length} of {myTotal} finding{myTotal === 1 ? '' : 's'}
  </div>

  <!-- Findings table -->
  {#if visible.length === 0}
    <div class="empty">
      {snapshot.scanning
        ? 'Scanning…'
        : snapshot.last_scan_at === null
          ? 'No scan yet — press Scan to run the enabled tools.'
          : 'No findings match the current filters.'}
    </div>
  {:else}
    <div class="table" role="table">
      <div class="trow thead" role="row">
        <span class="c-sel" role="columnheader"></span>
        <span class="c-sev" role="columnheader">severity</span>
        <span class="c-tool" role="columnheader">tool</span>
        <span class="c-rule" role="columnheader">rule</span>
        <span class="c-loc" role="columnheader">file:line</span>
        <span class="c-msg" role="columnheader">message</span>
      </div>
      {#each visible as row (row.id)}
        <div class="trow" class:selected={selected.has(row.id)} role="row">
          <span class="c-sel" role="cell">
            <input
              type="checkbox"
              checked={selected.has(row.id)}
              onchange={() => toggleRow(row.id)}
              aria-label="Select finding"
            />
          </span>
          <span class="c-sev sev-{row.severity}" role="cell">{row.severity}</span>
          <span class="c-tool" role="cell">{row.tool}</span>
          <span class="c-rule" role="cell" title={row.code ?? ''}>{row.code ?? '—'}</span>
          <span class="c-loc" role="cell">
            {#if canJump}
              <button
                type="button"
                class="loc-jump"
                title="Reveal in Graph view (Tools)"
                onclick={() => jump(row)}
              >{row.file}{row.line > 0 ? `:${row.line}` : ''}</button>
            {:else}
              <span title={row.file}>{row.file}{row.line > 0 ? `:${row.line}` : ''}</span>
            {/if}
          </span>
          <span class="c-msg" role="cell" title={row.message}>{row.message}</span>
        </div>
      {/each}
    </div>
  {/if}

  {#if isSecurity}
    <!-- Network reality: these tools need the network, and a bare offline failure
         is opaque — the failed chip's tooltip carries the tool's own stderr. -->
    <p class="net-hint">
      Scans need network access — osv-scanner queries the OSV API / deps.dev, and
      semgrep downloads its rules on first run. Offline runs degrade; a failed
      tool's chip tooltip shows its own error.
    </p>
  {:else}
    <p class="net-hint">
      Most quality linters run offline. semgrep (quality rules) downloads its
      ruleset on first run, and ESLint / knip use your project's
      <code>node_modules</code>. A failed tool's chip tooltip shows its own error.
    </p>
  {/if}
</div>

<style>
  .code-audit {
    position: absolute;
    inset: 0;
    overflow-y: auto;
    padding: 16px;
    font-size: 13px;
    color: var(--text-primary, #ddd);
    box-sizing: border-box;
  }
  header {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 12px;
    margin-bottom: 12px;
    flex-wrap: wrap;
  }
  .title {
    display: flex;
    align-items: baseline;
    gap: 10px;
    min-width: 0;
  }
  /* Subsection size — the enclosing CodeAuditView's "Code Audit" h2 (15px)
     is the tab title; this heading names the active sub-tab's panel. */
  .title h2 {
    margin: 0;
    font-size: 14px;
  }
  .root {
    font-size: 11px;
    opacity: 0.6;
    font-family: var(--font-mono, monospace);
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
    max-width: 40vw;
  }
  .head-actions {
    display: flex;
    align-items: center;
    gap: 10px;
  }
  .scanned {
    font-size: 11px;
    opacity: 0.6;
  }
  .waiting {
    font-size: 11px;
    opacity: 0.7;
    color: var(--text-warning, #e3b341);
    font-style: italic;
  }
  .btn {
    border: 1px solid var(--border-subtle, #3a3a3a);
    border-radius: 6px;
    background: transparent;
    color: var(--text-primary, #ddd);
    font-size: 12px;
    padding: 3px 12px;
    cursor: pointer;
  }
  .btn:hover:not(:disabled) {
    background: rgba(255, 255, 255, 0.06);
  }
  .btn:disabled {
    opacity: 0.4;
    cursor: default;
  }
  .btn.scan {
    background: var(--accent, #3b6ea5);
    color: var(--accent-fg, #fff);
    border-color: var(--accent, #3b6ea5);
  }
  .btn.cancel {
    color: var(--text-danger-soft, #ffb4ab);
    border-color: var(--border-danger-soft, rgba(255, 180, 171, 0.5));
  }
  .scan-error {
    border: 1px solid var(--border-danger-soft, rgba(255, 180, 171, 0.5));
    background: var(--surface-danger-bg, rgba(255, 180, 171, 0.08));
    color: var(--text-danger-soft, #ffb4ab);
    border-radius: 6px;
    padding: 6px 10px;
    margin-bottom: 10px;
    font-size: 12px;
  }
  .chips {
    display: flex;
    flex-wrap: wrap;
    gap: 8px;
    margin-bottom: 12px;
  }
  .chip {
    display: inline-flex;
    align-items: center;
    gap: 6px;
    border: 1px solid var(--border-subtle, #3a3a3a);
    border-radius: 14px;
    padding: 2px 10px;
    font-size: 12px;
    background: var(--surface-card, #1e1e1e);
  }
  .chip-name {
    font-weight: 600;
  }
  .chip-label {
    opacity: 0.8;
  }
  .chip.done {
    border-color: color-mix(in srgb, var(--success, #3fb950) 50%, transparent);
  }
  .chip.done .chip-icon {
    color: var(--text-success, #3fb950);
  }
  .chip.failed {
    border-color: var(--border-danger-soft, rgba(255, 180, 171, 0.5));
  }
  .chip.failed .chip-icon,
  .chip.failed .chip-label {
    color: var(--text-danger-soft, #ffb4ab);
  }
  .chip.not-installed,
  .chip.path-invalid {
    border-color: var(--border-warning, rgba(227, 179, 65, 0.5));
  }
  .chip.not-installed .chip-icon,
  .chip.not-installed .chip-label {
    color: var(--text-warning, #e3b341);
  }
  /* A configured-but-broken path is a user error, not a missing install —
     tint it toward the failed red so it reads as "fix me". */
  .chip.path-invalid .chip-icon,
  .chip.path-invalid .chip-label {
    color: var(--text-danger-soft, #ffb4ab);
  }
  .chip.running .chip-name {
    color: var(--text-info, #58a6ff);
  }
  .chip-link {
    border: none;
    background: transparent;
    color: var(--accent, #58a6ff);
    cursor: pointer;
    font-size: 11px;
    text-decoration: underline;
    padding: 0;
  }
  .hidden-tools {
    font-size: 11px;
    opacity: 0.55;
    margin: -4px 0 12px;
    font-style: italic;
  }
  .spinner {
    width: 10px;
    height: 10px;
    border: 2px solid var(--border-default, rgba(255, 255, 255, 0.25));
    border-top-color: var(--accent, #58a6ff);
    border-radius: 50%;
    animation: spin 0.8s linear infinite;
  }
  @keyframes spin {
    to {
      transform: rotate(360deg);
    }
  }
  .controls {
    display: flex;
    align-items: center;
    flex-wrap: wrap;
    gap: 12px;
    margin-bottom: 8px;
    padding-bottom: 8px;
    border-bottom: 1px solid var(--border-subtle, #333);
  }
  .ctl {
    display: inline-flex;
    align-items: center;
    gap: 6px;
    font-size: 12px;
  }
  select,
  input[type='text'] {
    background: var(--surface-input, #1e1e1e);
    color: var(--text-primary, #ddd);
    border: 1px solid var(--border-subtle, #3a3a3a);
    border-radius: 5px;
    padding: 2px 6px;
    font-size: 12px;
  }
  .tool-toggles {
    gap: 10px;
    flex-wrap: wrap;
  }
  .tog {
    display: inline-flex;
    align-items: center;
    gap: 4px;
    opacity: 0.85;
  }
  .filter-text {
    min-width: 140px;
  }
  .sel-actions {
    display: flex;
    gap: 6px;
    margin-left: auto;
  }
  .coverage {
    font-size: 11px;
    opacity: 0.7;
    margin: -4px 0 12px;
    font-family: var(--font-mono, monospace);
    display: flex;
    flex-wrap: wrap;
    gap: 6px;
    align-items: baseline;
  }
  .coverage-label {
    font-family: inherit;
    text-transform: uppercase;
    letter-spacing: 0.04em;
    font-size: 0.82em;
    opacity: 0.7;
    font-weight: 600;
  }
  .count-line {
    font-size: 11px;
    opacity: 0.6;
    margin-bottom: 6px;
  }
  .net-hint {
    font-size: 11px;
    opacity: 0.55;
    line-height: 1.5;
    margin: 16px 0 0;
    max-width: 60ch;
  }
  .empty {
    opacity: 0.6;
    font-style: italic;
    padding: 16px 4px;
  }
  .table {
    display: flex;
    flex-direction: column;
    border: 1px solid var(--border-faint, #2a2a2a);
    border-radius: 8px;
    overflow: hidden;
  }
  .trow {
    display: grid;
    grid-template-columns: 2rem 5rem 6.5rem 12rem minmax(10rem, 1fr) minmax(12rem, 2fr);
    align-items: center;
    gap: 8px;
    padding: 4px 8px;
    border-bottom: 1px solid var(--border-faint, #2a2a2a);
    font-size: 0.9em;
  }
  .trow:last-child {
    border-bottom: none;
  }
  .thead {
    font-size: 0.78em;
    text-transform: uppercase;
    letter-spacing: 0.04em;
    opacity: 0.6;
    background: rgba(255, 255, 255, 0.03);
  }
  .trow.selected {
    background: var(--accent-muted, rgba(88, 166, 255, 0.1));
  }
  .trow:hover:not(.thead) {
    background: rgba(255, 255, 255, 0.04);
  }
  .c-sev {
    font-weight: 600;
    text-transform: uppercase;
    font-size: 0.82em;
  }
  .sev-error {
    color: var(--text-danger-soft, #ffb4ab);
  }
  .sev-warning {
    color: var(--text-warning, #e3b341);
  }
  .sev-note {
    color: var(--text-info, #58a6ff);
  }
  .c-tool,
  .c-rule,
  .c-loc,
  .c-msg {
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .c-rule {
    font-family: var(--font-mono, monospace);
    opacity: 0.85;
  }
  .loc-jump {
    border: none;
    background: transparent;
    color: var(--accent, #58a6ff);
    cursor: pointer;
    font-family: var(--font-mono, monospace);
    font-size: 1em;
    padding: 0;
    text-align: left;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
    max-width: 100%;
  }
  .loc-jump:hover {
    text-decoration: underline;
  }
</style>
