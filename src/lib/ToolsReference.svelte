<script lang="ts">
  // A collapsible reference panel listing the tools a feature exposes, each
  // with a one-line description and a short example prompt. Styled to match the
  // "Raw server log" collapsible in the Offload Server tab (caret toggle + a
  // bordered, sunken panel). Reused by the Code Graph and Offload Server tabs.
  import { loadCardOpen, saveCardOpen } from './viewSection';

  interface ToolRef {
    name: string;
    desc: string;
    example: string;
  }
  // `persistKey` (optional) makes the open/collapsed state survive the
  // destroy/recreate cycle of the hosting tab (and app restarts) — see
  // viewSection.ts. No key → ephemeral, as before.
  let {
    title = 'Tools',
    tools,
    note = '',
    persistKey,
  }: { title?: string; tools: ToolRef[]; note?: string; persistKey?: string } = $props();

  // svelte-ignore state_referenced_locally — persistKey is a static
  // identity; only the initial value matters for seeding the open state.
  let open = $state(persistKey ? loadCardOpen('tools-ref', persistKey) : false);
  $effect(() => {
    if (persistKey) saveCardOpen('tools-ref', persistKey, open);
  });
</script>

<div class="tools-ref">
  <button type="button" class="tools-toggle" onclick={() => (open = !open)}>
    <span class="caret" class:open>▸</span>
    {title}
    <span class="muted">({tools.length})</span>
  </button>
  {#if open}
    <div class="tools-view">
      {#if note}<div class="tools-note">{note}</div>{/if}
      {#each tools as t (t.name)}
        <div class="tool">
          <code class="tool-name">{t.name}</code>
          <div class="tool-desc">{t.desc}</div>
          <div class="tool-eg"><span class="eg">e.g.</span> {t.example}</div>
        </div>
      {/each}
    </div>
  {/if}
</div>

<style>
  .tools-ref {
    margin-top: 0.6rem;
    border-top: 1px solid var(--border-subtle, #21262d);
    padding-top: 0.5rem;
  }
  .tools-toggle {
    background: none;
    border: none;
    color: var(--text-secondary, #8b949e);
    cursor: pointer;
    font: inherit;
    display: inline-flex;
    align-items: center;
    gap: 0.4rem;
    padding: 0.2rem 0;
  }
  .caret {
    display: inline-block;
    transition: transform 0.12s ease;
  }
  .caret.open {
    transform: rotate(90deg);
  }
  .muted {
    color: var(--text-secondary, #8b949e);
    font-weight: 400;
  }
  .tools-view {
    margin-top: 0.4rem;
    max-height: 24rem;
    overflow: auto;
    background: var(--surface-sunken, #161b22);
    border: 1px solid var(--border-subtle, #21262d);
    border-radius: 4px;
    padding: 0.3rem 0.6rem;
    font-size: 0.9em;
    line-height: 1.4;
  }
  .tools-note {
    color: var(--text-secondary, #8b949e);
    font-style: italic;
    padding: 0.4rem 0;
    border-bottom: 1px solid var(--border-subtle, #21262d);
  }
  .tool {
    padding: 0.5rem 0;
    border-bottom: 1px solid var(--border-subtle, #21262d);
  }
  .tool:last-child {
    border-bottom: none;
  }
  .tool-name {
    font-family: var(--font-mono, ui-monospace, monospace);
    font-size: 0.92em;
    /* Hardcoded blue accent (not --text-*, which follow the terminal palette
       and can render pink/magenta) so tool names stay legible in any theme. */
    color: #58a6ff;
  }
  .tool-desc {
    margin-top: 0.15rem;
    color: var(--text-primary, #c9d1d9);
  }
  .tool-eg {
    margin-top: 0.2rem;
    color: var(--text-secondary, #8b949e);
  }
  .eg {
    text-transform: uppercase;
    font-size: 0.82em;
    letter-spacing: 0.04em;
    opacity: 0.8;
    margin-right: 0.2rem;
  }
</style>
