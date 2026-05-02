<script lang="ts">
  import Terminal from './lib/Terminal.svelte';
  import AvatarOverlay from './lib/AvatarOverlay.svelte';
  import WaveformOverlay from './lib/WaveformOverlay.svelte';
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
