<script lang="ts">
  import Tab from './Tab.svelte';
  import TabContextMenu from './TabContextMenu.svelte';
  import TaintMenu from './TaintMenu.svelte';
  import DelegationPopover from './DelegationPopover.svelte';
  import { accessOf, glyphState, hasCommIcon } from './delegation';
  import {
    fetchLatchStatus,
    injectionStatus,
    isTainted,
    latchByTab,
    reducedFeaturesFor,
    tabProtectionRows,
    type LatchRow,
  } from './latch';
  import { tabs } from './tabs/store';
  import { switchTab } from './tabs/state';
  import {
    perTabAvatarState,
    perTabAwaitingPermission,
    perTabClosedState,
    perTabDoneWhileAway,
  } from './avatarState';
  import {
    closeTab as closeTabIpc,
    createAiTab,
    createPreviewTab,
    renameTab as renameTabIpc,
    restartShellTab,
  } from './ipc';
  import { settings } from './settings/store';
  import { openConfigureTabDialog, openNewShellTabDialog, openNewWorktreeTabDialog } from './dialog/store';
  import { openSettingsWindowToTab } from './settings/ipc';
  import { isPreviewTabId, isShellTab, type TabId } from './tabs/types';
  import { tabMeta } from './tabs/store';
  import { cancelPlacement, requestTabIntoPane, setFocusedPane, setPaneActiveTab } from './layout/store';
  import { paneRegistry } from './layout/registry';
  import { beginDrag } from './dnd/drag';
  import type { PaneNode } from './layout/types';

  // Pane-scoped tab bar. Renders only the tabs that belong to this
  // pane, drives active state from the pane's own active_tab_id, and
  // routes the `+` button to land its newly-created tab here (via the
  // layout store's `pendingTabTargetPane` cell, consumed by the next
  // tab-created event).

  let { pane }: { pane: PaneNode } = $props();

  // Per-pane rename mode flag. Two-way bound on each Tab instance.
  let renamingTab = $state<TabId | null>(null);

  // Unified context-menu state. `tab === null` means right-click on
  // bar background (pane-only menu); `tab !== null` means right-click
  // on a specific tab (tab + pane menu in one). The two cases share
  // one TabContextMenu instance so they can never overlap.
  let menu = $state<{
    x: number;
    y: number;
    tab: { id: TabId; builtin: boolean } | null;
  } | null>(null);

  // V32 Phase F: the taint-override popover. Anchored at the badge the user
  // clicked, carrying that tab's latch row. Separate state from `menu` so the
  // two can never be open at once for different reasons.
  //
  // V32 Phase G: it also carries the tab's reduced-protection rows, because the
  // badge can now open on a CLEAN tab whose controls are switched off — and a
  // popover that then said only "not latched" would be answering a question
  // nobody asked.
  //
  // **#48, M-24 → M-22: this cell holds the ANCHOR, never the state.** It used
  // to carry the `LatchRow` and the reduced-feature rows copied out of the
  // stores at click time, so the popover was a click-time SNAPSHOT that never
  // moved again. A tab that became contaminated — or latched EXTERNAL by a
  // native-web beacon — while the user was reading the popover kept saying
  // "Not latched.", in the one surface whose entire job is to say what is in
  // force *right now*, and whose buttons the user is about to press on the
  // strength of it.
  //
  // The fix is to derive the row and the reduced list below instead of copying
  // them, so the popover re-renders from the SAME 4 s `startLatchPolling` tick
  // that already drives the badge. Deliberately no timer of its own: an
  // interval added here would also have to be gated on `appViewVisibility`
  // (`appViews.ts` keeps views alive in a portal registry and never destroys
  // them on a tab switch), and there is nothing for a second poller to learn
  // that the store does not already have.
  let taintMenu = $state<{
    x: number;
    y: number;
    tab: TabId;
  } | null>(null);

  /// V39 Phase A: the communication popover. Anchored at the glyph, one at a
  /// time, and holding only the anchor + tab id — everything it renders is
  /// derived from `$settings` below, so a write made inside it is reflected by
  /// the same broadcast that persists it (the M-22 rule the taint popover
  /// already follows).
  let commMenu = $state<{
    x: number;
    y: number;
    tab: TabId;
  } | null>(null);

  const commMenuAccess = $derived(commMenu ? accessOf($settings, commMenu.tab) : 'rw');
  const commMenuName = $derived(
    (commMenu ? $tabs.find((m) => m.id === commMenu?.tab)?.name : null) ?? commMenu?.tab ?? '',
  );

  /// Open the communication popover, closing anything else that is open — two
  /// popovers anchored at neighbouring glyphs would overlap.
  function onCommGlyph(tab: TabId, e: MouseEvent): void {
    menu = null;
    taintMenu = null;
    commMenu = { x: e.clientX, y: e.clientY, tab };
  }

  /// The popover's LIVE row — re-read from `latchByTab` on every poll tick for
  /// as long as the popover is open (M-22).
  const taintMenuRow = $derived(
    taintMenu ? rowFor(taintMenu.tab, $latchByTab[taintMenu.tab]) : null,
  );

  /// The popover's LIVE reduced-protection rows, from the same tick. Kept
  /// beside the row rather than fetched separately for the reason
  /// `startLatchPolling` fetches them together: the badge means one thing when
  /// the latch is engaged and another when the latch feature is switched off, so
  /// the two disagreeing for a poll interval is how a badge stops being read.
  const taintMenuReduced = $derived(
    taintMenu ? reducedFeaturesFor($injectionStatus, taintMenu.tab) : [],
  );

  /// V39: the popover's LIVE per-tab injection switches, from the same tick as
  /// the badge that opened it. Derived rather than copied at click time for the
  /// M-22 reason above — but with a second one of its own now that these rows
  /// are a CONTROL: a checkbox rendered from a click-time snapshot would show
  /// the pre-click value after the user's own write landed.
  const taintMenuProtection = $derived(
    taintMenu ? tabProtectionRows($injectionStatus, taintMenu.tab) : [],
  );

  /// The row a badge click carries. A tab with no gated call yet has no latch
  /// row at all, so synthesize the "nothing latched" one rather than making the
  /// popover handle a null: the state it describes is real (open, uncontaminated,
  /// nothing to override), and the backend publishes exactly that shape for a tab
  /// it has never served.
  function rowFor(tab: TabId, row: LatchRow | undefined): LatchRow {
    return (
      row ?? {
        consumer: 'claude',
        tab,
        session: null,
        latch: 'open',
        contaminated: false,
        can_flip_local: false,
        can_unlatch: false,
        can_clear: false,
        awaiting_session_clear: false,
        local_by_user_flip: false,
      }
    );
  }

  /// Open the popover. Takes only the anchor and the tab id — the state comes
  /// from the stores, live (M-22). `row` stays in the signature because the
  /// badge already holds it and the call site reads better naming what it is
  /// about; it is deliberately not stored.
  function onTaintBadge(tab: TabId, _row: LatchRow | undefined, e: MouseEvent): void {
    menu = null;
    commMenu = null;
    taintMenu = { x: e.clientX, y: e.clientY, tab };
  }

  /// Refresh the latch snapshot immediately after an override, instead of
  /// leaving the badge showing a stale state until the next poll tick.
  function refreshLatches(): void {
    void fetchLatchStatus()
      .then((rows) => {
        const next: Partial<Record<TabId, LatchRow>> = {};
        for (const row of rows) {
          const prev = next[row.tab];
          if (!prev || (!isTainted(prev) && isTainted(row))) next[row.tab] = row;
        }
        latchByTab.set(next);
      })
      .catch(() => {
        /* the poll will catch up */
      });
  }

  // The tab bar's root element, registered with the DOM registry so
  // the M2 drop-target hit-tester can distinguish "drop on tab bar"
  // (reorder / move-to-pane) from "drop in content area" (split). The
  // registry stores the element; rects are read live so resizes and
  // splitter moves stay correct without re-registering.
  let barEl: HTMLDivElement | undefined = $state();
  let listEl: HTMLDivElement | undefined = $state();
  let canScrollLeft = $state(false);
  let canScrollRight = $state(false);

  $effect(() => {
    if (!barEl) return;
    const id = pane.id;
    paneRegistry.setTabBarElement(id, barEl);
    return () => {
      paneRegistry.setTabBarElement(id, null);
    };
  });

  // Recompute edge-fade visibility from the scroll state. Driven by
  // (1) scroll events on the list, (2) ResizeObserver on the list (the
  // pane width can change via splitter drag without firing a scroll),
  // and (3) reactive re-runs whenever pane.tab_ids changes (adding /
  // removing tabs changes scrollWidth without scrolling).
  function updateScrollEdges(): void {
    if (!listEl) return;
    const scrollLeft = listEl.scrollLeft;
    const maxScroll = listEl.scrollWidth - listEl.clientWidth;
    canScrollLeft = scrollLeft > 0;
    canScrollRight = scrollLeft < maxScroll - 1;
  }

  $effect(() => {
    if (!listEl) return;
    const el = listEl;
    updateScrollEdges();
    el.addEventListener('scroll', updateScrollEdges, { passive: true });
    const ro = new ResizeObserver(() => updateScrollEdges());
    ro.observe(el);
    return () => {
      el.removeEventListener('scroll', updateScrollEdges);
      ro.disconnect();
    };
  });

  // Re-evaluate when the tab list changes (add / remove / rename can
  // shift scrollWidth without firing scroll). UI-hidden tabs are absent
  // from pane.tab_ids entirely, so hide/show flows through this too.
  $effect(() => {
    void pane.tab_ids.length;
    queueMicrotask(updateScrollEdges);
  });

  // Scroll the active tab into view whenever it changes. Catches
  // Ctrl+N switching to a tab that's off-screen in an overflowed bar
  // and click-on-truncated-tab activating one. `inline: 'nearest'`
  // avoids unnecessary movement when the active tab is already fully
  // visible.
  $effect(() => {
    const id = pane.active_tab_id;
    if (!id || !listEl) return;
    queueMicrotask(() => {
      const tabEl = listEl?.querySelector<HTMLElement>(`[data-tab-id="${CSS.escape(id)}"]`);
      tabEl?.scrollIntoView({ inline: 'nearest', block: 'nearest' });
    });
  });

  /// Arrow-button scroll (replaces the under-tab scrollbar, which ate
  /// vertical space and clashed with the TUI look). One click moves
  /// roughly one tab width; smooth-scroll keeps the motion readable.
  function scrollTabs(dir: -1 | 1): void {
    listEl?.scrollBy({ left: dir * 120, behavior: 'smooth' });
  }

  function onTabClick(tabId: TabId): void {
    setPaneActiveTab(pane.id, tabId);
    // Mirror to the backend so audio / avatar / compose routing follows
    // the pane's new active tab. switchTab is the v1.2 call that
    // updates session.active_tab_id and broadcasts ActiveTabChanged.
    void switchTab(tabId);
  }

  function onCloseTab(tab: TabId): void {
    void closeTabIpc(tab).catch((e) => {
      console.error('close_tab failed:', e);
    });
  }

  /// Spawn a duplicate of an AI builtin (the `+` on a Claude/OpenCode tab).
  /// Routes the new tab into this pane via the same pending-placement
  /// cell the New Shell Tab `+` uses, so it lands next to its origin.
  function onSpawnAiTab(template: TabId): void {
    const placement = requestTabIntoPane(pane.id);
    void createAiTab(template).catch((e) => {
      // No tab-created will arrive to consume the placement — cancel it
      // or it would silently re-route the next tab created anywhere.
      cancelPlacement(placement);
      console.error('create_ai_tab failed:', e);
    });
  }

  function onRestartTab(tab: TabId): void {
    void restartShellTab(tab).catch((e) => {
      console.error('restart_shell_tab failed:', e);
    });
  }

  function onRenameTab(tab: TabId, newName: string): void {
    void renameTabIpc(tab, newName).catch((e) => {
      console.error('rename_tab failed:', e);
    });
  }

  function onTabContextMenu(tab: TabId, builtin: boolean, e: MouseEvent): void {
    menu = { x: e.clientX, y: e.clientY, tab: { id: tab, builtin } };
  }


  function dismissMenu(): void {
    menu = null;
  }

  /// Right-click on the tab bar's background. Tab.svelte stops
  /// propagation on its own contextmenu handler, so by the time this
  /// fires we know the click really did land on the bar background
  /// (not on a tab) — open the pane-only variant of the menu. The `+`
  /// button silences its own contextmenu separately. Focusing the pane
  /// on open mirrors what a left-click does — the user almost
  /// certainly wants to act on this pane next.
  function onBarContextMenu(e: MouseEvent): void {
    e.preventDefault();
    setFocusedPane(pane.id);
    menu = { x: e.clientX, y: e.clientY, tab: null };
  }

  /// Right-click on the `+` button opens the same pane-only menu as a
  /// right-click on the bar background (V14 Phase F added "New Preview
  /// tab" there) — the `+` button itself stays a left-click shortcut for
  /// "new shell tab", but the less-common "new Preview tab" affordance
  /// doesn't need its own dedicated button.
  function onNewTabContextMenu(e: MouseEvent): void {
    e.preventDefault();
    e.stopPropagation();
    setFocusedPane(pane.id);
    menu = { x: e.clientX, y: e.clientY, tab: null };
  }

  /// The dialog enqueues the pane placement itself at submit time (and
  /// cancels it if the create IPC fails). Pushing it here at open time
  /// leaked the placement whenever the dialog was cancelled — no
  /// tab-created ever consumed it, so the next tab created anywhere in
  /// the app was silently routed into this pane.
  function onNewShellTab(): void {
    openNewShellTabDialog(pane.id);
  }

  /// V14 Phase F: "New Preview tab" (pane context menu). An empty url lets
  /// the backend fall back to `Settings.preview_last_url` /
  /// `DEFAULT_PREVIEW_URL` — the frontend doesn't duplicate that fallback
  /// logic here.
  function onNewPreviewTabAction(): void {
    const placement = requestTabIntoPane(pane.id);
    void createPreviewTab($settings.preview_last_url ?? '').catch((e) => {
      // Same contract as onSpawnAiTab: a failed create must release
      // its queued placement.
      cancelPlacement(placement);
      console.error('create_preview_tab failed:', e);
    });
  }
