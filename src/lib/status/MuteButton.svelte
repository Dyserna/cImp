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
  /* Shell + focus ring: `.status-button` in `src/app.css`. State only here. */
  .status-button:hover:not([aria-pressed="true"]) {
    background: var(--surface-3);
    color: var(--text-primary);
  }
  .status-button[aria-pressed="true"] {
    background: var(--accent-muted);
    border-color: var(--accent);
    color: var(--accent);
  }
  .glyph {
    font-size: 14px;
  }
  .glyph.muted {
    opacity: 1;
  }
</style>
