<script lang="ts">
  // V14 Phase A: the prompt-library popover rendered above the compose
  // textarea by `ComposeOverlay.svelte`. Purely presentational — all
  // filtering/selection state (query, active index, fuzzy-matching) lives
  // in the parent; this component just renders the already-filtered list
  // and reports clicks back via `onPick`.
  import type { ResolvedTemplate } from './compose/templates';

  let {
    templates,
    activeIndex,
    onPick,
  }: {
    templates: ResolvedTemplate[];
    activeIndex: number;
    onPick: (index: number) => void;
  } = $props();
</script>

<div class="template-picker" role="listbox" aria-label="Prompt templates">
  {#if templates.length === 0}
    <div class="empty">No matching templates</div>
  {:else}
    {#each templates as t, i (t.scope + ':' + t.name)}
      <button
        type="button"
        class="item"
        class:active={i === activeIndex}
        role="option"
        aria-selected={i === activeIndex}
        onclick={() => onPick(i)}
      >
        <span class="name">{t.name}</span>
        <span class="scope" class:project={t.scope === 'project'}>{t.scope}</span>
        <span class="preview">{t.body.replace(/\s+/g, ' ').slice(0, 80)}</span>
      </button>
    {/each}
  {/if}
</div>

<style>
  .template-picker {
    max-height: 220px;
    overflow-y: auto;
    background: var(--surface-1);
    border: 1px solid var(--border-default);
    border-radius: var(--radius-md);
    box-shadow: var(--shadow-md);
  }

  .empty {
    padding: 8px 10px;
    color: var(--text-secondary);
    font-size: 13px;
  }

  .item {
    display: flex;
    align-items: baseline;
    gap: 8px;
    width: 100%;
    box-sizing: border-box;
    padding: 6px 10px;
    background: none;
    border: none;
    border-bottom: 1px solid var(--border-subtle);
    text-align: left;
    cursor: pointer;
    color: var(--text-primary);
    font-family: inherit;
    font-size: 13px;
  }

  .item:last-child {
    border-bottom: none;
  }

  .item.active,
  .item:hover {
    background: var(--surface-sunken);
  }

  .name {
    font-weight: 600;
    flex: 0 0 auto;
  }

  .scope {
    font-size: 11px;
    text-transform: uppercase;
    letter-spacing: 0.03em;
    color: var(--text-secondary);
    flex: 0 0 auto;
  }

  .scope.project {
    color: var(--accent);
  }

  .preview {
    flex: 1 1 auto;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
    color: var(--text-secondary);
  }
</style>
