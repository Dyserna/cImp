<script lang="ts">
  // V39 Phase A — the tab communication popover (locked decision 7).
  //
  // The glyph next to the shield is the ONE control surface for delegation, so
  // this popover is where a tab's access is set. Phase A ships only the Access
  // radio; the Role radio (None / Manual / Remote offload) and the Remote
  // knobs land in Phases B and C, in this same popover.
  //
  // Modelled on `TaintMenu.svelte`, deliberately: same fixed-position anchor
  // with a viewport clamp, same deferred mousedown + Escape dismissal, same
  // "derive state from the store, never snapshot it at click time" rule — a
  // radio rendered from a click-time copy would show the pre-click value right
  // after the user's own write landed.
  import { onMount } from 'svelte';
  import { tabSetReadOnly } from './ipc';
  import { glyphState, type TabAccess } from './delegation';
  import { showToast } from './toast';
  import type { TabId } from './tabs/types';

  let {
    x,
    y,
    tab,
    tabName,
    access,
    /// The tab is being driven by a delegation right now. Always `false` in
    /// Phase A — nothing sets it yet — but the state it renders is written
    /// here so Phase B adds an engine, not a UI.
    driven = false,
    driverName = null,
    onDismiss,
  }: {
    x: number;
    y: number;
    tab: TabId;
    tabName: string;
    access: TabAccess;
    driven?: boolean;
    driverName?: string | null;
    onDismiss: () => void;
  } = $props();

  let menuEl: HTMLDivElement | undefined = $state();
  let busy = $state(false);
  let err = $state<string | null>(null);

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

  const glyph = $derived(
    glyphState({ role: 'none', access, inFlight: driven, driverName }),
  );

  async function setAccess(next: TabAccess): Promise<void> {
    if (busy || next === access || driven) return;
    busy = true;
    err = null;
    try {
      await tabSetReadOnly(tab, next === 'ro');
      showToast(
        next === 'ro'
          ? `“${tabName}” is now read-only — your keyboard is refused, the tab keeps running.`
          : `“${tabName}” accepts your keyboard again.`,
      );
    } catch (e) {
      // Say it in the popover, not only in the console: the radio would
      // otherwise appear to have taken while the tab still accepts keys.
      err = String(e);
    } finally {
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
  aria-label="Delegation and access for this tab"
>
  <div class="head">Communication — {tabName}</div>
  <div class="state">{glyph.title}</div>

  <div class="separator"></div>
  <div class="head">Access</div>
  <ul class="choices">
    <li>
      <label class:disabled={driven || busy}>
        <input
          type="radio"
          name="delegation-access-{tab}"
          checked={!driven && access === 'rw'}
          disabled={driven || busy}
          onchange={() => void setAccess('rw')}
        />
        <span class="name">Read/write</span>
      </label>
    </li>
    <li>
      <label class:disabled={driven || busy}>
        <input
          type="radio"
          name="delegation-access-{tab}"
          checked={!driven && access === 'ro'}
          disabled={driven || busy}
          onchange={() => void setAccess('ro')}
        />
        <span class="name">Read-only</span>
      </label>
    </li>
    <!--
      The engine's own lock, shown as a third, disabled state rather than by
      hiding the radio: a control that vanishes reads as "there is no such
      setting", and the user's next move is to go looking for it. Take over is
      how this one ends — a radio button never lifts a lock a delegation owns.
      Nothing sets `driven` in Phase A; the markup exists so Phase B wires an
      engine to a UI that already says the right thing.
    -->
    {#if driven}
      <li>
        <label class="disabled">
          <input type="radio" name="delegation-access-{tab}" checked disabled />
          <span class="name">Read-only (driven by {driverName?.trim() || 'another tab'})</span>
        </label>
      </li>
    {/if}
  </ul>

  {#if driven}
    <div class="row">
      <!-- Phase B: cancels the delegation, leaves the worker running. -->
      <button type="button" class="entry" disabled title="Available in a later phase">
        Take over (cancel delegation)
      </button>
    </div>
  {/if}

  {#if err}
    <div class="state err">{err}</div>
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
  .err {
    color: var(--text-danger-soft);
  }
  .choices {
    margin: 0;
    padding: 0 var(--space-3) 6px;
    list-style: none;
    font-size: var(--font-size-sm);
    color: var(--text-secondary);
  }
  .choices li {
    display: flex;
    align-items: center;
  }
  .choices label {
    display: flex;
    align-items: center;
    gap: 6px;
    flex: 1 1 auto;
    min-width: 0;
    cursor: pointer;
    padding: 2px 0;
  }
  .choices label.disabled {
    cursor: default;
    color: var(--text-disabled);
  }
  .choices .name {
    flex: 1 1 auto;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
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
