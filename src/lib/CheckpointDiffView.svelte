<script lang="ts" module>
  import { SvelteSet } from 'svelte/reactivity';

  // Per-instance expansion state, keyed by the parent's `stateKey` and held
  // at MODULE scope so it survives the destroy/recreate cycle that tears
  // this component down with its parent (section/tab switch, hide/un-hide,
  // collapsing and re-expanding a commit row). Session-scoped on purpose —
  // commit hashes are meaningless across projects and this never needs to
  // hit disk. Capped LRU-style: the app stays open for days, so "one entry
  // per commit ever clicked into" would otherwise grow without bound; the
  // least-recently-mounted key is evicted first (losing only the expansion
  // memory of a diff the user hasn't looked at in 200 diffs).
  interface KeyedState {
    expanded: SvelteSet<string>;
    fullView: SvelteSet<string>;
  }
  const KEYED_STATES_CAP = 200;
  const keyedStates = new Map<string, KeyedState>();

  function stateFor(stateKey: string | undefined): KeyedState {
    if (!stateKey) return { expanded: new SvelteSet(), fullView: new SvelteSet() };
    let s = keyedStates.get(stateKey);
    if (!s) {
      s = { expanded: new SvelteSet(), fullView: new SvelteSet() };
    } else {
      // Re-insert so Map iteration order doubles as recency order.
      keyedStates.delete(stateKey);
    }
    keyedStates.set(stateKey, s);
    while (keyedStates.size > KEYED_STATES_CAP) {
      const oldest = keyedStates.keys().next().value;
      if (oldest === undefined) break;
      keyedStates.delete(oldest);
    }
    return s;
  }
</script>

