<script lang="ts">
  // V13 Phase C — read-only rendering of a checkpoint's diff-vs-now
  // (`workbench_checkpoint_diff`), shown in the Timeline section's "Diff vs
  // now" action. Unlike `DiffView.svelte` (Phase B's live pane) this has no
  // hunk actions (Revert/Send-to-agent make no sense against a checkpoint —
  // there's nothing "current" to apply a reverse patch to) and no lazy
  // per-file fetch: the whole parsed diff arrives in one
  // `workbench_checkpoint_diff` call and is rendered as-is, unified only.
  import { SvelteSet } from 'svelte/reactivity';
  import type { FileStatus, FileDiff } from './workbench';
  import { pairHunkLines, wordDiff } from './diffWords';

  let { files }: { files: FileDiff[] } = $props();

  // SvelteSet, NOT a plain Set in $state: Svelte 5 doesn't proxy Set, so an
  // in-place .add() would never re-render — and with static props there's no
  // other refresh here to mask it, leaving expansion completely dead.
  const expanded = new SvelteSet<string>();
  function toggleExpand(path: string): void {
    if (expanded.has(path)) expanded.delete(path);
    else expanded.add(path);
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
            <div class="file-body">
              {#if f.hunks.length === 0}
                <p class="msg">No hunks.</p>
              {:else}
                {#each f.hunks as hunk, hunkIndex (hunkIndex)}
                  <div class="hunk">
                    <div class="hunk-header">{hunk.header}</div>
                    <div class="hunk-body">
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
</style>
