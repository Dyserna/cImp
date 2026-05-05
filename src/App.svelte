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
  import { avatarState, startAvatarStateListener } from './lib/avatarState';
  import { initSettings, settings } from './lib/settings/store';
  import { openSettingsWindow } from './lib/settings/ipc';
  import { activeTab, switchTab } from './lib/tabs/state';
  import { ALL_TABS } from './lib/tabs/types';
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
      // Start the backend state listener early — it drives both the
      // per-tab avatar cache AND the activeTab store (via the
      // ActiveTabChanged event), so it must run regardless of whether
      // the avatar overlay is mounted/visible.
      void startAvatarStateListener().catch((e) =>
        console.error('startAvatarStateListener failed:', e),
      );
      installDispatcher();
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
          switch_to_tab_1: () => void switchTab('claude'),
          switch_to_tab_2: () => void switchTab('aider'),
        });
      });
      // Window title reflects the active tab's avatar state. Switching
      // tabs re-derives the avatar state, so this listener picks that up
      // automatically.
      const win = getCurrentWindow();
      unsubTitle = avatarState.subscribe((s) => {
        const tab = get(activeTab);
        const tabLabel = tab === 'claude' ? 'Claude' : 'Aider';
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
    {#each ALL_TABS as tab (tab)}
      <div class="terminal-pane" class:hidden={$activeTab !== tab}>
        <Terminal tabId={tab} />
      </div>
    {/each}
    <AvatarOverlay />
    <WaveformOverlay />
    <ComposeOverlay />
    <ErrorBanner />
    <AiderFirstLaunchNotice />
  </div>
  <StatusBar />
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
