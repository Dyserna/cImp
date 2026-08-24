<script lang="ts">
  // Bottom-bar eye button: a popover listing every currently-open tab with a
  // visibility checkbox. Unchecking removes the tab from the layout exactly
  // like closing it — its space is freed and emptied panes collapse — but
  // its PTY / backend feed keeps running and the Settings toggle that
  // materializes it is untouched. Re-checking re-inserts the tab into the
  // focused pane and activates it, showing its up-to-date content instantly.
  // Sits next to the Settings button as the quick alternative to
  // enabling/disabling whole features in Settings.
  import { tabs } from '../tabs/store';
  import { hiddenTabs, setTabHidden, showAllTabs } from '../tabs/visibility';

  let open = $state(false);
  let wrapEl: HTMLSpanElement | undefined = $state();

  const hiddenCount = $derived(
    $tabs.reduce((n, m) => n + ($hiddenTabs.has(m.id) ? 1 : 0), 0),
  );

  // Dismiss on outside pointerdown (capture, so a click that lands in a
  // terminal — which stops propagation — still closes us) and on Escape.
  $effect(() => {
    if (!open) return;
    const onPointerDown = (e: PointerEvent): void => {
      if (wrapEl && e.target instanceof Node && wrapEl.contains(e.target)) return;
      open = false;
    };
    const onKeyDown = (e: KeyboardEvent): void => {
      if (e.key !== 'Escape') return;
      e.stopPropagation();
      open = false;
    };
    window.addEventListener('pointerdown', onPointerDown, true);
    window.addEventListener('keydown', onKeyDown, true);
    return () => {
      window.removeEventListener('pointerdown', onPointerDown, true);
      window.removeEventListener('keydown', onKeyDown, true);
    };
  });
</script>

<span class="wrap" bind:this={wrapEl}>
  <button
    type="button"
    class="status-button"
    class:active={open}
    onclick={() => (open = !open)}
    title="Tab visibility"
    aria-label="Show or hide tabs"
    aria-expanded={open}
  >
    <span class="glyph" aria-hidden="true">👁</span>
    {#if hiddenCount > 0}
      <span class="count" aria-label="{hiddenCount} hidden">{hiddenCount}</span>
    {/if}
  </button>

  {#if open}
    <div class="panel" role="menu" aria-label="Tab visibility">
      <div class="panel-head">Tab visibility</div>
      <div class="rows">
        {#each $tabs as m (m.id)}
          <label class="row">
            <input
              type="checkbox"
              checked={!$hiddenTabs.has(m.id)}
              onchange={(e) => setTabHidden(m.id, !e.currentTarget.checked)}
            />
            <span class="name">{m.name}</span>
          </label>
        {/each}
      </div>
      <div class="panel-foot">
        <span class="hint">Hidden tabs keep running in the background.</span>
        <button
          type="button"
          class="mini"
          disabled={hiddenCount === 0}
          onclick={showAllTabs}
        >
          Show all
        </button>
      </div>
    </div>
  {/if}
</span>

<style>
  .wrap {
    position: relative;
    display: inline-flex;
  }
  /* Shell + focus ring: `.status-button` in `src/app.css`. Only the delta is
     local: the hidden-count pip is absolutely positioned against this button. */
  .status-button {
    position: relative;
  }
  .status-button:hover,
  .status-button.active {
    background: var(--surface-3);
    color: var(--text-primary);
  }
  .glyph {
    font-size: 14px;
  }
  /* Hidden-count pip so "some tabs are hidden" is visible at a glance even
     with the popover closed. */
  .count {
    position: absolute;
    top: -4px;
    right: -4px;
    min-width: 12px;
    height: 12px;
    padding: 0 2px;
    border-radius: 6px;
    background: var(--accent);
    color: var(--accent-fg, #fff);
    font-size: 9px;
    font-weight: 700;
    line-height: 12px;
    text-align: center;
  }
  .panel {
    position: absolute;
    right: 0;
    bottom: calc(100% + 10px);
    z-index: 60;
    width: 240px;
    border: 1px solid var(--border-subtle);
    border-radius: var(--radius-md, 6px);
    background: var(--surface-2);
    box-shadow: 0 6px 20px rgba(0, 0, 0, 0.45);
    font-size: 12px;
    color: var(--text-primary);
  }
  .panel-head {
    padding: 8px 10px 6px;
    font-weight: 600;
    border-bottom: 1px solid var(--border-subtle);
  }
  .rows {
    max-height: 260px;
    overflow-y: auto;
    padding: 4px 0;
  }
  .row {
    display: flex;
    align-items: center;
    gap: 8px;
    padding: 4px 10px;
    cursor: pointer;
    white-space: nowrap;
  }
  .row:hover {
    background: var(--surface-3);
  }
  .row input {
    accent-color: var(--accent);
    margin: 0;
    flex: 0 0 auto;
  }
  .name {
    overflow: hidden;
    text-overflow: ellipsis;
  }
  .panel-foot {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 8px;
    padding: 6px 10px 8px;
    border-top: 1px solid var(--border-subtle);
  }
  .hint {
    color: var(--text-tertiary);
    font-size: 10.5px;
  }
  .mini {
    appearance: none;
    border: 1px solid var(--border-subtle);
    border-radius: 4px;
    background: transparent;
    color: var(--text-secondary);
    font-size: 11px;
    padding: 2px 8px;
    cursor: pointer;
    flex: 0 0 auto;
  }
  .mini:hover:not(:disabled) {
    background: var(--surface-3);
    color: var(--text-primary);
  }
  .mini:disabled {
    opacity: 0.5;
    cursor: default;
  }
</style>
