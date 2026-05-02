<script lang="ts">
  import { onMount } from 'svelte';
  import { get } from 'svelte/store';
  import { getCurrentWindow } from '@tauri-apps/api/window';
  import Terminal from './lib/Terminal.svelte';
  import AvatarOverlay from './lib/AvatarOverlay.svelte';
  import WaveformOverlay from './lib/WaveformOverlay.svelte';
  import ComposeOverlay from './lib/ComposeOverlay.svelte';
  import ErrorBanner from './lib/ErrorBanner.svelte';
  import { avatarState } from './lib/avatarState';
  import { initSettings, settings } from './lib/settings/store';
  import { openSettingsWindow } from './lib/settings/ipc';
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

  // Bootstrap: pull settings, install the global keydown dispatcher, then
  // re-configure on every settings change. Compose handlers use the
  // dispatcher's active-predicate form so `Ctrl+Enter` and `Escape` only
  // intercept when contextually relevant (textarea focused / sheet open),
  // otherwise the keypress flows to xterm.js as normal.
  let unsubSettings: (() => void) | undefined;
  let unsubContent: (() => void) | undefined;
  let unsubTitle: (() => void) | undefined;

  onMount(() => {
    void (async () => {
      await initSettings();
      installDispatcher();
      unsubSettings = settings.subscribe((s) => {
        configureShortcuts(s.shortcuts, {
          open_compose: openCompose,
          submit_compose: {
            handler: () => {
              void submitCompose();
            },
            // Only fire when the textarea has DOM focus — otherwise let
            // Ctrl+Enter through to the terminal.
            active: () => get(composeFocused),
          },
          cancel_compose: {
            handler: closeCompose,
            // Only fire while the sheet is open — otherwise let Escape
            // through to xterm.js (which sends ESC to Claude Code).
            active: () => get(composeOpen),
          },
          open_settings: () => {
            void openSettingsWindow();
          },
        });
      });
      // Reflect avatar state in the OS window title so the user can see
      // what's happening in the taskbar / Alt-Tab / window list without
      // looking at the avatar itself.
      const win = getCurrentWindow();
      unsubTitle = avatarState.subscribe((s) => {
        const label = s === 'Idle' ? 'Claude' : `Claude — ${s}`;
        void win.setTitle(label).catch((e) =>
          console.warn('setTitle failed:', e),
        );
      });

      // Edge-trigger the compose-content state-machine signal: emit only
      // when the empty/non-empty state actually flips. Avoids spamming the
      // signal channel on every keystroke.
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
  <Terminal />
  <AvatarOverlay />
  <!-- Sibling, NOT a child of AvatarOverlay: the avatar's CSS opacity must
       not bleed into the waveform. See WaveformOverlay.svelte for the full
       reasoning. -->
  <WaveformOverlay />
  <ComposeOverlay />
  <ErrorBanner />
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
  }
</style>
