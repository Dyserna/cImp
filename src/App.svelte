<script lang="ts">
  import Terminal from './lib/Terminal.svelte';
  import AvatarOverlay from './lib/AvatarOverlay.svelte';
</script>

<main>
  <Terminal />
  <AvatarOverlay />
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
