<script lang="ts">
  // Bottom-bar volume slider. Bound to `tts.volume` (0..1). The audio
  // thread already subscribes to settings updates; pushing through
  // `applySettings` is enough — the backend's debounced saver coalesces
  // rapid drag updates so we don't need to throttle in the UI.
  import { get } from 'svelte/store';
  import { settings, applySettings } from '../settings/store';

  function onInput(e: Event) {
    const target = e.currentTarget as HTMLInputElement;
    const v = parseFloat(target.value);
    if (!Number.isFinite(v)) return;
    const s = get(settings);
    void applySettings({ ...s, tts: { ...s.tts, volume: v } });
  }
</script>

<div class="volume-control" title="Volume">
  <span class="glyph" aria-hidden="true">🔉</span>
  <input
    type="range"
    min="0"
    max="1"
    step="0.01"
    value={$settings.tts.volume}
    oninput={onInput}
    aria-label="Volume"
  />
</div>

<style>
  .volume-control {
    display: inline-flex;
    align-items: center;
    gap: var(--space-1);
    color: var(--text-secondary);
  }
  .glyph {
    font-size: 14px;
    line-height: 1;
  }
  input[type='range'] {
    width: 90px;
    height: 16px;
    accent-color: var(--accent);
    cursor: pointer;
  }
</style>
