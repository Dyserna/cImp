<script lang="ts">
  // Bottom-bar dictation record button (V6-01). Honors `stt.button_mode`:
  //   - toggle (default): click to start, click again to stop.
  //   - hold: press-and-hold to record, release to stop. The hold path is
  //     shared with push-to-talk (both call startRecording/stopRecording).
  // Visual states track the `sttState` store: idle (mic), recording (pulsing
  // red, aria-pressed), transcribing (spinner, disabled).
  import { get } from 'svelte/store';
  import { stt as sttSettings } from '../settings/store';
  import { sttState, startRecording, stopRecording } from '../stt';

  function start() {
    if (get(sttState) === 'idle' || get(sttState) === 'error') {
      void startRecording();
    }
  }

  function stop() {
    if (get(sttState) === 'recording') {
      void stopRecording();
    }
  }

  // Toggle-mode click handler.
  function onClick() {
    if ($sttSettings.button_mode !== 'toggle') return;
    const s = get(sttState);
    if (s === 'recording') stop();
    else if (s === 'idle' || s === 'error') start();
    // 'transcribing' → disabled, no-op
  }

  // Hold-mode pointer handlers.
  function onPointerDown() {
    if ($sttSettings.button_mode !== 'hold') return;
    start();
  }
  function onPointerUp() {
    if ($sttSettings.button_mode !== 'hold') return;
    stop();
  }

  const recording = $derived($sttState === 'recording');
  const transcribing = $derived($sttState === 'transcribing');

  const title = $derived(
    transcribing
      ? 'Transcribing…'
      : recording
        ? 'Stop recording'
        : $sttSettings.button_mode === 'hold'
          ? 'Hold to dictate'
          : 'Start dictation',
  );
</script>

<button
  type="button"
  class="status-button"
  class:recording
  onclick={onClick}
  onpointerdown={onPointerDown}
  onpointerup={onPointerUp}
  onpointerleave={onPointerUp}
  disabled={transcribing}
  {title}
  aria-label={title}
  aria-pressed={recording}
>
  {#if transcribing}
    <span class="glyph spinner" aria-hidden="true">◐</span>
  {:else}
    <span class="glyph" aria-hidden="true">🎤</span>
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
  .status-button:hover:not([aria-pressed='true']):not(:disabled) {
    background: var(--surface-3);
    color: var(--text-primary);
  }
  .status-button:disabled {
    cursor: default;
    opacity: 0.7;
  }
  .status-button.recording {
    background: var(--accent-muted);
    border-color: #ff5555;
    color: #ff5555;
    animation: stt-pulse 1.1s ease-in-out infinite;
  }
  .status-button:focus-visible {
    outline: 2px solid var(--accent);
    outline-offset: 2px;
  }
  .glyph {
    font-size: 14px;
  }
  .glyph.spinner {
    animation: stt-spin 0.9s linear infinite;
  }
  @keyframes stt-pulse {
    0%,
    100% {
      box-shadow: 0 0 0 0 rgba(255, 85, 85, 0.45);
    }
    50% {
      box-shadow: 0 0 0 4px rgba(255, 85, 85, 0);
    }
  }
  @keyframes stt-spin {
    from {
      transform: rotate(0deg);
    }
    to {
      transform: rotate(360deg);
    }
  }
</style>
