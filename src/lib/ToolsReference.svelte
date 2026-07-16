<script lang="ts">
  // A reference panel listing the tools a feature exposes, each with a
  // one-line description and a short example prompt. Always expanded and
  // sized to fill the hosting section (the list scrolls internally). Reused
  // by the Tool Activity tab's reference sections.

  interface ToolRef {
    name: string;
    desc: string;
    example: string;
  }
  let {
    title = 'Tools',
    tools,
    note = '',
  }: { title?: string; tools: ToolRef[]; note?: string } = $props();
</script>

<div class="tools-ref">
  <div class="tools-title">
    {title}
    <span class="muted">({tools.length})</span>
  </div>
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
</div>

<style>
  .tools-ref {
    /* Fill the hosting flex column (the Tools tab section area); the list
       below scrolls internally instead of growing the page. */
    flex: 1;
    min-height: 0;
    display: flex;
    flex-direction: column;
  }
  .tools-title {
    color: var(--text-secondary, #8b949e);
    display: inline-flex;
    align-items: center;
    gap: 0.4rem;
    padding: 0.2rem 0;
  }
  .muted {
    color: var(--text-secondary, #8b949e);
    font-weight: 400;
  }
  .tools-view {
    margin-top: 0.4rem;
    flex: 1;
    min-height: 0;
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
