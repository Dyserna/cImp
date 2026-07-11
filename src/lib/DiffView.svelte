<script lang="ts">
  // V13 Phase B4 — the live diff pane (`WorkbenchView`'s "Diff" section).
  // File list comes from the shared `workbenchDiff` store (kept fresh by
  // `WorkbenchDiffBadge`'s fs-batch listener + fallback poll — see
  // workbenchDiff.ts); this component only fetches the FULL parsed diff
  // (`workbench_diff_file`) for files the user actually expands, and refetches
  // those whenever a new summary lands (a cheap re-diff per expanded file,
  // not a rebuild of the whole view).
  import { onMount, onDestroy } from 'svelte';
  import { SvelteMap, SvelteSet } from 'svelte/reactivity';
  import { writeText as clipboardWriteText } from '@tauri-apps/plugin-clipboard-manager';
  import {
    workbenchDiffFile,
    workbenchRevertHunk,
    workbenchSendHunk,
    FULL_FILE_CONTEXT,
    type FileStatus,
    type Hunk,
    type FileDiff,
  } from './workbench';
  import { workbenchDiff, workbenchDiffError, watchWorkbenchDiff, refreshWorkbenchDiffNow } from './workbenchDiff';
  import { pairHunkLines, wordDiff } from './diffWords';
  import { openComposeWith } from './composeState';
  import { settings } from './settings/store';
  import { revealFileInGraph } from './graphReveal';
  import { revealTab } from './tabs/visibility';
  import { GRAPH_VIEW_TAB_ID } from './tabs/types';
  import { graphVizFileStatus, onGraphStatus, type VizFileStatus } from './graph';
  import type { UnlistenFn } from '@tauri-apps/api/event';

  // SvelteSet/SvelteMap, NOT plain Set/Map in $state: Svelte 5's proxy only
  // deep-proxies plain objects/arrays, so in-place .add()/.set() on a plain
  // collection triggers no re-render — expanding a file would only become
  // visible whenever the next summary refresh happened to re-render the list.
  const expanded = new SvelteSet<string>();
  const fileDiffs = new SvelteMap<string, FileDiff>();
  const fileErrors = new SvelteMap<string, string>();
  const revertErrors = new SvelteMap<string, string>();
  let viewMode = $state<'unified' | 'side-by-side'>('unified');
  // Files showing the FULL-file view (huge unified context — the whole file
  // as one hunk, change highlighting intact). Hunk actions that echo an
  // index/hash back to the backend (Send to agent, Revert) are hidden in this
  // mode: the backend re-derives the diff at the default context, so a
  // full-context hunk's index/hash would never match.
  const fullView = new SvelteSet<string>();

  let release: (() => void) | null = null;
  let unsubGraphStatus: UnlistenFn | undefined;
  onMount(() => {
    release = watchWorkbenchDiff();
    void refreshWorkbenchDiffNow();
    // Statuses only move when the graph re-indexes, so refresh them on each
    // completed index pass (the fs-watcher re-indexes exactly the files that
    // change) rather than on every diff-summary tick.
    void onGraphStatus((s) => {
      if (s.state !== 'ready') return;
      void loadGraphStatuses(lastStatusPaths, true);
    }).then((un) => (unsubGraphStatus = un));
  });
  onDestroy(() => {
    release?.();
    release = null;
    unsubGraphStatus?.();
    unsubGraphStatus = undefined;
  });

  // ── ⌖ button state — per-file graph presence ─────────────────────────────
  // The jump button disables for files the graph can't show: not indexed at
  // all, or indexed with zero rolled-up import/call edges (degree-0 files are
  // never in the viz snapshot). While a status is unknown (fetch pending or
  // failed) the button stays enabled and the Graph View's own reveal-miss
  // notice is the fallback.
  const graphStatuses = new SvelteMap<string, VizFileStatus>();
  let statusSeq = 0;
  let lastStatusKey = '';
  let lastStatusPaths: string[] = [];
  async function loadGraphStatuses(paths: string[], force = false): Promise<void> {
    const key = paths.join('\n');
    if (!force && key === lastStatusKey) return; // same visible list — nothing moved
    lastStatusKey = key;
    lastStatusPaths = paths;
    if (paths.length === 0) {
      graphStatuses.clear();
      return;
    }
    const seq = ++statusSeq;
    try {
      const rows = await graphVizFileStatus(paths);
      if (seq !== statusSeq) return; // a newer fetch superseded this one
      graphStatuses.clear();
      for (const r of rows) graphStatuses.set(r.path, r);
    } catch (e) {
      // Keep whatever statuses we had — unknown files fall back to enabled.
      console.warn('graph_viz_file_status failed:', e);
    }
  }
  $effect(() => {
    if (!$settings.graph.enabled || !$settings.graph.graph_viz) return;
    const files = $workbenchDiff?.files ?? [];
    void loadGraphStatuses(files.slice(0, MAX_FILE_ROWS).map((f) => f.path));
  });

  // Per-path fetch tokens: successive summaries can put two
  // `workbench_diff_file` calls for one path in flight (edit → revert in
  // quick succession), and without this the FIRST (stale) response resolving
  // last would clobber the fresher diff — same out-of-order guard the
  // summary store's `refreshSeq` applies at its level.
  const loadSeq = new Map<string, number>();
  async function loadFile(path: string): Promise<void> {
    const seq = (loadSeq.get(path) ?? 0) + 1;
    loadSeq.set(path, seq);
    try {
      const fd = await workbenchDiffFile(path, fullView.has(path) ? FULL_FILE_CONTEXT : undefined);
      if (loadSeq.get(path) !== seq) return; // a newer fetch superseded this one
      fileDiffs.set(path, fd);
      fileErrors.delete(path);
    } catch (e) {
      if (loadSeq.get(path) !== seq) return;
      fileErrors.set(path, String(e));
      console.warn('workbench_diff_file failed:', e);
    }
  }

  // Refetch every currently-expanded file whenever a fresh summary lands
  // (fs-batch / poll) — simpler and more robust than trying to infer exactly
  // which expanded files the batch touched, and a per-file `git diff` is
  // cheap. Also fires when the user expands/collapses a row (reading
  // `expanded` below makes it a tracked dependency), which is what actually
  // fetches a newly-expanded file the first time.
  $effect(() => {
    if (!$workbenchDiff) return;
    for (const path of expanded) void loadFile(path);
  });

  function toggleExpand(path: string): void {
    if (expanded.has(path)) expanded.delete(path);
    else expanded.add(path);
  }

  function setFull(path: string, full: boolean): void {
    if (full === fullView.has(path)) return;
    if (full) fullView.add(path);
    else fullView.delete(path);
    // The refetch-expanded $effect re-runs anyway (it tracks `fullView` via
    // loadFile's synchronous read), but call directly so a not-yet-expanded
    // file still can't end up stale.
    void loadFile(path);
  }

  function statusLabel(s: FileStatus): string {
    switch (s.kind) {
      case 'Modified': return 'M';
      case 'Added': return 'A';
      case 'Deleted': return 'D';
      case 'Renamed': return 'R';
      case 'Untracked': return 'U';
    }
  }

  function statusTitle(s: FileStatus): string {
    switch (s.kind) {
      case 'Modified': return 'Modified';
      case 'Added': return 'Added';
      case 'Deleted': return 'Deleted';
      case 'Renamed': return `Renamed from ${s.from}`;
      case 'Untracked': return 'Untracked (not yet added to git)';
    }
  }

  function copyHunk(hunk: Hunk): void {
    const text = hunk.lines.map(([m, t]) => `${m}${t}`).join('\n');
    void clipboardWriteText(text).catch((e) => console.warn('copy hunk to clipboard failed:', e));
  }

  async function sendHunk(path: string, hunkIndex: number): Promise<void> {
    try {
      const text = await workbenchSendHunk(path, hunkIndex);
      openComposeWith(text);
    } catch (e) {
      console.error('workbench_send_hunk failed:', e);
    }
  }

  // Revert requires a confirm dialog above 20 lines (the milestone's
  // threshold for "this is big enough that a misclick would hurt") — below
  // that a hunk revert is a one-click, easily-redone-by-hand action.
  const REVERT_CONFIRM_LINES = 20;

  // The file list renders plain rows (no windowing), so cap it: a pathological
  // change set (first snapshot of a vendored tree, say) would otherwise mount
  // tens of thousands of DOM rows at once and freeze the webview. Per-file
  // CONTENT is already capped backend-side (1 MiB); this bounds the row count,
  // with an explicit "showing first N" notice so truncation is never silent.
  const MAX_FILE_ROWS = 500;

  async function doRevert(path: string, hunkIndex: number, hunk: Hunk, status: FileStatus): Promise<void> {
    if (status.kind === 'Untracked') {
      // An untracked file's "diff" is one synthesized whole-file hunk against
      // /dev/null, so reverting it DELETES the file — and the content exists
      // nowhere in git, so it's unrecoverable. Always confirm, in delete
      // terms, regardless of the size threshold below.
      if (!confirm(`'${path}' is untracked — reverting deletes the whole file from disk, and it isn't in git so it can't be recovered. Delete it?`)) {
        return;
      }
    } else if (hunk.lines.length > REVERT_CONFIRM_LINES) {
      if (!confirm(`Revert this ${hunk.lines.length}-line hunk in ${path}? This edits the file on disk immediately.`)) {
        return;
      }
    }
    const key = `${path}:${hunkIndex}`;
    try {
      const fresh = await workbenchRevertHunk(path, hunkIndex, hunk.hash);
      fileDiffs.set(path, fresh);
      revertErrors.delete(key);
      void refreshWorkbenchDiffNow();
    } catch (e) {
      revertErrors.set(key, String(e));
      console.error('workbench_revert_hunk failed:', e);
    }
  }

  // Jump to this file's node in the Graph View tab, as if it were clicked
  // there. The button only renders when the settings-gated Graph View tab
  // can exist, so revealTab never silently no-ops.
  function jumpToGraph(path: string): void {
    revealFileInGraph(path);
    revealTab(GRAPH_VIEW_TAB_ID);
  }
