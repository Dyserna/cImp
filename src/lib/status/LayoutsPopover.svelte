<script lang="ts">
  // Layouts menu: a small button on the bottom status bar that opens a
  // popover with "Save current layout", a list of recent presets, and a
  // "Manage presets..." entry. Clicking a preset name restores it.
  //
  // Mounted in the status bar's left section so it sits next to future
  // contextual UI; the right section already hosts mute / announcements
  // / volume.
  import { onMount } from 'svelte';
  import { settings } from '../settings/store';
  import {
    openManagePresetsDialog,
    openSaveLayoutDialog,
  } from '../dialog/store';
  import { restoreLayoutPreset } from '../layout/presets';
  import type { LayoutPreset } from '../settings/types';

  let open = $state(false);
  let buttonEl: HTMLButtonElement | undefined = $state();
  let popoverEl: HTMLDivElement | undefined = $state();

  /// Top 5 presets, most-recent first. Recomputed reactively whenever
  /// the settings store updates (after every preset CRUD broadcast).
  /// Sorted by ISO 8601 string, which is lexicographically ordered for
  /// dates of the same shape — works without parsing.
  let recent = $derived(
    [...$settings.layout_presets]
      .sort((a, b) => b.created_at.localeCompare(a.created_at))
      .slice(0, 5),
  );

  function toggle(): void {
    open = !open;
  }

  function handleSaveAs(): void {
    open = false;
    openSaveLayoutDialog();
  }

  function handleManage(): void {
    open = false;
    openManagePresetsDialog();
  }

  function handleRestore(name: string): void {
    open = false;
    restoreLayoutPreset(name);
  }

  // Click-outside / Escape to close. Capture phase so a click on the
  // toggle button (which then re-opens us) doesn't double-fire.
  function onDocumentMouseDown(e: MouseEvent): void {
    if (!open) return;
    const target = e.target as Node | null;
    if (target && (popoverEl?.contains(target) || buttonEl?.contains(target))) {
      return;
    }
    open = false;
  }

  function onKeyDown(e: KeyboardEvent): void {
    if (open && e.key === 'Escape') {
      e.preventDefault();
      open = false;
    }
  }

  onMount(() => {
    document.addEventListener('mousedown', onDocumentMouseDown, true);
    window.addEventListener('keydown', onKeyDown);
    return () => {
      document.removeEventListener('mousedown', onDocumentMouseDown, true);
      window.removeEventListener('keydown', onKeyDown);
    };
  });

  function formatTimestamp(p: LayoutPreset): string {
    // ISO 8601 → friendlier "YYYY-MM-DD HH:MM" tooltip; the popover
    // shows the name only, but tooltip exposes when it was saved.
    return p.created_at.replace('T', ' ').replace('Z', ' UTC').slice(0, 19);
  }
</script>

<div class="root">
  <button
    type="button"
    class="trigger"
    bind:this={buttonEl}
    onclick={toggle}
    aria-haspopup="menu"
    aria-expanded={open}
  >
    Layouts
    <span class="caret" aria-hidden="true">▾</span>
  </button>

  {#if open}
    <div class="popover" bind:this={popoverEl} role="menu">
      <button
        type="button"
        class="entry"
        role="menuitem"
        onclick={handleSaveAs}
      >
        Save current layout as…
      </button>
      <div class="separator" role="separator"></div>
      {#if recent.length === 0}
        <div class="empty">No saved layouts yet</div>
      {:else}
        <div class="section-label">Recent presets</div>
        {#each recent as preset (preset.name)}
          <button
            type="button"
            class="entry preset"
            role="menuitem"
            title={formatTimestamp(preset)}
            onclick={() => handleRestore(preset.name)}
          >
            <span class="preset-name">{preset.name}</span>
          </button>
        {/each}
      {/if}
      <div class="separator" role="separator"></div>
      <button
        type="button"
        class="entry"
        role="menuitem"
        onclick={handleManage}
      >
        Manage presets…
      </button>
    </div>
  {/if}
</div>

<style>
  .root {
    position: relative;
    display: inline-block;
  }
  .trigger {
    background: transparent;
    color: var(--text-secondary);
    border: 1px solid transparent;
    padding: 2px var(--space-3);
    border-radius: var(--radius-pill);
    font-size: var(--font-size-sm);
    cursor: pointer;
    display: inline-flex;
    align-items: center;
    gap: var(--space-1);
    height: 22px;
    transition:
      background var(--motion-fast) var(--easing-standard),
      color var(--motion-fast) var(--easing-standard),
      border-color var(--motion-fast) var(--easing-standard);
  }
  .trigger:hover {
    background: var(--surface-3);
    color: var(--text-primary);
  }
  .trigger[aria-expanded="true"] {
    background: var(--surface-3);
    color: var(--text-primary);
    border-color: var(--border-subtle);
  }
  .trigger:focus-visible {
    outline: 2px solid var(--accent);
    outline-offset: 2px;
  }
  .caret {
    font-size: 10px;
    opacity: 0.7;
  }
  .popover {
    position: absolute;
    bottom: calc(100% + 8px);
    left: 0;
    min-width: 220px;
    max-width: 320px;
    background: var(--surface-3);
    border: 1px solid var(--border-subtle);
    border-radius: var(--radius-md);
    box-shadow: var(--shadow-md);
    padding: var(--space-1) 0;
    z-index: 50;
  }
  .entry {
    display: block;
    width: calc(100% - 8px);
    margin: 0 var(--space-1);
    text-align: left;
    background: transparent;
    color: var(--text-primary);
    border: none;
    padding: 6px var(--space-3);
    font-size: var(--font-size-sm);
    cursor: pointer;
    box-sizing: border-box;
    border-radius: var(--radius-sm);
    transition: background var(--motion-fast) var(--easing-standard);
  }
  .entry:hover {
    background: var(--surface-4);
  }
  .entry:focus-visible {
    outline: 2px solid var(--accent);
    outline-offset: -2px;
  }
  .preset-name {
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .separator {
    height: 1px;
    background: var(--border-subtle);
    margin: var(--space-1) var(--space-2);
  }
  .section-label {
    color: var(--text-tertiary);
    font-size: 10px;
    text-transform: uppercase;
    letter-spacing: 0.5px;
    padding: var(--space-1) var(--space-3) 2px;
  }
  .empty {
    color: var(--text-tertiary);
    font-size: var(--font-size-xs);
    font-style: italic;
    padding: 6px var(--space-3);
  }
</style>
