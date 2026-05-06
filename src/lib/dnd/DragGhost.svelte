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
    background: #1f1f1f;
    color: #ffffff;
    border: 1px solid #4a90e2;
    border-radius: 4px;
    padding: 4px 12px;
    font-size: 13px;
    font-family: system-ui, -apple-system, "Segoe UI", sans-serif;
    opacity: 0.85;
    box-shadow: 0 4px 12px rgba(0, 0, 0, 0.4);
    user-select: none;
    white-space: nowrap;
  }
</style>
