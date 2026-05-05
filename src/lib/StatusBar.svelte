<script lang="ts">
  // Bottom status bar (V2-04). Thin horizontal strip below the terminal
  // area; right side hosts mute / announcements / volume; left side is
  // reserved for future text. Live-updates from the settings store, so
  // changes elsewhere (settings window) reflect here and vice versa.
  import MuteButton from './status/MuteButton.svelte';
  import AnnouncementsButton from './status/AnnouncementsButton.svelte';
  import VolumeSlider from './status/VolumeSlider.svelte';
</script>

<div class="status-bar">
  <div class="status-bar-left"></div>
  <div class="status-bar-right">
    <MuteButton />
    <AnnouncementsButton />
    <VolumeSlider />
  </div>
</div>

<style>
  .status-bar {
    display: flex;
    flex-direction: row;
    align-items: center;
    justify-content: space-between;
    height: 28px;
    flex: 0 0 28px;
    background: #1a1a1a;
    border-top: 1px solid #444;
    padding: 0 8px;
    box-sizing: border-box;
  }
  .status-bar-left {
    flex: 1 1 auto;
  }
  .status-bar-right {
    display: inline-flex;
    flex-direction: row;
    align-items: center;
    gap: 8px;
    flex: 0 0 auto;
  }
</style>
