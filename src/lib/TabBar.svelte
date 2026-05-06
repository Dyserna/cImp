<script lang="ts">
  import Tab from './Tab.svelte';
  import TabContextMenu from './TabContextMenu.svelte';
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
    renameTab as renameTabIpc,
    restartShellTab,
  } from './ipc';
  import { openConfigureTabDialog, openNewShellTabDialog } from './dialog/store';
  import { isShellTab, type TabId } from './tabs/types';
  import { requestTabIntoPane, setPaneActiveTab } from './layout/store';
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

  let menu = $state<{ tab: TabId; builtin: boolean; x: number; y: number } | null>(null);

  // The tab bar's root element, registered with the DOM registry so
  // the M2 drop-target hit-tester can distinguish "drop on tab bar"
  // (reorder / move-to-pane) from "drop in content area" (split). The
  // registry stores the element; rects are read live so resizes and
  // splitter moves stay correct without re-registering.
  let barEl: HTMLDivElement | undefined = $state();
  $effect(() => {
    if (!barEl) return;
    const id = pane.id;
    paneRegistry.setTabBarElement(id, barEl);
    return () => {
      paneRegistry.setTabBarElement(id, null);
    };
  });

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
    menu = { tab, builtin, x: e.clientX, y: e.clientY };
  }

  function dismissMenu(): void {
    menu = null;
  }

  function onNewShellTab(): void {
    requestTabIntoPane(pane.id);
    openNewShellTabDialog();
  }
</script>

<div class="tab-bar" role="tablist" bind:this={barEl}>
  {#each pane.tab_ids as id (id)}
    {@const meta = $tabs.find((m) => m.id === id)}
    {#if meta}
      <Tab
        tabId={id}
        label={meta.name}
        active={pane.active_tab_id === id}
        builtin={meta.builtin}
        canSkipCloseConfirm={$perTabClosedState[id]?.closed ?? false}
        avatarState={$perTabAvatarState[id] ?? 'Idle'}
        awaitingPermission={$perTabAwaitingPermission[id] ?? false}
        doneWhileAway={$perTabDoneWhileAway[id] ?? false}
        bind:renaming={
          () => renamingTab === id,
          (v) => {
            if (v) renamingTab = id;
            else if (renamingTab === id) renamingTab = null;
          }
        }
        onclick={() => onTabClick(id)}
        onclose={meta.builtin ? undefined : () => onCloseTab(id)}
        oncontextmenu={(e) => onTabContextMenu(id, meta.builtin, e)}
        onpointerdowndrag={(e) => beginDrag(id, pane.id, e)}
        onrename={(newName) => onRenameTab(id, newName)}
      />
    {/if}
  {/each}
  <button
    type="button"
    class="new-tab"
    aria-label="New shell tab"
    title="New shell tab (Ctrl+T)"
    onclick={onNewShellTab}
  >
    +
  </button>
</div>

{#if menu}
  {@const isShell = isShellTab(menu.tab)}
  {@const closed = $perTabClosedState[menu.tab]?.closed ?? false}
  <TabContextMenu
    x={menu.x}
    y={menu.y}
    builtin={menu.builtin}
    canRestart={isShell && !closed}
    onRename={() => {
      if (menu) renamingTab = menu.tab;
    }}
    onConfigure={() => {
      if (menu) openConfigureTabDialog(menu.tab);
    }}
    onRestart={() => {
      if (menu) onRestartTab(menu.tab);
    }}
    onClose={() => {
      if (menu) onCloseTab(menu.tab);
    }}
    onDismiss={dismissMenu}
  />
{/if}

<style>
  .tab-bar {
    display: flex;
    flex-direction: row;
    height: 32px;
    background: #2a2a2a;
    border-bottom: 1px solid #444;
    flex: 0 0 32px;
  }
  .new-tab {
    appearance: none;
    border: none;
    background: transparent;
    color: #888;
    font-size: 18px;
    width: 32px;
    height: 100%;
    cursor: pointer;
    line-height: 30px;
    padding: 0;
    user-select: none;
    border-right: 1px solid #2a2a2a;
  }
  .new-tab:hover {
    background: #303030;
    color: #e0e0e0;
  }
</style>
