<script lang="ts">
  // Tab context menu. Triggered by right-click on a tab; rendered as a
  // popover at the click coordinates. Kind-aware entries:
  //   builtin tabs        → Rename
  //   user Shell tabs     → Rename, Configure…, [Restart shell], Close
  // Restart shell is hidden when the tab is in a closed state — the
  // closed overlay's Enter affordance is the equivalent action there, and
  // a restart on a launch-failed tab would just fail again (the user
  // needs Configure to fix the broken command).
  // Click outside or Escape closes the menu without firing an action.
  import { onMount } from 'svelte';

  let {
    x,
    y,
    builtin,
    canRestart = false,
    onRename,
    onConfigure,
    onRestart,
    onClose,
    onDismiss,
  }: {
    x: number;
    y: number;
    builtin: boolean;
    /// True for user Shell tabs whose subprocess is currently running.
    /// Hides the Restart entry when the tab is in a closed state (the
    /// closed overlay's Enter is the equivalent affordance).
    canRestart?: boolean;
    onRename: () => void;
    onConfigure: () => void;
    onRestart?: () => void;
    onClose: () => void;
    onDismiss: () => void;
  } = $props();

  let menuEl: HTMLDivElement | undefined = $state();

  function onWindowMouseDown(e: MouseEvent): void {
    // Only dismiss when the click started outside the menu. Without this
    // check, mousedown on a menu entry unmounts the menu before its
    // click event fires, so none of the entries ever trigger their action.
    const target = e.target as Node | null;
    if (target && menuEl && menuEl.contains(target)) return;
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
  bind:this={menuEl}
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
    {#if canRestart && onRestart}
      <button type="button" class="entry" role="menuitem" onclick={fire(onRestart)}>
        Restart shell
      </button>
    {/if}
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
