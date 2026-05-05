<script lang="ts">
  // Mute toggle for the bottom status bar. Bound to `tts.mute` in settings;
  // the audio thread folds mute into volume (volume = 0 when muted), so
  // toggling here is sufficient — no separate audio command needed.
  import { get } from 'svelte/store';
  import { settings, applySettings } from '../settings/store';

  function toggle() {
    const s = get(settings);
    void applySettings({ ...s, tts: { ...s.tts, mute: !s.tts.mute } });
  }
</script>

<button
  type="button"
  class="status-button"
  onclick={toggle}
  title={$settings.tts.mute ? 'Unmute TTS' : 'Mute TTS'}
  aria-pressed={$settings.tts.mute}
>
  {#if $settings.tts.mute}
    <span class="glyph muted" aria-hidden="true">🔇</span>
  {:else}
    <span class="glyph" aria-hidden="true">🔊</span>
  {/if}
</button>

<style>
  .status-button {
    appearance: none;
    background: transparent;
    border: none;
    color: #c0c0c0;
    cursor: pointer;
    width: 24px;
    height: 24px;
    border-radius: 4px;
    padding: 0;
    display: inline-flex;
    align-items: center;
    justify-content: center;
    line-height: 1;
  }
  .status-button:hover {
    background: #303030;
    color: #ffffff;
  }
  .glyph {
    font-size: 14px;
  }
  .glyph.muted {
    opacity: 0.55;
  }
</style>
