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
    border: 1px solid transparent;
    color: var(--text-secondary);
    cursor: pointer;
    width: 26px;
    height: 22px;
    border-radius: var(--radius-pill);
    padding: 0;
    display: inline-flex;
    align-items: center;
    justify-content: center;
    line-height: 1;
    transition:
      background var(--motion-fast) var(--easing-standard),
      color var(--motion-fast) var(--easing-standard),
      border-color var(--motion-fast) var(--easing-standard);
  }
  .status-button:hover:not([aria-pressed="true"]) {
    background: var(--surface-3);
    color: var(--text-primary);
  }
  .status-button[aria-pressed="true"] {
    background: var(--accent-muted);
    border-color: var(--accent);
    color: var(--accent);
  }
  .status-button:focus-visible {
    outline: 2px solid var(--accent);
    outline-offset: 2px;
  }
  .glyph {
    font-size: 14px;
  }
  .glyph.muted {
    opacity: 1;
  }
</style>
