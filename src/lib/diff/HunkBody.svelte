<script lang="ts">
  // The unified line rows of one diff hunk: context / removed / added, with
  // word-level highlighting on a paired remove+add.
  //
  // Extracted in #128 from `DiffView.svelte` (the Workbench live pane) and
  // `CheckpointDiffView.svelte` (the read-only checkpoint / commit view), which
  // carried the same twenty markup lines and the same `.line` / `.marker` /
  // `.text` / `span.hl` rule set verbatim.
  //
  // Scope boundary, and the reason this component is the ROWS rather than the
  // whole body: the wrapper `.hunk-body` stays with each caller. `DiffView`
  // uses the same wrapper for its side-by-side mode, whose `.sbs-*` rules are
  // its own, so moving the wrapper here would have forced `DiffView` to
  // re-declare it. The wrapper supplies the monospace font, size and
  // line-height; those inherit into these rows across the component boundary.
  //
  // The `.line` rule set moved WHOLESALE. A partial move would leave a
  // scoped selector in the caller with nothing left to match — Svelte prunes
  // it, and the styling silently disappears.
  import { pairHunkLines, wordDiff } from '../diffWords';

  let { lines }: { lines: [string, string][] } = $props();
</script>

{#each pairHunkLines(lines) as group, gi (gi)}
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

<style>
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
