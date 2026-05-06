<script lang="ts">
  // Tab context menu. Triggered by right-click on a tab; rendered as a
  // popover at the click coordinates. Kind-aware entries:
  //   builtin tabs        → Rename
  //   user Shell tabs     → Rename, Configure…, Close
  // Click outside or Escape closes the menu without firing an action.
  import { onMount } from 'svelte';

  let {
    x,
    y,
    builtin,
    onRename,
    onConfigure,
    onClose,
    onDismiss,
  }: {
    x: number;
    y: number;
    builtin: boolean;
    onRename: () => void;
    onConfigure: () => void;
    onClose: () => void;
    onDismiss: () => void;
  } = $props();

  function onWindowMouseDown(): void {
    onDismiss();
  }

  function onWindowKeyDown(e: KeyboardEvent): void {
    if (e.key === 'Escape') {
      e.preventDefault();
      onDismiss();
    }
  }

  function fire(action: () => void) {
    return (e: MouseEvent) => {
      e.stopPropagation();
      action();
      onDismiss();
    };
  }

  onMount(() => {
    // Defer the click listener by a tick so the right-click that opened
    // the menu doesn't immediately dismiss it.
    const id = setTimeout(() => {
      window.addEventListener('mousedown', onWindowMouseDown);
    }, 0);
    window.addEventListener('keydown', onWindowKeyDown);
    return () => {
      clearTimeout(id);
      window.removeEventListener('mousedown', onWindowMouseDown);
      window.removeEventListener('keydown', onWindowKeyDown);
    };
  });
</script>

<div
  class="menu"
  style="left: {x}px; top: {y}px;"
  role="menu"
>
  <button type="button" class="entry" role="menuitem" onclick={fire(onRename)}>
    Rename
  </button>
  {#if !builtin}
    <button type="button" class="entry" role="menuitem" onclick={fire(onConfigure)}>
      Configure…
    </button>
    <div class="separator"></div>
    <button type="button" class="entry danger" role="menuitem" onclick={fire(onClose)}>
      Close
    </button>
  {/if}
</div>

<style>
  .menu {
    position: fixed;
    background: #2a2a2a;
    border: 1px solid #444;
    border-radius: 4px;
    box-shadow: 0 4px 12px rgba(0, 0, 0, 0.5);
    padding: 4px 0;
    min-width: 140px;
    z-index: 200;
  }
  .entry {
    appearance: none;
    border: none;
    background: transparent;
    color: #e0e0e0;
    text-align: left;
    width: 100%;
    padding: 6px 16px;
    font-size: 13px;
    font-family: inherit;
    cursor: pointer;
  }
  .entry:hover {
    background: #383838;
  }
  .entry.danger:hover {
    background: #4a2a2a;
    color: #ffaaaa;
  }
  .separator {
    height: 1px;
    background: #444;
    margin: 4px 0;
  }
</style>
