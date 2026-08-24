<script lang="ts">
  // Bottom-bar transport for selection TTS: play / pause / restart / stop.
  //   Play    — read the current terminal selection (same effect as
  //             Ctrl+right-click), or resume when paused. Toasts if nothing
  //             is selected.
  //   Pause   — pause in-flight playback.
  //   Restart — replay the current read from its first sentence.
  //   Stop    — stop and clear the highlight (the Esc equivalent).
  // The whole section is gated by `tts.show_selection_controls` (StatusBar
  // decides whether to mount it).
  import {
    selectionTtsState,
    playSelectionTts,
    pauseSelectionTts,
    restartSelectionTts,
    stopSelectionTts,
  } from '../selectionTts';

  // $selectionTtsState is 'idle' | 'playing' | 'paused'.
  const playing = $derived($selectionTtsState === 'playing');
  const paused = $derived($selectionTtsState === 'paused');
  const active = $derived(playing || paused);
</script>

<div class="selection-tts" role="group" aria-label="Selection text-to-speech controls">
  <button
    type="button"
    class="status-button"
    onclick={() => playSelectionTts()}
    disabled={playing}
    title={paused ? 'Resume reading' : 'Read selection aloud'}
  >
    <span class="glyph" aria-hidden="true">▶</span>
  </button>
  <button
    type="button"
    class="status-button"
    onclick={() => void pauseSelectionTts()}
    disabled={!playing}
    title="Pause reading"
  >
    <span class="glyph" aria-hidden="true">⏸</span>
  </button>
  <button
    type="button"
    class="status-button"
    onclick={() => void restartSelectionTts()}
    disabled={!active}
    title="Restart from the beginning"
  >
    <span class="glyph" aria-hidden="true">↺</span>
  </button>
  <button
    type="button"
    class="status-button"
    onclick={() => void stopSelectionTts()}
    disabled={!active}
    title="Stop reading"
  >
    <span class="glyph" aria-hidden="true">⏹</span>
  </button>
</div>

<style>
  .selection-tts {
    display: inline-flex;
    flex-direction: row;
    align-items: center;
    gap: var(--space-1);
  }
  /* Shell + focus ring: `.status-button` in `src/app.css`. State only here. */
  .status-button:hover:not(:disabled) {
    background: var(--surface-3);
    color: var(--text-primary);
  }
  .status-button:disabled {
    opacity: 0.4;
    cursor: default;
  }
  .glyph {
    font-size: 13px;
  }
</style>