</script>

<div class="diff-view">
  {#if $workbenchDiffError && !$workbenchDiff}
    <p class="msg err">Couldn't load the diff: {$workbenchDiffError}</p>
  {:else if !$workbenchDiff}
    <p class="msg">Loading…</p>
  {:else}
    {@const summary = $workbenchDiff}
    <div class="toolbar">
      <span class="count">
        {summary.files.length === 0 ? 'No changes' : `${summary.files.length} file${summary.files.length === 1 ? '' : 's'} changed`}
      </span>
      {#if $workbenchDiffError}
        <span class="stale-note" title={$workbenchDiffError}>
          refresh failed — showing the last good diff
        </span>
      {/if}
      {#if summary.readonly}
        <span class="readonly-note" title="A merge or rebase is in progress — hunk reverts are disabled until it's resolved.">
          read-only (merge/rebase in progress)
        </span>
      {/if}
      <div class="view-toggle" role="group" aria-label="Diff layout">
        <button type="button" class:active={viewMode === 'unified'} onclick={() => (viewMode = 'unified')}>Unified</button>
        <button type="button" class:active={viewMode === 'side-by-side'} onclick={() => (viewMode = 'side-by-side')}>Side-by-side</button>
      </div>
    </div>

    <div class="file-list">
      {#if summary.files.length > MAX_FILE_ROWS}
        <p class="msg">Showing the first {MAX_FILE_ROWS} of {summary.files.length} changed files.</p>
      {/if}
      {#each summary.files.slice(0, MAX_FILE_ROWS) as f (f.path)}
        <div class="file-row">
          <!-- The expand toggle is itself a <button>, so the graph-jump
               button must be a SIBLING (nested interactive elements are
               invalid); this flex wrapper keeps them on one visual row. -->
          <div class="file-row-head">
            <button
              type="button"
              class="file-header"
              onclick={() => toggleExpand(f.path)}
              aria-expanded={expanded.has(f.path) && !f.binary && !f.too_large}
              disabled={f.binary || f.too_large}
            >
              <span class="chevron" aria-hidden="true">{expanded.has(f.path) ? '▾' : '▸'}</span>
              <span class="status-chip status-{f.status.kind.toLowerCase()}" title={statusTitle(f.status)}>{statusLabel(f.status)}</span>
              <span class="path">{f.path}</span>
              {#if f.status.kind === 'Renamed'}<span class="from-path">← {f.status.from}</span>{/if}
              {#if f.binary}<span class="tag">binary</span>{/if}
              {#if f.too_large}<span class="tag">too large</span>{/if}
              {#if !f.binary && !f.too_large && (f.added > 0 || f.removed > 0)}
                <span class="counts">
                  {#if f.added > 0}<span class="added">+{f.added}</span>{/if}
                  {#if f.removed > 0}<span class="removed">-{f.removed}</span>{/if}
                </span>
              {/if}
            </button>
            {#if $settings.graph.enabled && $settings.graph.graph_viz}
              {@const gs = graphStatuses.get(f.path)}
              <button
                type="button"
                class="graph-jump"
                title={gs && !gs.indexed
                  ? 'Not in the code graph (file isn’t indexed)'
                  : gs && gs.degree === 0
                    ? 'No imports or calls in the code graph'
                    : 'Show in Graph View'}
                aria-label="Show {f.path} in Graph View"
                disabled={gs !== undefined && (!gs.indexed || gs.degree === 0)}
                onclick={() => jumpToGraph(f.path)}
              >⌖</button>
            {/if}
          </div>

          {#if expanded.has(f.path) && !f.binary && !f.too_large}
            {@const fd = fileDiffs.get(f.path)}
            {@const err = fileErrors.get(f.path)}
            {@const full = fullView.has(f.path)}
            <div class="file-body">
              <div class="body-toolbar">
                <div class="view-toggle" role="group" aria-label="Diff or full file">
                  <button type="button" class:active={!full} onclick={() => setFull(f.path, false)}>Diff</button>
                  <button type="button" class:active={full} onclick={() => setFull(f.path, true)}>Full file</button>
                </div>
              </div>
              {#if err}
                <p class="msg err">{err}</p>
              {:else if !fd}
                <p class="msg">Loading…</p>
              {:else if fd.hunks.length === 0}
                <p class="msg">No hunks (already clean).</p>
              {:else}
                {#each fd.hunks as hunk, hunkIndex (hunkIndex)}
                  <div class="hunk">
                    <div class="hunk-toolbar">
                      <span class="hunk-header">{hunk.header}</span>
                      <span class="hunk-actions">
                        <button type="button" onclick={() => copyHunk(hunk)}>Copy</button>
                        {#if !full}
                          <button type="button" onclick={() => void sendHunk(f.path, hunkIndex)}>Send to agent</button>
                          <button
                            type="button"
                            class="revert"
                            disabled={summary.readonly}
                            title={summary.readonly ? 'Disabled — a merge or rebase is in progress' : 'Revert this hunk'}
                            onclick={() => void doRevert(f.path, hunkIndex, hunk, f.status)}
                          >Revert</button>
                        {/if}
                      </span>
                    </div>
                    {#if revertErrors.get(`${f.path}:${hunkIndex}`)}
                      <p class="msg err">{revertErrors.get(`${f.path}:${hunkIndex}`)}</p>
                    {/if}

                    {#if viewMode === 'unified'}
                      <div class="hunk-body unified">
                        {#each pairHunkLines(hunk.lines) as group, gi (gi)}
                          {#if group.type === 'ctx'}
                            <div class="line ctx"><span class="marker"> </span><span class="text">{group.text}</span></div>
                          {:else if group.type === 'del'}
                            <div class="line del"><span class="marker">-</span><span class="text">{group.text}</span></div>
                          {:else if group.type === 'add'}
                            <div class="line add"><span class="marker">+</span><span class="text">{group.text}</span></div>
                          {:else}
                            {@const wd = wordDiff(group.oldText, group.newText)}
                            <div class="line del">
                              <span class="marker">-</span><span class="text">{#each wd.left as p, pi (pi)}<span class:hl={p.kind === 'del'}>{p.text}</span>{/each}</span>
                            </div>
                            <div class="line add">
                              <span class="marker">+</span><span class="text">{#each wd.right as p, pi (pi)}<span class:hl={p.kind === 'add'}>{p.text}</span>{/each}</span>
                            </div>
                          {/if}
                        {/each}
                      </div>
                    {:else}
                      <div class="hunk-body side-by-side">
                        {#each pairHunkLines(hunk.lines) as group, gi (gi)}
                          <div class="sbs-row">
                            <div class="sbs-col" class:empty={group.type === 'add'}>
                              {#if group.type === 'ctx'}
                                <span class="text">{group.text}</span>
                              {:else if group.type === 'del'}
                                <span class="text del">{group.text}</span>
                              {:else if group.type === 'pair'}
                                {@const wd = wordDiff(group.oldText, group.newText)}
                                <span class="text del">{#each wd.left as p, pi (pi)}<span class:hl={p.kind === 'del'}>{p.text}</span>{/each}</span>
                              {/if}
                            </div>
                            <div class="sbs-col" class:empty={group.type === 'del'}>
                              {#if group.type === 'ctx'}
                                <span class="text">{group.text}</span>
                              {:else if group.type === 'add'}
                                <span class="text add">{group.text}</span>
                              {:else if group.type === 'pair'}
                                {@const wd = wordDiff(group.oldText, group.newText)}
                                <span class="text add">{#each wd.right as p, pi (pi)}<span class:hl={p.kind === 'add'}>{p.text}</span>{/each}</span>
                              {/if}
                            </div>
                          </div>
                        {/each}
                      </div>
                    {/if}
                  </div>
                {/each}
              {/if}
            </div>
          {/if}
        </div>
      {/each}
    </div>
  {/if}
</div>

<style>
  .diff-view {
    display: flex;
    flex-direction: column;
    gap: var(--space-2);
    font-size: var(--font-size-sm);
  }
  .msg {
    opacity: 0.7;
    font-style: italic;
    padding: var(--space-2) 0;
  }
  .msg.err {
    color: var(--text-danger-soft);
    font-style: normal;
  }
  .toolbar {
    display: flex;
    align-items: center;
    gap: var(--space-3);
    flex-wrap: wrap;
  }
  .count {
    color: var(--text-secondary);
  }
  .readonly-note {
    color: var(--text-warning);
    font-size: var(--font-size-xs);
  }
  .stale-note {
    color: var(--text-warning);
    font-size: var(--font-size-xs);
  }
  .view-toggle {
    display: inline-flex;
    gap: 2px;
    margin-left: auto;
  }
  .view-toggle button {
    appearance: none;
    background: transparent;
    border: 1px solid var(--border-subtle);
    color: var(--text-secondary);
    border-radius: var(--radius-sm);
    padding: 2px 8px;
    font-size: var(--font-size-xs);
    cursor: pointer;
  }
  .view-toggle button.active {
    background: var(--accent-muted);
    border-color: var(--accent);
    color: var(--text-primary);
  }

  .file-list {
    display: flex;
    flex-direction: column;
    gap: 1px;
  }
  .file-row {
    border: 1px solid var(--border-subtle);
    border-radius: var(--radius-md);
    overflow: hidden;
  }
  .file-row-head {
    display: flex;
    align-items: stretch;
    background: var(--surface-2);
  }
  .graph-jump {
    appearance: none;
    flex: 0 0 auto;
    background: transparent;
    border: none;
    border-left: 1px solid var(--border-subtle);
    color: var(--text-secondary);
    padding: 0 10px;
    font-size: var(--font-size-md);
    cursor: pointer;
  }
  .graph-jump:hover:not(:disabled) {
    background: var(--surface-3);
    color: var(--text-primary);
  }
  .graph-jump:disabled {
    opacity: 0.35;
    cursor: default;
  }
  .file-header {
    appearance: none;
    flex: 1 1 auto;
    min-width: 0;
    display: flex;
    align-items: center;
    gap: var(--space-2);
    background: var(--surface-2);
    border: none;
    color: var(--text-primary);
    padding: 6px var(--space-2);
    text-align: left;
    cursor: pointer;
    font-family: inherit;
    font-size: inherit;
  }
  .file-header:disabled {
    cursor: default;
    opacity: 0.75;
  }
  .file-header:not(:disabled):hover {
    background: var(--surface-3);
  }
  .chevron {
    width: 1em;
    flex: 0 0 auto;
    opacity: 0.6;
  }
  .status-chip {
    flex: 0 0 auto;
    display: inline-flex;
    align-items: center;
    justify-content: center;
    width: 16px;
    height: 16px;
    border-radius: var(--radius-sm);
    font-size: 10px;
    font-weight: var(--font-weight-semibold);
  }
  .status-chip.status-modified { background: var(--surface-warning); color: var(--text-warning); }
  .status-chip.status-added    { background: var(--surface-success); color: var(--text-success); }
  .status-chip.status-deleted  { background: var(--surface-danger); color: var(--text-danger-soft); }
  .status-chip.status-renamed  { background: var(--surface-info); color: var(--text-info); }
  .status-chip.status-untracked{ background: var(--surface-3); color: var(--text-secondary); }
  .path {
    flex: 1 1 auto;
    min-width: 0;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
    font-family: 'SF Mono', 'Cascadia Code', Consolas, monospace;
  }
  .from-path {
    flex: 0 0 auto;
    color: var(--text-tertiary);
    font-size: var(--font-size-xs);
  }
  .tag {
    flex: 0 0 auto;
    font-size: var(--font-size-xs);
    color: var(--text-tertiary);
    border: 1px solid var(--border-subtle);
    border-radius: var(--radius-sm);
    padding: 0 4px;
  }
  .counts {
    flex: 0 0 auto;
    font-variant-numeric: tabular-nums;
    font-size: var(--font-size-xs);
  }
  .counts .added { color: var(--text-success); margin-right: 4px; }
  .counts .removed { color: var(--text-danger-soft); }

  .file-body {
    background: var(--surface-sunken);
    padding: var(--space-2);
  }
  .body-toolbar {
    display: flex;
    justify-content: flex-end;
    margin-bottom: 4px;
  }
  .body-toolbar .view-toggle {
    margin-left: 0;
  }

  .hunk + .hunk {
    margin-top: var(--space-3);
  }
  .hunk-toolbar {
    display: flex;
    align-items: center;
    gap: var(--space-2);
    margin-bottom: 4px;
  }
  .hunk-header {
    flex: 1 1 auto;
    min-width: 0;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
    color: var(--text-tertiary);
    font-family: 'SF Mono', 'Cascadia Code', Consolas, monospace;
    font-size: var(--font-size-xs);
  }
  .hunk-actions {
    flex: 0 0 auto;
    display: inline-flex;
    gap: 4px;
  }
  .hunk-actions button {
    appearance: none;
    background: transparent;
    border: 1px solid var(--border-subtle);
    color: var(--text-secondary);
    border-radius: var(--radius-sm);
    padding: 2px 6px;
    font-size: var(--font-size-xs);
    cursor: pointer;
  }
  .hunk-actions button:hover:not(:disabled) {
    background: var(--surface-3);
    color: var(--text-primary);
  }
  .hunk-actions button.revert:not(:disabled) {
    border-color: var(--border-danger);
    color: var(--text-danger-soft);
  }
  .hunk-actions button:disabled {
    opacity: 0.4;
    cursor: not-allowed;
  }

  .hunk-body {
    font-family: 'SF Mono', 'Cascadia Code', Consolas, monospace;
    font-size: var(--font-size-xs);
    line-height: var(--line-height-tight);
    border-radius: var(--radius-sm);
    overflow: hidden;
    border: 1px solid var(--border-faint);
  }
  .line {
    display: flex;
    gap: 6px;
    padding: 0 6px;
    white-space: pre-wrap;
    word-break: break-all;
  }
  .line .marker {
    flex: 0 0 auto;
    opacity: 0.5;
    user-select: none;
  }
  .line.ctx { color: var(--text-secondary); }
  .line.del { background: var(--surface-danger-faint); color: var(--text-danger-pastel); }
  .line.add { background: var(--surface-success); color: var(--text-success); }
  .line .text :global(span.hl) {
    background: rgba(255, 255, 255, 0.18);
    border-radius: 2px;
  }

  .side-by-side .sbs-row {
    display: grid;
    grid-template-columns: 1fr 1fr;
  }
  .side-by-side .sbs-col {
    padding: 0 6px;
    white-space: pre-wrap;
    word-break: break-all;
    border-left: 1px solid var(--border-faint);
  }
  .side-by-side .sbs-col:first-child {
    border-left: none;
  }
  .side-by-side .sbs-col.empty {
    background: var(--surface-deep);
  }
  .side-by-side .text.del { background: var(--surface-danger-faint); color: var(--text-danger-pastel); }
  .side-by-side .text.add { background: var(--surface-success); color: var(--text-success); }
  .side-by-side .text :global(span.hl) {
    background: rgba(255, 255, 255, 0.18);
    border-radius: 2px;
  }
</style>
