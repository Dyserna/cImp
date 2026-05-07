<script lang="ts">
  // Translucent tab-like element that follows the cursor during a
  // drag. Mounted once at the app root (alongside other overlays so
  // it's layered above panes but below modal dialogs). Pointer events
  // are disabled on it — the ghost must never intercept the
  // pointermove/up flow on the source tab.

  import { dragState } from './drag';
  import { tabMeta } from '../tabs/store';

  const offsetX = 8;
  const offsetY = 8;
</script>

{#if $dragState.kind === 'dragging'}
  {@const meta = tabMeta($dragState.tabId)}
  {#if meta}
    <div
      class="drag-ghost"
      aria-hidden="true"
      style:left="{$dragState.cursorX + offsetX}px"
      style:top="{$dragState.cursorY + offsetY}px"
    >
      {meta.name}
    </div>
  {/if}
{/if}

<style>
  .drag-ghost {
    position: fixed;
    z-index: 10000;
    pointer-events: none;
    background: var(--surface-3);
    color: var(--text-bright);
    border: 1px solid var(--accent);
    border-radius: var(--radius-md);
    padding: var(--space-1) var(--space-3);
    font-size: var(--font-size-md);
    font-family: system-ui, -apple-system, "Segoe UI", sans-serif;
    opacity: 0.92;
    box-shadow: var(--shadow-md);
    transform: rotate(-1.5deg);
    user-select: none;
    white-space: nowrap;
  }
</style>