<script lang="ts">
  // V13 Phase C — read-only rendering of a checkpoint's diff-vs-now
  // (`workbench_checkpoint_diff`), shown in the Timeline section's "Diff vs
  // now" action. Unlike `DiffView.svelte` (Phase B's live pane) this has no
  // hunk actions (Revert/Send-to-agent make no sense against a checkpoint —
  // there's nothing "current" to apply a reverse patch to) and no lazy
  // per-file fetch: the whole parsed diff arrives in one
  // `workbench_checkpoint_diff` call and is rendered as-is, unified only.
  import type { FileStatus, FileDiff } from './workbench';
  import HunkBody from './diff/HunkBody.svelte';

  // `fetchFull` (optional) powers the per-file "diff ↔ full file" toggle: it
  // fetches the SAME diff with a huge unified context (whole file as one
  // hunk, change highlighting intact). One call covers every file — the
  // result is cached for the component's lifetime, which is scoped to one
  // commit/checkpoint/worktree anyway. No prop → no toggle rendered.
  //
  // `stateKey` (optional) opts this instance into the module-scope keyed
  // state above, so its expansion survives a remount. The key must pin down
  // ONE immutable diff (e.g. `git-graph:<hash>`) — parents whose instance
  // can be repointed at a different diff must recreate it ({#key}) so the
  // captured state can't bleed across. No prop → ephemeral, as before.
  let {
    files,
    fetchFull,
    stateKey,
  }: { files: FileDiff[]; fetchFull?: () => Promise<FileDiff[]>; stateKey?: string } = $props();

  // SvelteSet, NOT a plain Set in $state: Svelte 5 doesn't proxy Set, so an
  // in-place .add() would never re-render — and with static props there's no
  // other refresh here to mask it, leaving expansion completely dead.
  // svelte-ignore state_referenced_locally — the key is an init-time
  // identity by contract (see the prop doc above): a parent that repoints
  // the instance must {#key} it, so capturing the initial value is correct.
  const { expanded, fullView } = stateFor(stateKey);
  function toggleExpand(path: string): void {
    if (expanded.has(path)) expanded.delete(path);
    else expanded.add(path);
  }

  // (`fullView` holds the paths showing the full-file view — per-file, so
  // one giant file doesn't force every other expanded file into full mode.)
  let fullFiles = $state<FileDiff[] | null>(null);
  let fullError = $state<string | null>(null);
  let fullLoading = $state(false);

  async function ensureFullFiles(): Promise<void> {
    if (fullFiles !== null || fullLoading || !fetchFull) return;
    fullLoading = true;
    fullError = null;
    try {
      fullFiles = await fetchFull();
    } catch (e) {
      fullError = String(e);
    } finally {
      fullLoading = false;
    }
  }

  async function toggleFull(path: string): Promise<void> {
    if (fullView.has(path)) {
      fullView.delete(path);
      return;
    }
    fullView.add(path);
    await ensureFullFiles();
  }

  // A remount restores `fullView` but the full-file content is a fresh
  // component-local fetch — kick it off now, or restored toggles would sit
  // at "Loading full file…" forever (toggleFull only fetches on click).
  if (fullView.size > 0) void ensureFullFiles();

  /// The hunks to render for `f`: the full-file variant when toggled on and
  /// loaded, the normal diff otherwise. Falls back to the diff for a path the
  /// full fetch didn't return (e.g. a pure rename with no hunks).
  function displayFile(f: FileDiff): FileDiff {
    if (!fullView.has(f.path) || !fullFiles) return f;
    return fullFiles.find((x) => x.path === f.path) ?? f;
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
</script>

<div class="checkpoint-diff">
  {#if files.length === 0}
    <p class="msg">No difference from the current working tree.</p>
  {:else}
    <div class="file-list">
      {#each files as f (f.path)}
        <div class="file-row">
          <button
            type="button"
            class="file-header"
            onclick={() => toggleExpand(f.path)}
            aria-expanded={expanded.has(f.path) && !f.binary && !f.too_large}
            disabled={f.binary || f.too_large}
          >
            <span class="chevron" aria-hidden="true">{expanded.has(f.path) ? '▾' : '▸'}</span>
            <span class="status-chip status-{f.status.kind.toLowerCase()}">{statusLabel(f.status)}</span>
            <span class="path">{f.path}</span>
            {#if f.status.kind === 'Renamed'}<span class="from-path">← {f.status.from}</span>{/if}
            {#if f.binary}<span class="tag">binary</span>{/if}
            {#if f.too_large}<span class="tag">too large</span>{/if}
          </button>

          {#if expanded.has(f.path) && !f.binary && !f.too_large}
            {@const shown = displayFile(f)}
            <div class="file-body">
              {#if fetchFull}
                <div class="body-toolbar">
                  <div class="view-toggle" role="group" aria-label="Diff or full file">
                    <button type="button" class:active={!fullView.has(f.path)} onclick={() => { fullView.delete(f.path); }}>Diff</button>
                    <button type="button" class:active={fullView.has(f.path)} onclick={() => void toggleFull(f.path)}>Full file</button>
                  </div>
                </div>
              {/if}
              {#if fullView.has(f.path) && fullError}
                <p class="msg err">Couldn't load the full file: {fullError}</p>
              {:else if fullView.has(f.path) && !fullFiles}
                <p class="msg">Loading full file…</p>
              {:else if shown.hunks.length === 0}
                <p class="msg">No hunks.</p>
              {:else}
                {#each shown.hunks as hunk, hunkIndex (hunkIndex)}
                  <div class="hunk">
                    <div class="hunk-header">{hunk.header}</div>
                    <div class="hunk-body">
                      <HunkBody lines={hunk.lines} />
                    </div>
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
  .checkpoint-diff {
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
    color: var(--text-danger-soft, #ffb4ab);
    font-style: normal;
  }
  .body-toolbar {
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
  .file-header {
    appearance: none;
    width: 100%;
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
  .file-body {
    background: var(--surface-sunken);
    padding: var(--space-2);
  }
  .hunk + .hunk {
    margin-top: var(--space-3);
  }
  .hunk-header {
    color: var(--text-tertiary);
    font-family: 'SF Mono', 'Cascadia Code', Consolas, monospace;
    font-size: var(--font-size-xs);
    margin-bottom: 4px;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .hunk-body {
    font-family: 'SF Mono', 'Cascadia Code', Consolas, monospace;
    font-size: var(--font-size-xs);
    line-height: var(--line-height-tight);
    border-radius: var(--radius-sm);
    overflow: hidden;
    border: 1px solid var(--border-faint);
  }
</style>
