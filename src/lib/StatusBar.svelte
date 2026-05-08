<script lang="ts">
  // Bottom status bar (V2-04). Thin horizontal strip below the terminal
  // area; right side hosts mute / announcements / volume; left side is
  // reserved for future text. Live-updates from the settings store, so
  // changes elsewhere (settings window) reflect here and vice versa.
  import MuteButton from './status/MuteButton.svelte';
  import AnnouncementsButton from './status/AnnouncementsButton.svelte';
  import VolumeSlider from './status/VolumeSlider.svelte';
  import LayoutsPopover from './status/LayoutsPopover.svelte';
  import SettingsButton from './status/SettingsButton.svelte';
  import AvatarToggleButton from './status/AvatarToggleButton.svelte';
</script>

<div class="status-bar">
  <div class="status-bar-left">
    <LayoutsPopover />
  </div>
  <div class="status-bar-right">
    <AvatarToggleButton />
    <MuteButton />
    <AnnouncementsButton />
    <VolumeSlider />
    <SettingsButton />
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
    background: var(--surface-sunken);
    border-top: 1px solid var(--border-subtle);
    padding: 0 var(--space-3);
    box-sizing: border-box;
  }
  .status-bar-left {
    flex: 1 1 auto;
  }
  .status-bar-right {
    display: inline-flex;
    flex-direction: row;
    align-items: center;
    gap: var(--space-2);
    flex: 0 0 auto;
  }
</style>
