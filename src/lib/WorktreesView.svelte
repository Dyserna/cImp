<script lang="ts">
  // V13 Phase D D3 — the Worktrees section (`WorkbenchView`'s 'worktrees'
  // segment). One row per cImp-managed worktree: slug · branch · ahead/behind
  // vs its base · a live-tab indicator · row actions (Diff / Run checks /
  // Merge / Discard / Open shell here).
  import { onMount } from 'svelte';
  import { get } from 'svelte/store';
  import { SvelteMap, SvelteSet } from 'svelte/reactivity';
  import {
    workbenchWorktrees,
    workbenchWorktreeDiff,
    workbenchWorktreeMerge,
    workbenchWorktreeDiscard,
    workbenchWorktreeRunChecks,
    workbenchWorktreeCheckStatus,
    workbenchWorktreesVersion,
    bumpWorkbenchWorktreesVersion,
    FULL_FILE_CONTEXT,
    type WorktreeInfo,
    type FileDiff,
    type WorktreeCheckStatus,
  } from './workbench';
  import { createShellTab, defaultShellSpec } from './ipc';
  import { cancelPlacement, focusedPane, requestTabIntoPane } from './layout/store';
  import { pairHunkLines } from './diffWords';
  import { errorMessage } from './errors';

  let worktrees = $state<WorktreeInfo[]>([]);
  let loading = $state(true);
  let loadError = $state<string | null>(null);

  // SvelteSet/SvelteMap, NOT plain Set/Map in $state: Svelte 5's proxy only
  // deep-proxies plain objects/arrays, so in-place .add()/.set() on a plain
  // collection triggers no re-render — the diff panel, check chips, and row
  // errors below would silently never appear.
  const expandedDiff = new SvelteSet<string>();
  const diffs = new SvelteMap<string, FileDiff[]>();
  const diffErrors = new SvelteMap<string, string>();
  // Worktrees whose diff panel shows the FULL-file view (huge unified
  // context — whole file as one hunk) instead of the normal 3-line diff;
  // fetched separately and cached in `fullDiffs`.
  const fullDiff = new SvelteSet<string>();
  const fullDiffs = new SvelteMap<string, FileDiff[]>();

  const checkStatuses = new SvelteMap<string, WorktreeCheckStatus | null>();
  const checking = new SvelteSet<string>();

  const busySlugs = new SvelteSet<string>();
  const rowErrors = new SvelteMap<string, string>();

  async function load(): Promise<void> {
    loading = true;
    loadError = null;
    try {
      worktrees = await workbenchWorktrees();
      // Pick up the merge-readiness chip's last cached result for each row
      // (doesn't re-run checks — just whatever was last computed this
      // session). Fetched concurrently — serial awaits would add one IPC
      // round-trip of latency per worktree.
      await Promise.all(
        worktrees
          .filter((w) => !checkStatuses.has(w.slug))
          .map(async (w) => {
            try {
              checkStatuses.set(w.slug, await workbenchWorktreeCheckStatus(w.slug));
            } catch {
              // Non-fatal — the chip just shows "not checked yet".
            }
          }),
      );
    } catch (e) {
      loadError = errorMessage(e);
    } finally {
      loading = false;
    }
  }

  onMount(() => {
    void load();
    const unsub = workbenchWorktreesVersion.subscribe(() => void load());
    return unsub;
  });

  function toggleDiff(slug: string): void {
    if (expandedDiff.has(slug)) {
      expandedDiff.delete(slug);
      return;
    }
    expandedDiff.add(slug);
    if (!diffs.has(slug)) void loadDiff(slug);
  }

  async function loadDiff(slug: string): Promise<void> {
    try {
      const files = await workbenchWorktreeDiff(slug);
      diffs.set(slug, files);
      diffErrors.delete(slug);
    } catch (e) {
      diffErrors.set(slug, errorMessage(e));
    }
  }

  function toggleFullDiff(slug: string, full: boolean): void {
    if (!full) {
      fullDiff.delete(slug);
      return;
    }
    fullDiff.add(slug);
    if (!fullDiffs.has(slug)) void loadFullDiff(slug);
  }

  async function loadFullDiff(slug: string): Promise<void> {
    try {
      const files = await workbenchWorktreeDiff(slug, FULL_FILE_CONTEXT);
      fullDiffs.set(slug, files);
      diffErrors.delete(slug);
    } catch (e) {
      diffErrors.set(slug, errorMessage(e));
    }
  }

  async function runChecks(slug: string): Promise<void> {
    if (checking.has(slug)) return;
    checking.add(slug);
    try {
      const status = await workbenchWorktreeRunChecks(slug);
      checkStatuses.set(slug, status);
    } catch (e) {
      rowErrors.set(slug, errorMessage(e));
    } finally {
      checking.delete(slug);
    }
  }

  async function doMerge(w: WorktreeInfo): Promise<void> {
    if (busySlugs.has(w.slug)) return;
    if (!confirm(`Merge '${w.slug}' (branch cimp/${w.slug}) into '${w.base}'? This runs in your main working tree.`)) {
      return;
    }
    busySlugs.add(w.slug);
    rowErrors.delete(w.slug);
    try {
      await workbenchWorktreeMerge(w.slug);
      bumpWorkbenchWorktreesVersion();
    } catch (e) {
      rowErrors.set(w.slug, errorMessage(e));
    } finally {
      busySlugs.delete(w.slug);
    }
  }

  // Double-confirmed per the milestone — discard permanently deletes the
  // worktree directory AND force-deletes its branch, so any work that never
  // made it into a merge is gone for good.
  async function doDiscard(w: WorktreeInfo): Promise<void> {
    if (busySlugs.has(w.slug)) return;
    if (!confirm(`Discard worktree '${w.slug}'? This deletes the worktree directory and force-deletes branch cimp/${w.slug}.`)) {
      return;
    }
    if (!confirm(`This cannot be undone. Any unmerged work in '${w.slug}' will be permanently lost. Discard anyway?`)) {
      return;
    }
    busySlugs.add(w.slug);
    rowErrors.delete(w.slug);
    try {
      await workbenchWorktreeDiscard(w.slug);
      diffs.delete(w.slug);
      checkStatuses.delete(w.slug);
      bumpWorkbenchWorktreesVersion();
    } catch (e) {
      rowErrors.set(w.slug, errorMessage(e));
    } finally {
      busySlugs.delete(w.slug);
    }
  }

  async function openShellHere(w: WorktreeInfo): Promise<void> {
    const pane = get(focusedPane);
    const placement = requestTabIntoPane(pane.id);
    try {
      const spec = await defaultShellSpec();
      await createShellTab({
        name: `Shell: ${w.slug}`,
        command: spec.command,
        argsString: spec.args,
        cwd: w.path,
        env: {},
        notificationsError: spec.notifications_error,
        notificationsExited: spec.notifications_exited,
      });
    } catch (e) {
      // The shell tab was never created, so the pane placement queued above
      // would mis-route the next tab created anywhere — cancel it.
      cancelPlacement(placement);
      rowErrors.set(w.slug, errorMessage(e));
    }
  }

  function statusTitle(s: FileDiff['status']): string {
    switch (s.kind) {
      case 'Modified': return 'Modified';
      case 'Added': return 'Added';
      case 'Deleted': return 'Deleted';
      case 'Renamed': return `Renamed from ${s.from}`;
      case 'Untracked': return 'Untracked';
    }
  }