</script>

<div
  class="tab-bar"
  role="tablist"
  tabindex="-1"
  bind:this={barEl}
  oncontextmenu={onBarContextMenu}
>
  <div
    class="tab-list-wrap"
    class:fade-left={canScrollLeft}
    class:fade-right={canScrollRight}
  >
    <div class="tab-list" bind:this={listEl}>
      {#each pane.tab_ids as id (id)}
        {@const meta = $tabs.find((m) => m.id === id)}
        {@const latchRow = $latchByTab[id]}
        {#if meta}
          <Tab
            tabId={id}
            label={meta.name}
            active={pane.active_tab_id === id}
            builtin={meta.builtin}
            canSkipCloseConfirm={$perTabClosedState[id]?.closed ?? false}
            showIndicator={meta.kind === 'ai-tool'}
            avatarState={$perTabAvatarState[id] ?? 'Idle'}
            awaitingPermission={$perTabAwaitingPermission[id] ?? false}
            doneWhileAway={$perTabDoneWhileAway[id] ?? false}
            taint={isTainted(latchRow) ? (latchRow ?? null) : null}
            reduced={reducedFeaturesFor($injectionStatus, id)}
            protection={tabProtectionRows($injectionStatus, id)}
            ontaint={(e) => onTaintBadge(id, latchRow, e)}
            comm={hasCommIcon($settings, id)
              ? glyphState({ role: 'none', access: accessOf($settings, id), inFlight: false })
              : null}
            oncomm={(e) => onCommGlyph(id, e)}
            bind:renaming={
              () => renamingTab === id,
              (v) => {
                if (v) renamingTab = id;
                else if (renamingTab === id) renamingTab = null;
              }
            }
            onclick={() => onTabClick(id)}
            onclose={meta.builtin ? undefined : () => onCloseTab(id)}
            onnew={meta.kind === 'ai-tool' && meta.builtin
              ? () => onSpawnAiTab(id)
              : undefined}
            oncontextmenu={(e) => onTabContextMenu(id, meta.builtin, e)}
            onpointerdowndrag={(e) => beginDrag(id, pane.id, e)}
            onrename={(newName) => onRenameTab(id, newName)}
          />
        {/if}
      {/each}
    </div>
  </div>
  <div class="tab-bar-end">
    {#if canScrollLeft || canScrollRight}
      <button
        type="button"
        class="scroll-arrow"
        aria-label="Scroll tabs left"
        title="Scroll tabs left"
        disabled={!canScrollLeft}
        onclick={() => scrollTabs(-1)}
      >
        ‹
      </button>
      <button
        type="button"
        class="scroll-arrow scroll-arrow-right"
        aria-label="Scroll tabs right"
        title="Scroll tabs right"
        disabled={!canScrollRight}
        onclick={() => scrollTabs(1)}
      >
        ›
      </button>
    {/if}
    <button
      type="button"
      class="new-tab"
      aria-label="New shell tab"
      title="New shell tab (Ctrl+T)"
      onclick={onNewShellTab}
      oncontextmenu={onNewTabContextMenu}
    >
      +
    </button>
  </div>
</div>

{#if menu}
  {@const t = menu.tab}
  {@const isShell = t ? isShellTab(t.id) : false}
  {@const isPreview = t ? isPreviewTabId(t.id) : false}
  {@const closed = t ? ($perTabClosedState[t.id]?.closed ?? false) : false}
  <TabContextMenu
    x={menu.x}
    y={menu.y}
    paneId={pane.id}
    tab={t
      ? {
          id: t.id,
          builtin: t.builtin,
          kind: isPreview ? 'preview' : isShell ? 'shell' : 'ai-tool',
          canRestart: isShell && !closed,
        }
      : null}
    onRename={t ? () => (renamingTab = t.id) : undefined}
    onConfigure={t
      ? () => {
          // V1.4-07 A: AI tabs route Configure to the Settings window
          // scrolled to that tab's section. Shell tabs keep using the
          // shell-only ConfigureTabDialog. The dialog's `getShellTabConfig`
          // / `reconfigureShellTab` IPC pair is shell-specific; AI tabs
          // get their full per-tab edit surface (env, command, args,
          // theme/background overrides, etc.) via Settings → Tabs.
          const meta = tabMeta(t.id);
          if (meta?.kind === 'ai-tool') {
            void openSettingsWindowToTab(t.id);
          } else {
            openConfigureTabDialog(t.id);
          }
        }
      : undefined}
    onRestart={t ? () => onRestartTab(t.id) : undefined}
    onClose={t ? () => onCloseTab(t.id) : undefined}
    onNewWorktreeTab={t && !isShell && !isPreview ? () => openNewWorktreeTabDialog(t.id, pane.id) : undefined}
    onNewPreviewTab={onNewPreviewTabAction}
    onDismiss={dismissMenu}
  />
{/if}

<!-- M-22: `row` / `reduced` are the DERIVED values, not the click-time copies,
     so an open popover follows the tab's real state instead of freezing the
     instant it was opened. `TaintMenu` already renders entirely from its props,
     so it needed no change for this. -->
{#if commMenu}
  <DelegationPopover
    x={commMenu.x}
    y={commMenu.y}
    tab={commMenu.tab}
    tabName={commMenuName}
    access={commMenuAccess}
    onDismiss={() => (commMenu = null)}
  />
{/if}

{#if taintMenu && taintMenuRow}
  <TaintMenu
    x={taintMenu.x}
    y={taintMenu.y}
    row={taintMenuRow}
    reduced={taintMenuReduced}
    protection={taintMenuProtection}
    masterOn={$injectionStatus?.protection ?? true}
    onDismiss={() => (taintMenu = null)}
    onApplied={refreshLatches}
  />
{/if}

<style>
  .tab-bar {
    display: flex;
    flex-direction: row;
    height: 32px;
    background: var(--surface-2);
    border-bottom: 1px solid var(--border-subtle);
    flex: 0 0 32px;
    /* Outer is non-scrolling; .tab-list owns the horizontal scroll so
       the + button stays pinned at the right and the bottom border
       isn't fragmented by a scrollbar gutter. */
    overflow: hidden;
    position: relative;
  }
  /* Non-scrolling wrapper around the scrollable list. It exists so the
     edge-fade pseudo-elements have a positioned ancestor whose edges
     coincide exactly with the list's visible edges — no magic offsets
     that would drift when the end-zone (arrows + `+` button) changes
     width. */
  .tab-list-wrap {
    position: relative;
    display: flex;
    flex: 1 1 auto;
    min-width: 0;
    overflow: hidden;
  }
  .tab-list {
    display: flex;
    flex-direction: row;
    flex: 1 1 auto;
    min-width: 0;
    overflow-x: auto;
    overflow-y: hidden;
    /* No scrollbar: it resized the (deliberately compact) tabs and broke
       the TUI look. Overflow is signalled by the edge fades and scrolled
       via the arrow buttons next to the `+` (plus wheel / drag-to-edge). */
    scrollbar-width: none;
  }
  .tab-list::-webkit-scrollbar {
    display: none;
  }
  /* Edge-fade gradients: pseudo-elements on the non-scrolling wrapper so
     they don't move with scroll. Visibility is class-toggled by the
     reactive scroll-state effect, so a fully in-view bar shows neither
     fade. The gradients fade out the leftmost / rightmost few px of tabs,
     telling the user "more content here". */
  .tab-list-wrap.fade-left::before,
  .tab-list-wrap.fade-right::after {
    content: '';
    position: absolute;
    top: 0;
    bottom: 0;
    width: 16px;
    pointer-events: none;
    z-index: 1;
  }
  .tab-list-wrap.fade-left::before {
    left: 0;
    background: linear-gradient(to right, var(--surface-2), transparent);
  }
  .tab-list-wrap.fade-right::after {
    right: 0;
    background: linear-gradient(to left, var(--surface-2), transparent);
  }
  .tab-bar-end {
    display: flex;
    flex: 0 0 auto;
    border-left: 1px solid var(--border-subtle);
  }
  .scroll-arrow {
    appearance: none;
    border: none;
    background: transparent;
    color: var(--text-tertiary);
    font-size: 16px;
    width: 20px;
    height: 100%;
    cursor: pointer;
    line-height: 30px;
    padding: 0;
    user-select: none;
    transition:
      background var(--motion-fast) var(--easing-standard),
      color var(--motion-fast) var(--easing-standard);
  }
  /* The gap the user reads as "two characters" between the arrows and
     the `+` button. */
  .scroll-arrow-right {
    margin-right: 2ch;
  }
  .scroll-arrow:hover:not(:disabled) {
    background: var(--surface-3);
    color: var(--text-primary);
  }
  .scroll-arrow:disabled {
    color: var(--text-disabled);
    cursor: default;
  }
  .scroll-arrow:focus-visible {
    outline: 2px solid var(--accent);
    outline-offset: -2px;
  }
  .new-tab {
    appearance: none;
    border: none;
    background: transparent;
    color: var(--text-tertiary);
    font-size: 18px;
    width: 32px;
    height: 100%;
    cursor: pointer;
    line-height: 30px;
    padding: 0;
    user-select: none;
    transition:
      background var(--motion-fast) var(--easing-standard),
      color var(--motion-fast) var(--easing-standard);
  }
  .new-tab:hover {
    background: var(--surface-3);
    color: var(--text-primary);
  }
  .new-tab:focus-visible {
    outline: 2px solid var(--accent);
    outline-offset: -2px;
  }
</style>
