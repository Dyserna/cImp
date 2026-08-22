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
  import SandboxBadge from './status/SandboxBadge.svelte';
  import DelegationBadge from './status/DelegationBadge.svelte';
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
    <!-- V39 — the SECURITY section: the three switches that decide what a
         running agent may do, in the order they take effect.

         ⛨ injection protection (L1) — what content may reach a model and what a
         latched session may call. ▣ sandboxing — whether the processes cImp
         starts for the agent are confined by the OS. ⇅ sandbox network —
         whether those confined processes get egress.

         They are a section of their own, bracketed like the visibility/settings
         group, because they are permanent CONTROLS rather than conditional
         indicators: each states its value and each flips on click. That is a
         change from the pre-V39 shield, which was silent while everything was on
         — see `InjectionBadge.svelte` for why a surface that goes quiet when it
         is happy cannot be told apart from one that is broken. -->
    <span class="sep" aria-hidden="true"></span>
    <InjectionBadge />
    <SandboxBadge kind="sandbox" />
    <SandboxBadge kind="network" />
    <DelegationBadge />
    <span class="sep" aria-hidden="true"></span>
    <TabVisibilityButton />
    <SettingsButton />
  </div>
</div>

<style>
  /* Height = one 22px row per stacked usage row the running harness's usage
     source declares (V40 Phase D, locked decision 19). `--status-bar-rows` is
     set by `UsageMeter` from `harness_usage`'s declared window count; the `2`
     here is the fallback for every state in which nothing has declared one
     yet — which is what the hard-coded 44px used to be, so the strip never
     reflows on startup. */
  .status-bar {
    display: flex;
    flex-direction: row;
    align-items: center;
    justify-content: space-between;
    height: calc(var(--status-bar-rows, 2) * 22px);
    flex: 0 0 calc(var(--status-bar-rows, 2) * 22px);
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
