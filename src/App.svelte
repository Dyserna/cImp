<script lang="ts">
  import { onMount } from 'svelte';
  import Terminal from './lib/Terminal.svelte';
  import AvatarOverlay from './lib/AvatarOverlay.svelte';
  import WaveformOverlay from './lib/WaveformOverlay.svelte';
  import { initSettings, settings } from './lib/settings/store';
  import { openSettingsWindow } from './lib/settings/ipc';
  import {
    configureShortcuts,
    installDispatcher,
  } from './lib/shortcuts/dispatcher';

  // Bootstrap: pull settings, install the global keydown dispatcher, then
  // re-configure on every settings change. Compose-related shortcuts are
  // registered with no-op handlers in M6 (the compose overlay arrives in
  // M7); only `open_settings` actually fires here. The handlers map is
  // re-installed on each settings change so the latest predicates apply.
  let unsubscribe: (() => void) | undefined;

  onMount(() => {
    void (async () => {
      await initSettings();
      installDispatcher();
      unsubscribe = settings.subscribe((s) => {
        configureShortcuts(s.shortcuts, {
          open_settings: () => {
            void openSettingsWindow();
          },
        });
      });
    })();
    return () => {
      unsubscribe?.();
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
