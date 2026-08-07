<script lang="ts">
  // V32 Phase F (locked decision 15): the per-tab taint-latch override
  // popover, opened from the tab-chrome badge.
  //
  // Three things, in the order the user needs them:
  //   1. What is in force right now (the latch, and whether the conversation
  //      is contaminated). A security control that does not say what it is
  //      doing gets clicked past.
  //   2. "Switch to local" — the workflow button. Flips an EXTERNAL latch to
  //      Local: the proxied local-capability tools come back and the external
  //      side closes in the same move, so the session never holds web and
  //      private-data access at once.
  //   3. "Restore full access" — at-own-risk, behind an explicit second click
  //      that spells out WHY it is risky: the injected content is still in the
  //      conversation, so re-opening both sides recreates the trifecta with a
  //      model that may already be steered.
  //
  // The restart line is static and always shown, because it is the only truly
  // clean exit — every override leaves the contamination bit set.
  //
  // Positioning / dismissal mirror TabContextMenu: fixed at the click coords,
  // clamped into the viewport, dismissed by Escape or a mousedown outside.
  import { onMount } from 'svelte';
  import { applyLatchOverride, type LatchAction, type LatchRow } from './latch';

  let {
    x,
    y,
    row,
    onDismiss,
    onApplied,
  }: {
    x: number;
    y: number;
    /// The tab's current latch row — the popover renders entirely from it,
    /// including which actions are legal (the backend owns that rule and
    /// publishes it as `can_flip_local` / `can_unlatch`; re-deriving it here
    /// from the label would put the state machine in two places).
    row: LatchRow;
    onDismiss: () => void;
    /// Fired after a successful override so the caller can refresh its
    /// snapshot without waiting for the next poll tick.
    onApplied?: () => void;
  } = $props();

  let menuEl: HTMLDivElement | undefined = $state();
  let confirmingUnlatch = $state(false);
  let busy = $state(false);
  let error = $state<string | null>(null);

  // svelte-ignore state_referenced_locally
  let posX = $state(x);
  // svelte-ignore state_referenced_locally
  let posY = $state(y);
  $effect(() => {
    const wantX = x;
    const wantY = y;
    if (!menuEl) {
      posX = wantX;
      posY = wantY;
      return;
    }
    const rect = menuEl.getBoundingClientRect();
    const margin = 4;
    posX = Math.max(margin, Math.min(wantX, window.innerWidth - rect.width - margin));
    posY = Math.max(margin, Math.min(wantY, window.innerHeight - rect.height - margin));
  });

  const latchLine = $derived(
    row.latch === 'external'
      ? 'Web / external content in use — local file and source-text tools are closed.'
      : row.latch === 'local'
        ? 'Local file / source-text tools in use — web and other external tools are closed.'
        : 'Not latched.',
  );

  async function run(action: LatchAction): Promise<void> {
    if (busy) return;
    busy = true;
    error = null;
    try {
      await applyLatchOverride(row.tab, row.consumer, action);
      onApplied?.();
      onDismiss();
    } catch (e) {
      // Show the backend's own message. A control that appears to do nothing
      // when clicked is worse than one that explains why it declined.
      error = typeof e === 'string' ? e : ((e as { message?: string })?.message ?? String(e));
      busy = false;
    }
  }

  function onWindowMouseDown(e: MouseEvent): void {
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

  onMount(() => {
    // Defer by a tick so the click that opened the popover doesn't close it.
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
  style="left: {posX}px; top: {posY}px;"
  role="dialog"
  aria-label="Containment state for this tab"
>
  <div class="head">Containment — {row.consumer} · {row.tab}</div>
  <div class="state">{latchLine}</div>
  {#if row.contaminated}
    <div class="state warn">
      This conversation has read external content. Memory writes stay
      quarantined and external results stay wrapped — whatever the latch says.
    </div>
  {/if}

  <div class="separator"></div>

  <button
    type="button"
    class="entry"
    disabled={!row.can_flip_local || busy}
    onclick={() => void run('flip_local')}
  >
    Switch to local — closes web access
  </button>

  {#if confirmingUnlatch}
    <div class="state warn">
      Restoring full access re-opens the web side while the injected content is
      still in this conversation — the model can be steered by it and reach your
      files at the same time. Continue?
    </div>
    <div class="row">
      <button
        type="button"
        class="entry danger"
        disabled={busy}
        onclick={() => void run('unlatch')}
      >
        Yes, restore full access
      </button>
      <button type="button" class="entry" onclick={() => (confirmingUnlatch = false)}>
        Cancel
      </button>
    </div>
  {:else}
    <button
      type="button"
      class="entry"
      disabled={!row.can_unlatch || busy}
      onclick={() => (confirmingUnlatch = true)}
    >
      Restore full access (at your own risk)…
    </button>
  {/if}

  <div class="separator"></div>
  <div class="state">Restarting the tab is the only clean reset.</div>
  {#if error}
    <div class="state err">{error}</div>
  {/if}
</div>

<style>
  .menu {
    position: fixed;
    background: var(--surface-3);
    border: 1px solid var(--border-subtle);
    border-radius: var(--radius-md);
    box-shadow: var(--shadow-md);
    padding: var(--space-1);
    min-width: 260px;
    max-width: 340px;
    z-index: 200;
  }
  .head {
    padding: 4px var(--space-3);
    font-size: var(--font-size-sm);
    color: var(--text-tertiary);
  }
  .state {
    padding: 2px var(--space-3) 6px;
    font-size: var(--font-size-sm);
    color: var(--text-secondary);
    line-height: 1.4;
  }
  .warn {
    color: var(--awaiting);
  }
  .err {
    color: var(--text-danger-soft);
  }
  .row {
    display: flex;
    gap: var(--space-1);
  }
  .entry {
    appearance: none;
    border: none;
    background: transparent;
    color: var(--text-primary);
    text-align: left;
    width: 100%;
    padding: 6px var(--space-3);
    font-size: var(--font-size-md);
    font-family: inherit;
    cursor: pointer;
    border-radius: var(--radius-sm);
    transition: background var(--motion-fast) var(--easing-standard);
  }
  .entry:hover:not([disabled]) {
    background: var(--surface-4);
  }
  .entry:focus-visible {
    outline: 2px solid var(--accent);
    outline-offset: -2px;
  }
  .entry.danger:hover:not([disabled]) {
    background: var(--surface-danger-soft);
    color: var(--text-danger-soft);
  }
  .entry[disabled] {
    color: var(--text-disabled);
    cursor: default;
  }
  .separator {
    height: 1px;
    background: var(--border-default);
    margin: var(--space-1) 0;
  }
</style>
