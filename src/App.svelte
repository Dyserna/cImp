<script lang="ts">
  import { onMount } from 'svelte';
  import { get } from 'svelte/store';
  import { getCurrentWindow } from '@tauri-apps/api/window';
  import Terminal from './lib/Terminal.svelte';
  import TabBar from './lib/TabBar.svelte';
  import StatusBar from './lib/StatusBar.svelte';
  import AvatarOverlay from './lib/AvatarOverlay.svelte';
  import WaveformOverlay from './lib/WaveformOverlay.svelte';
  import ComposeOverlay from './lib/ComposeOverlay.svelte';
  import ErrorBanner from './lib/ErrorBanner.svelte';
  import AiderFirstLaunchNotice from './lib/AiderFirstLaunchNotice.svelte';
  import NewShellTabDialog from './lib/dialog/NewShellTabDialog.svelte';
  import ConfigureTabDialog from './lib/dialog/ConfigureTabDialog.svelte';
  import Toast from './lib/Toast.svelte';
  import { dialogState, openNewShellTabDialog } from './lib/dialog/store';
  import { closeTab as closeTabIpc } from './lib/ipc';
  import { showToast } from './lib/toast';
  import {
    avatarState,
    seedPerTabEntries,
    startAvatarStateListener,
  } from './lib/avatarState';
  import { initSettings, settings } from './lib/settings/store';
  import { openSettingsWindow } from './lib/settings/ipc';
  import { activeTab, switchTab } from './lib/tabs/state';
  import { applyTabCreated, tabMeta, tabs } from './lib/tabs/store';
  import { listTabs } from './lib/ipc';
  import {
    configureShortcuts,
    installDispatcher,
  } from './lib/shortcuts/dispatcher';
  import {
    composeOpen,
    composeFocused,
    composeContent,
    openCompose,
    closeCompose,
    submitCompose,
  } from './lib/composeState';
  import { composeContentChanged } from './lib/ipc';

  let unsubSettings: (() => void) | undefined;
  let unsubContent: (() => void) | undefined;
  let unsubTitle: (() => void) | undefined;

  onMount(() => {
    void (async () => {
      await initSettings();
      // Seed the tabs store from a synchronous snapshot before attaching
      // the avatar-state listener. Event-driven add/remove still updates
      // the store at runtime; the snapshot just guarantees the launch
      // tabs are present even if backend TabCreated emissions raced the
      // webview mount. Idempotent: events for tabs already in the store
      // overwrite name/kind in place.
      try {
        const snapshot = await listTabs();
        snapshot.forEach((m, position) => {
          seedPerTabEntries(m.id);
          applyTabCreated({
            tab: m.id,
            kind: m.kind,
            name: m.name,
            builtin: m.builtin,
            position,
          });
        });
      } catch (e) {
        console.error('list_tabs failed:', e);
      }
      // Start the backend state listener early — it drives both the
      // per-tab avatar cache AND the activeTab store (via the
      // ActiveTabChanged event), so it must run regardless of whether
      // the avatar overlay is mounted/visible.
      void startAvatarStateListener().catch((e) =>
        console.error('startAvatarStateListener failed:', e),
      );
      installDispatcher();
      // Position-bound tab-switch handler: 1-indexed lookup against the
      // live tabs store. No-op when fewer than N tabs exist.
      const switchToPosition = (n: number) => () => {
        const list = get(tabs);
        const target = list[n - 1];
        if (target) void switchTab(target.id);
      };
      // Active-tab close handler. Builtins surface a transient toast
      // since closing them is rejected by the backend; the toast keeps
      // the keystroke from feeling like a no-op.
      const closeActiveTab = () => {
        if (get(dialogState).kind !== 'none') return;
        const tab = get(activeTab);
        void closeTabIpc(tab).catch((e) => {
          const wire = e as { kind?: string } | string | null;
          if (wire && typeof wire === 'object' && 'kind' in wire) {
            if (wire.kind === 'builtin-not-closable') {
              showToast('This tab cannot be closed.');
              return;
            }
          }
          console.error('close_tab failed:', e);
        });
      };
      unsubSettings = settings.subscribe((s) => {
        configureShortcuts(s.shortcuts, {
          open_compose: openCompose,
          submit_compose: {
            handler: () => {
              void submitCompose();
            },
            active: () => get(composeFocused),
          },
          cancel_compose: {
            handler: closeCompose,
            active: () => get(composeOpen),
          },
          open_settings: () => {
            void openSettingsWindow();
          },
          switch_to_tab_1: switchToPosition(1),
          switch_to_tab_2: switchToPosition(2),
          switch_to_tab_3: switchToPosition(3),
          switch_to_tab_4: switchToPosition(4),
          switch_to_tab_5: switchToPosition(5),
          switch_to_tab_6: switchToPosition(6),
          switch_to_tab_7: switchToPosition(7),
          switch_to_tab_8: switchToPosition(8),
          switch_to_tab_9: switchToPosition(9),
          new_shell_tab: openNewShellTabDialog,
          close_tab: closeActiveTab,
        });
      });
      // Window title reflects the active tab's avatar state. Switching
      // tabs re-derives the avatar state, so this listener picks that up
      // automatically. The tab name comes from the tabs store so user
      // renames flow through immediately.
      const win = getCurrentWindow();
      unsubTitle = avatarState.subscribe((s) => {
        const tab = get(activeTab);
        const meta = tabMeta(tab);
        const tabLabel = meta?.name ?? tab;
        const label = s === 'Idle' ? tabLabel : `${tabLabel} — ${s}`;
        void win.setTitle(label).catch((e) =>
          console.warn('setTitle failed:', e),
        );
      });

      let lastNonEmpty = false;
      unsubContent = composeContent.subscribe((content) => {
        const nonEmpty = content.length > 0;
        if (nonEmpty !== lastNonEmpty) {
          lastNonEmpty = nonEmpty;
          void composeContentChanged(nonEmpty).catch((e) =>
            console.error('compose_content_changed failed:', e),
          );
        }
      });
    })();
    return () => {
      unsubSettings?.();
      unsubContent?.();
      unsubTitle?.();
    };
  });
</script>

<main>
  <TabBar />
  <!--
    The avatar overlay positions itself absolutely against `.terminal-area`
    rather than the window root so it sits inside the visible terminal
    region (below the tab bar). This is per V2-01 acceptance #8.
  -->
  <div class="terminal-area">
    {#each $tabs as meta (meta.id)}
      <div class="terminal-pane" class:hidden={$activeTab !== meta.id}>
        <Terminal tabId={meta.id} />
      </div>
    {/each}
    <AvatarOverlay />
    <WaveformOverlay />
    <ComposeOverlay />
    <ErrorBanner />
    <AiderFirstLaunchNotice />
  </div>
  <StatusBar />
  <NewShellTabDialog />
  <ConfigureTabDialog />
  <Toast />
</main>

<style>
  :global(html, body) {
    margin: 0;
    padding: 0;
    height: 100%;
    overflow: hidden;
  }
  main {
    position: relative;
    width: 100vw;
    height: 100vh;
    display: flex;
    flex-direction: column;
  }
  .terminal-area {
    position: relative;
    flex: 1 1 auto;
    min-height: 0;
    overflow: hidden;
  }
  .terminal-pane {
    position: absolute;
    inset: 0;
    width: 100%;
    height: 100%;
  }
  .hidden {
    display: none;
  }
</style>