</script>

<div class="worktrees-view">
  {#if loading && worktrees.length === 0}
    <p class="msg">Loading worktrees…</p>
  {:else if loadError}
    <p class="msg err">Couldn't load worktrees: {loadError}</p>
  {:else if worktrees.length === 0}
    <p class="msg placeholder">
      No worktrees yet. Right-click a Claude/OpenCode tab and choose "New tab
      in worktree…" to spin one up for a parallel, isolated task.
    </p>
  {:else}
    <div class="wt-list">
      {#each worktrees as w (w.slug)}
        {@const status = checkStatuses.get(w.slug)}
        {@const busy = busySlugs.has(w.slug)}
        <div class="wt-row">
          <div class="wt-head">
            <span class="slug" title={w.path}>⑂ {w.slug}</span>
            <span class="branch">{w.branch} <span class="arrow">→</span> {w.base}</span>
            <span class="ahead-behind">
              {#if w.ahead > 0}<span class="ahead">+{w.ahead}</span>{/if}
              {#if w.behind > 0}<span class="behind">-{w.behind}</span>{/if}
              {#if w.ahead === 0 && w.behind === 0}<span class="even">up to date</span>{/if}
            </span>
            {#if w.has_live_tab}
              <span class="live-tag" title="An AI tab is open in this worktree">live tab</span>
            {:else}
              <span class="orphan-tag" title="No tab currently points at this worktree">no live tab</span>
            {/if}
            {#if status}
              <span
                class="check-chip"
                class:pass={status.pass && status.reports.length > 0}
                class:fail={!status.pass}
                title={status.reports.length === 0 ? 'No checks configured' : `Checked ${new Date(status.checked_at_unix * 1000).toLocaleTimeString()}`}
              >
                {status.reports.length === 0 ? 'no checks' : status.pass ? 'checks pass' : 'checks fail'}
              </span>
            {/if}
          </div>
          <div class="wt-actions">
            <button type="button" onclick={() => toggleDiff(w.slug)}>
              {expandedDiff.has(w.slug) ? 'Hide diff' : 'Diff'}
            </button>
            <button type="button" disabled={checking.has(w.slug)} onclick={() => void runChecks(w.slug)}>
              {checking.has(w.slug) ? 'Checking…' : 'Run checks'}
            </button>
            <button
              type="button"
              class="merge"
              class:ready={status?.pass && status.reports.length > 0}
              disabled={busy}
              onclick={() => void doMerge(w)}
            >Merge</button>
            <button type="button" onclick={() => void openShellHere(w)}>Open shell here</button>
            <button type="button" class="danger" disabled={busy} onclick={() => void doDiscard(w)}>Discard</button>
          </div>
          {#if rowErrors.get(w.slug)}
            <p class="msg err row-err">{rowErrors.get(w.slug)}</p>
          {/if}

          {#if expandedDiff.has(w.slug)}
            {@const full = fullDiff.has(w.slug)}
            {@const files = full ? fullDiffs.get(w.slug) : diffs.get(w.slug)}
            {@const err = diffErrors.get(w.slug)}
            <div class="diff-panel">
              <div class="diff-toolbar">
                <div class="view-toggle" role="group" aria-label="Diff or full file">
                  <button type="button" class:active={!full} onclick={() => toggleFullDiff(w.slug, false)}>Diff</button>
                  <button type="button" class:active={full} onclick={() => toggleFullDiff(w.slug, true)}>Full file</button>
                </div>
              </div>
              {#if err}
                <p class="msg err">{err}</p>
              {:else if !files}
                <p class="msg">Loading diff…</p>
              {:else if files.length === 0}
                <p class="msg">No difference from '{w.base}'.</p>
              {:else}
                {#each files as f (f.path)}
                  <div class="diff-file">
                    <div class="diff-file-head">
                      <span class="status-chip" title={statusTitle(f.status)}>{f.status.kind[0]}</span>
                      <span class="path">{f.path}</span>
                    </div>
                    {#if f.binary}
                      <p class="msg">binary file</p>
                    {:else if f.too_large}
                      <p class="msg">file too large to diff</p>
                    {:else}
                      {#each f.hunks as hunk, hi (hi)}
                        <div class="hunk-body">
                          {#each pairHunkLines(hunk.lines) as group, gi (gi)}
                            {#if group.type === 'ctx'}
                              <div class="line ctx"><span class="marker"> </span><span class="text">{group.text}</span></div>
                            {:else if group.type === 'del'}
                              <div class="line del"><span class="marker">-</span><span class="text">{group.text}</span></div>
                            {:else if group.type === 'add'}
                              <div class="line add"><span class="marker">+</span><span class="text">{group.text}</span></div>
                            {:else}
                              <div class="line del"><span class="marker">-</span><span class="text">{group.oldText}</span></div>
                              <div class="line add"><span class="marker">+</span><span class="text">{group.newText}</span></div>
                            {/if}
                          {/each}
                        </div>
                      {/each}
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
  .worktrees-view {
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
  .msg.placeholder {
    max-width: 60ch;
    line-height: 1.5;
  }
  .wt-list {
    display: flex;
    flex-direction: column;
    gap: var(--space-2);
  }
  .wt-row {
    border: 1px solid var(--border-subtle);
    border-radius: var(--radius-md);
    padding: var(--space-2) var(--space-3);
  }
  .wt-head {
    display: flex;
    flex-wrap: wrap;
    align-items: center;
    gap: var(--space-2);
    margin-bottom: 6px;
  }
  .slug {
    font-weight: 600;
  }
  .branch {
    opacity: 0.75;
    font-family: 'SF Mono', 'Cascadia Code', Consolas, monospace;
    font-size: var(--font-size-xs);
  }
  .arrow {
    opacity: 0.5;
  }
  .ahead-behind .ahead {
    color: var(--text-success, #4caf50);
  }
  .ahead-behind .behind {
    color: var(--text-danger-soft, #e57373);
  }
  .ahead-behind .even {
    opacity: 0.6;
    font-style: italic;
  }
  .live-tag,
  .orphan-tag,
  .check-chip {
    font-size: var(--font-size-xs);
    padding: 1px 6px;
    border-radius: 10px;
    border: 1px solid var(--border-default);
  }
  .live-tag {
    color: var(--text-success, #4caf50);
    border-color: var(--text-success, #4caf50);
  }
  .orphan-tag {
    opacity: 0.6;
  }
  .check-chip.pass {
    color: var(--text-success, #4caf50);
    border-color: var(--text-success, #4caf50);
  }
  .check-chip.fail {
    color: var(--text-danger-soft);
    border-color: var(--text-danger-soft);
  }
  .wt-actions {
    display: flex;
    flex-wrap: wrap;
    gap: 6px;
  }
  .wt-actions button {
    padding: 3px 10px;
    border-radius: var(--radius-sm);
    border: 1px solid var(--border-default);
    background: transparent;
    color: var(--text-primary);
    font-size: var(--font-size-xs);
    cursor: pointer;
  }
  .wt-actions button:hover:not([disabled]) {
    background: var(--surface-4);
  }
  .wt-actions button[disabled] {
    opacity: 0.5;
    cursor: not-allowed;
  }
  .wt-actions .merge.ready {
    border-color: var(--text-success, #4caf50);
    color: var(--text-success, #4caf50);
  }
  .wt-actions .danger {
    color: var(--text-danger-soft);
    border-color: var(--text-danger-soft);
  }
  .row-err {
    margin: 6px 0 0;
  }
  .diff-panel {
    margin-top: var(--space-2);
    border-top: 1px solid var(--border-faint);
    padding-top: var(--space-2);
  }
  .diff-toolbar {
    display: flex;
    justify-content: flex-end;
    margin-bottom: 4px;
  }
  .view-toggle {
    display: inline-flex;
    gap: 2px;
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
  .diff-file {
    margin-bottom: var(--space-2);
  }
  .diff-file-head {
    display: flex;
    align-items: center;
    gap: 6px;
    margin-bottom: 2px;
  }
  .status-chip {
    display: inline-block;
    width: 14px;
    text-align: center;
    font-size: var(--font-size-xs);
    border-radius: 3px;
    background: var(--surface-4);
  }
  .path {
    font-family: 'SF Mono', 'Cascadia Code', Consolas, monospace;
    font-size: var(--font-size-xs);
  }
  .hunk-body {
    font-family: 'SF Mono', 'Cascadia Code', Consolas, monospace;
    font-size: var(--font-size-xs);
    border-radius: var(--radius-sm);
    overflow: hidden;
    margin-bottom: 4px;
  }
  .line {
    display: flex;
    gap: 6px;
    padding: 0 6px;
    white-space: pre;
  }
  .line .marker {
    opacity: 0.5;
    width: 10px;
  }
  .line.add {
    background: rgba(76, 175, 80, 0.12);
  }
  .line.del {
    background: rgba(229, 115, 115, 0.12);
  }
</style>
