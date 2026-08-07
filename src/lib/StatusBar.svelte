<script lang="ts">
  // Bottom status bar (V2-04). Thin horizontal strip below the terminal
  // area; right side hosts mute / announcements / volume; left side is
  // reserved for future text. Live-updates from the settings store, so
  // changes elsewhere (settings window) reflect here and vice versa.
  import MuteButton from './status/MuteButton.svelte';
  import RecordButton from './status/RecordButton.svelte';
  import AnnouncementsButton from './status/AnnouncementsButton.svelte';
  import VolumeSlider from './status/VolumeSlider.svelte';
  import SettingsButton from './status/SettingsButton.svelte';
  import TabVisibilityButton from './status/TabVisibilityButton.svelte';
  import StatusBarArrangement from './status/StatusBarArrangement.svelte';
  import SelectionTtsControls from './status/SelectionTtsControls.svelte';
  import ToolLaunchButton from './status/ToolLaunchButton.svelte';
  import NoteButton from './status/NoteButton.svelte';
  import WorkbenchDiffBadge from './status/WorkbenchDiffBadge.svelte';
  import InjectionBadge from './status/InjectionBadge.svelte';
  import { settings } from './settings/store';
</script>

<div class="status-bar">
  <StatusBarArrangement />
  <div class="status-bar-right">
    <span class="sep" aria-hidden="true"></span>
    <ToolLaunchButton tool="broot" glyph="🌳" label="New broot tab" />
    <ToolLaunchButton tool="rustnet" glyph="🌐" label="New rustnet tab" />
    <NoteButton />
    <WorkbenchDiffBadge />
    <!-- V32 Phase G: silent while every injection control is on; a ⛨ chip when
         the master switch or any feature is off, so a reduced-protection state
         cannot be off and forgotten (locked decision 16). -->
    <InjectionBadge />
    {#if $settings.stt.enabled}
      <span class="sep" aria-hidden="true"></span>
      <RecordButton />
    {/if}
    <!-- TTS playback controls only make sense when TTS is enabled (the model is
         loaded); they hide with the feature, mirroring the record button. -->
    {#if $settings.tts.enabled}
      <span class="sep" aria-hidden="true"></span>
      {#if $settings.tts.show_selection_controls}
        <SelectionTtsControls />
      {/if}
      <VolumeSlider />
      <MuteButton />
      <AnnouncementsButton />
    {/if}
    <span class="sep" aria-hidden="true"></span>
    <TabVisibilityButton />
    <SettingsButton />
  </div>
</div>

<style>
  .status-bar {
    display: flex;
    flex-direction: row;
    align-items: center;
    justify-content: space-between;
    height: 44px;
    flex: 0 0 44px;
    background: var(--surface-sunken);
    border-top: 1px solid var(--border-subtle);
    padding: 0 var(--space-3);
    box-sizing: border-box;
  }
  .status-bar-right {
    display: inline-flex;
    flex-direction: row;
    align-items: center;
    gap: var(--space-2);
    flex: 0 0 auto;
  }
  /* Group divider — same hairline as the one that used to sit after the
     selection-TTS stop button. */
  .sep {
    width: 0;
    height: 22px;
    border-right: 1px solid var(--border-subtle);
  }
</style>
