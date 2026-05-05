<script lang="ts">
  import Tab from './Tab.svelte';
  import { activeTab, switchTab } from './tabs/state';
  import { TAB_META } from './tabs/types';
  import {
    perTabAvatarState,
    perTabAwaitingPermission,
    perTabDoneWhileAway,
  } from './avatarState';
</script>

<div class="tab-bar" role="tablist">
  {#each TAB_META as meta (meta.id)}
    <Tab
      label={meta.label}
      active={$activeTab === meta.id}
      avatarState={$perTabAvatarState[meta.id]}
      awaitingPermission={$perTabAwaitingPermission[meta.id]}
      doneWhileAway={$perTabDoneWhileAway[meta.id]}
      onclick={() => void switchTab(meta.id)}
    />
  {/each}
</div>

<style>
  .tab-bar {
    display: flex;
    flex-direction: row;
    height: 32px;
    background: #2a2a2a;
    border-bottom: 1px solid #444;
    flex: 0 0 32px;
  }
</style>
