<script lang="ts">
  import Tab from './Tab.svelte';
  import TabContextMenu from './TabContextMenu.svelte';
  import { activeTab, switchTab } from './tabs/state';
  import { tabs } from './tabs/store';
  import {
    perTabAvatarState,
    perTabAwaitingPermission,
    perTabClosedState,
    perTabDoneWhileAway,
  } from './avatarState';
  import { closeTab as closeTabIpc, renameTab as renameTabIpc } from './ipc';
  import { openConfigureTabDialog, openNewShellTabDialog } from './dialog/store';
  import type { TabId } from './tabs/types';

  // Per-tab rename mode flag. The `Tab` component two-way binds to this
  // value; flipping it true here (on the context menu's Rename action)
  // puts that tab into rename mode.
  let renamingTab = $state<TabId | null>(null);

  // Live context-menu state. Coordinates pin it to the cursor; the
  // builtin flag drives kind-aware entries.
  let menu = $state<{ tab: TabId; builtin: boolean; x: number; y: number } | null>(null);

  function onCloseTab(tab: TabId): void {
    void closeTabIpc(tab).catch((e) => {
      console.error('close_tab failed:', e);
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
</script>

<div class="tab-bar" role="tablist">
  {#each $tabs as meta (meta.id)}
    <Tab
      label={meta.name}
      active={$activeTab === meta.id}
      builtin={meta.builtin}
      canSkipCloseConfirm={$perTabClosedState[meta.id]?.closed ?? false}
      avatarState={$perTabAvatarState[meta.id] ?? 'Idle'}
      awaitingPermission={$perTabAwaitingPermission[meta.id] ?? false}
      doneWhileAway={$perTabDoneWhileAway[meta.id] ?? false}
      bind:renaming={
        () => renamingTab === meta.id,
        (v) => {
          if (v) renamingTab = meta.id;
          else if (renamingTab === meta.id) renamingTab = null;
        }
      }
      onclick={() => void switchTab(meta.id)}
      onclose={meta.builtin ? undefined : () => onCloseTab(meta.id)}
      oncontextmenu={(e) => onTabContextMenu(meta.id, meta.builtin, e)}
      onrename={(newName) => onRenameTab(meta.id, newName)}
    />
  {/each}
  <button
    type="button"
    class="new-tab"
    aria-label="New shell tab"
    title="New shell tab (Ctrl+T)"
    onclick={openNewShellTabDialog}
  >
    +
  </button>
</div>

{#if menu}
  <TabContextMenu
    x={menu.x}
    y={menu.y}
    builtin={menu.builtin}
    onRename={() => {
      if (menu) renamingTab = menu.tab;
    }}
    onConfigure={() => {
      if (menu) openConfigureTabDialog(menu.tab);
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
