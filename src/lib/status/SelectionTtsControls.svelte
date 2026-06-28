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
  $: playing = $selectionTtsState === 'playing';
  $: paused = $selectionTtsState === 'paused';
  $: active = playing || paused;
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
  .status-button:hover:not(:disabled) {
    background: var(--surface-3);
    color: var(--text-primary);
  }
  .status-button:disabled {
    opacity: 0.4;
    cursor: default;
  }
  .status-button:focus-visible {
    outline: 2px solid var(--accent);
    outline-offset: 2px;
  }
  .glyph {
    font-size: 13px;
  }
</style>
