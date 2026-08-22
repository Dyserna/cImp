<script lang="ts">
  // Combined tab + pane context menu. One menu component handles both
  // the right-click-on-tab case and the right-click-on-tab-bar-background
  // case so the two menus can never overlap each other (which they did
  // when split across two components and the tab event was allowed to
  // bubble).
  //
  // Sections:
  //   * Tab section — only when `tab` is provided. Rename for every
  //     tab; for non-builtin Shell tabs also Configure / Restart /
  //     Close.
  //   * Separator (only when both sections are present).
  //   * Pane section — Split horizontally / Split vertically, then a
  //     separator, then Close pane and Move all tabs to →. Always
  //     present so even a bare tab right-click can pivot to "actually
  //     I want to split this pane".
  //
  // Pane actions wire into the layout store directly (no callbacks
  // from the parent) because they have no per-call configuration —
  // close-focused-pane and split-with-new-shell don't need to know
  // which tab the user right-clicked on.
  //
  // Click outside or Escape dismisses without firing.
  import { onMount } from 'svelte';
  import { get } from 'svelte/store';
  import {
    closeFocusedPane,
    layout,
    moveAllTabsToPane,
  } from './layout/store';
  import { splitFocusedPaneWithNewShell } from './layout/actions';
  import { eachPane } from './layout/tree';
  import { tabs } from './tabs/store';
  import { settings } from './settings/store';
  import { openManagePresetsDialog, openSaveLayoutDialog } from './dialog/store';
  import { restoreLayoutPreset } from './layout/presets';
  import type { PaneId } from './layout/types';
  import type { TabId } from './tabs/types';

  let {
    x,
    y,
    paneId,
    tab,
    onRename,
    onConfigure,
    onRestart,
    onClose,
    onNewWorktreeTab,
    onNewPreviewTab,
    onTakeOver,
    onDismiss,
  }: {
    x: number;
    y: number;
    /// Owning pane — used to populate the Move-all-tabs-to submenu
    /// (excluded from the candidate list) and to scope pane actions.
    paneId: PaneId;
    /// When provided, the tab section renders. When null, only the
    /// pane section shows (right-click on the bar background).
    /// `kind` lets the menu show "Configure…" for builtin AI tabs
    /// (Claude / Claude-local) where the parent routes Configure to
    /// the Settings window. Builtin shell tabs continue to hide it
    /// because their ConfigureTabDialog rejects builtin-shell edits.
    /// V14 Phase F: `'preview'` never shows "Configure…" or "New tab in
    /// worktree…" — a Preview tab's own toolbar (URL bar/device presets/
    /// auto-reload) IS its configuration surface, and worktrees are an
    /// AI-tab-only concept.
    tab: { id: TabId; builtin: boolean; kind: 'shell' | 'ai-tool' | 'preview'; canRestart: boolean } | null;
    /// Tab actions. Required when `tab` is non-null; ignored otherwise.
    /// Optional in the type to make the bar-background callsite
    /// boilerplate-free.
    onRename?: () => void;
    onConfigure?: () => void;
    onRestart?: () => void;
    onClose?: () => void;
    /// V13 Phase D D3: "New <Claude|OpenCode> tab in worktree…" — offered
    /// for any AI-tool tab (builtin or already-duplicated), same
    /// availability rule as `onConfigure`.
    onNewWorktreeTab?: () => void;
    /// V14 Phase F: "New Preview tab" — a pane action (like Split/Close
    /// pane below), not gated on which tab was right-clicked (or offered
    /// even with no tab at all, on a bar-background right-click), since a
    /// Preview tab isn't tied to any existing tab the way a worktree
    /// duplicate is.
    onNewPreviewTab?: () => void;
    /// V39 Phase B, locked decision 6: "Take over (cancel delegation)". Set by
    /// the caller ONLY while this tab is actually being driven, so the entry is
    /// absent rather than disabled the rest of the time — a permanent
    /// greyed-out row on every tab would train the user to stop reading the
    /// menu. Role and access are deliberately NOT mirrored here: the popover is
    /// the one control surface (decision 7), and this is the one action that
    /// must be reachable without finding a glyph.
    onTakeOver?: () => void;
    onDismiss: () => void;
  } = $props();

  let menuEl: HTMLDivElement | undefined = $state();
  let submenuOpen = $state(false);
  let layoutsOpen = $state(false);

  // Clamp the menu into the viewport: opened at the raw cursor coords it would
  // overflow off-screen (and be unreachable) near the right/bottom edges.
  // Seeded from the open coords; the $effect below re-clamps on any change.
  // svelte-ignore state_referenced_locally
  let posX = $state(x);
  // svelte-ignore state_referenced_locally
  let posY = $state(y);
  $effect(() => {
    // Re-read x/y so a reposition re-clamps.
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

  // Top 5 layout presets, most-recent first. ISO 8601 `created_at`
  // strings sort lexicographically, so localeCompare orders them by date.
  const recentPresets = $derived(
    [...$settings.layout_presets]
      .sort((a, b) => b.created_at.localeCompare(a.created_at))
      .slice(0, 5),
  );

  // Other panes for the Move-all-tabs-to submenu, with friendly labels
  // derived from each pane's active tab. Snapshotted at mount; the
  // menu is short-lived (single user action then dismiss) so a live
  // subscription would just add bookkeeping.
  type OtherPane = { id: PaneId; label: string };
  const layoutSnapshot = get(layout);
  const tabsSnapshot = get(tabs);
  const otherPanes: OtherPane[] = (() => {
    const out: OtherPane[] = [];
    for (const pane of eachPane(layoutSnapshot.tree)) {
      if (pane.id === paneId) continue;
      const activeId = pane.active_tab_id;
      const activeMeta = activeId
        ? tabsSnapshot.find((tt) => tt.id === activeId)
        : null;
      const head = activeMeta?.name ?? '(empty pane)';
      const extra = pane.tab_ids.length > 1 ? pane.tab_ids.length - 1 : 0;
      const label = extra > 0 ? `${head} + ${extra} more` : head;
      out.push({ id: pane.id, label });
    }
    return out;
  })();

  // The pane is closeable only when it has a sibling — i.e. when the
  // root is a Split. A bare-root pane has nothing to merge its tabs
  // into and the menu entry is disabled.
  const closeable = layoutSnapshot.tree.type !== 'pane';

  function onWindowMouseDown(e: MouseEvent): void {
    // Only dismiss when the click started outside the menu. Without
    // this check, mousedown on a menu entry unmounts the menu before
    // its click event fires, so none of the entries ever trigger their
    // action.
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

  function fire(action: (() => void) | undefined) {
    return (e: MouseEvent) => {
      if (!action) return;
      e.stopPropagation();
      action();
      onDismiss();
    };
  }

  function onSplitHorizontal(): void {
    void splitFocusedPaneWithNewShell('horizontal');
  }
  function onSplitVertical(): void {
    void splitFocusedPaneWithNewShell('vertical');
  }
  function onClosePane(): void {
    closeFocusedPane();
  }
  function onMoveAllTo(targetId: PaneId): void {
    moveAllTabsToPane(paneId, targetId);
  }
  function onSaveLayout(): void {
    openSaveLayoutDialog();
  }
  function onManagePresets(): void {
    openManagePresetsDialog();
  }
  function onRestorePreset(name: string): void {
    void restoreLayoutPreset(name);
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
  style="left: {posX}px; top: {posY}px;"
  role="menu"
>
  {#if tab}
    <button type="button" class="entry" role="menuitem" onclick={fire(onRename)}>
      Rename
    </button>
    {#if tab.kind !== 'preview' && (!tab.builtin || tab.kind === 'ai-tool')}
      <button type="button" class="entry" role="menuitem" onclick={fire(onConfigure)}>
        Configure…
      </button>
    {/if}
    {#if tab.kind === 'ai-tool' && onNewWorktreeTab}
      <button type="button" class="entry" role="menuitem" onclick={fire(onNewWorktreeTab)}>
        New tab in worktree…
      </button>
    {/if}
    {#if onTakeOver}
      <button
        type="button"
        class="entry"
        role="menuitem"
        title="Stop cImp waiting and unlock your keyboard. The worker is sent nothing — no Escape, no interrupt — so it finishes its turn visibly."
        onclick={fire(onTakeOver)}
      >
        Take over (cancel delegation)
      </button>
    {/if}
    {#if !tab.builtin}
      {#if tab.canRestart && onRestart}
        <button type="button" class="entry" role="menuitem" onclick={fire(onRestart)}>
          Restart shell
        </button>
      {/if}
      <div class="separator"></div>
      <button type="button" class="entry danger" role="menuitem" onclick={fire(onClose)}>
        Close
      </button>
    {/if}
    <div class="separator"></div>
  {/if}
  <button
    type="button"
    class="entry"
    role="menuitem"
    onclick={fire(onSplitHorizontal)}
  >
    Split horizontally
  </button>
  <button
    type="button"
    class="entry"
    role="menuitem"
    onclick={fire(onSplitVertical)}
  >
    Split vertically
  </button>
  {#if onNewPreviewTab}
    <button type="button" class="entry" role="menuitem" onclick={fire(onNewPreviewTab)}>
      New Preview tab
    </button>
  {/if}
  <div class="separator"></div>
  <button
    type="button"
    class="entry"
    class:disabled={!closeable}
    role="menuitem"
    disabled={!closeable}
    onclick={closeable ? fire(onClosePane) : undefined}
  >
    Close pane
  </button>
  {#if otherPanes.length > 0}
    <div class="entry submenu-host" role="menuitem">
      <button
        type="button"
        class="submenu-trigger"
        onmouseenter={() => (submenuOpen = true)}
        onfocus={() => (submenuOpen = true)}
      >
        Move all tabs to
        <span class="submenu-arrow">▶</span>
      </button>
      {#if submenuOpen}
        <!-- svelte-ignore a11y_interactive_supports_focus -->
        <div
          class="submenu"
          role="menu"
          onmouseleave={() => (submenuOpen = false)}
        >
          {#each otherPanes as p (p.id)}
            <button
              type="button"
              class="entry"
              role="menuitem"
              onclick={fire(() => onMoveAllTo(p.id))}
            >
              {p.label}
            </button>
          {/each}
        </div>
      {/if}
    </div>
  {/if}
  <div class="separator"></div>
  <div class="entry submenu-host" role="menuitem">
    <button
      type="button"
      class="submenu-trigger"
      onmouseenter={() => (layoutsOpen = true)}
      onfocus={() => (layoutsOpen = true)}
    >
      Layouts
      <span class="submenu-arrow">▶</span>
    </button>
    {#if layoutsOpen}
      <!-- svelte-ignore a11y_interactive_supports_focus -->
      <div class="submenu" role="menu" onmouseleave={() => (layoutsOpen = false)}>
        <button
          type="button"
          class="entry"
          role="menuitem"
          onclick={fire(onSaveLayout)}
        >
          Save current layout as…
        </button>
        <div class="separator"></div>
        {#if recentPresets.length === 0}
          <div class="submenu-empty">No saved layouts yet</div>
        {:else}
          {#each recentPresets as preset (preset.name)}
            <button
              type="button"
              class="entry"
              role="menuitem"
              onclick={fire(() => onRestorePreset(preset.name))}
            >
              {preset.name}
            </button>
          {/each}
        {/if}
        <div class="separator"></div>
        <button
          type="button"
          class="entry"
          role="menuitem"
          onclick={fire(onManagePresets)}
        >
          Manage presets…
        </button>
      </div>
    {/if}
  </div>
</div>

<style>
  .menu {
    position: fixed;
    background: var(--surface-3);
    border: 1px solid var(--border-subtle);
    border-radius: var(--radius-md);
    box-shadow: var(--shadow-md);
    padding: var(--space-1);
    min-width: 200px;
    z-index: 200;
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
  .entry:hover:not(.disabled):not(.submenu-host):not([disabled]) {
    background: var(--surface-4);
  }
  .entry:focus-visible {
    outline: 2px solid var(--accent);
    outline-offset: -2px;
  }
  .entry.danger:hover {
    background: var(--surface-danger-soft);
    color: var(--text-danger-soft);
  }
  .entry.disabled,
  .entry[disabled] {
    color: var(--text-disabled);
    cursor: default;
  }
  .separator {
    height: 1px;
    background: var(--border-default);
    margin: var(--space-1) 0;
  }
  .submenu-host {
    position: relative;
    padding: 0;
  }
  .submenu-trigger {
    appearance: none;
    border: none;
    background: transparent;
    color: inherit;
    font: inherit;
    width: 100%;
    text-align: left;
    padding: 6px var(--space-3);
    cursor: pointer;
    display: flex;
    justify-content: space-between;
    align-items: center;
    border-radius: var(--radius-sm);
    transition: background var(--motion-fast) var(--easing-standard);
  }
  .submenu-host:hover .submenu-trigger,
  .submenu-trigger:focus {
    background: var(--surface-4);
  }
  .submenu-arrow {
    font-size: 9px;
    color: var(--text-tertiary);
    margin-left: var(--space-2);
  }
  .submenu {
    position: absolute;
    left: 100%;
    top: -5px;
    background: var(--surface-3);
    border: 1px solid var(--border-subtle);
    border-radius: var(--radius-md);
    box-shadow: var(--shadow-md);
    padding: var(--space-1);
    min-width: 200px;
    max-height: 320px;
    overflow-y: auto;
  }
  .submenu-empty {
    color: var(--text-tertiary);
    font-size: var(--font-size-sm);
    font-style: italic;
    padding: 6px var(--space-3);
  }
</style>
