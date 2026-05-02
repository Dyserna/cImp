<script lang="ts">
  import { setSuppressed } from '../shortcuts/dispatcher';
  import { formatShortcut } from '../shortcuts/parser';

  // Two-way string slot. Parent manages the underlying setting; this
  // component just renders a button that toggles capture mode.
  let { value = $bindable<string | null>(null) }: { value: string | null } = $props();

  let capturing = $state(false);

  function startCapture() {
    if (capturing) return;
    capturing = true;
    // While capturing, silence the global dispatcher so the user pressing
    // an existing shortcut to bind a new one doesn't fire that handler.
    setSuppressed(true);
    window.addEventListener('keydown', onKeyDown, true);
  }

  function stopCapture() {
    capturing = false;
    setSuppressed(false);
    window.removeEventListener('keydown', onKeyDown, true);
  }

  function onKeyDown(event: KeyboardEvent) {
    if (!capturing) return;
    event.preventDefault();
    event.stopPropagation();
    if (event.key === 'Escape') {
      stopCapture();
      return;
    }
    // Reject pure modifier presses; the user is still composing the chord.
    if (['Control', 'Shift', 'Alt', 'Meta'].includes(event.key)) return;
    value = formatShortcut(event);
    stopCapture();
  }

  function clear() {
    value = null;
  }
</script>

<div class="shortcut-row">
  <button type="button" class="capture" class:capturing onclick={startCapture}>
    {#if capturing}
      Press a key combination… (Esc to cancel)
    {:else}
      {value ?? 'Not set'}
    {/if}
  </button>
  <button type="button" class="clear" onclick={clear} disabled={!value}>
    Clear
  </button>
</div>

<style>
  .shortcut-row {
    display: flex;
    gap: 8px;
    align-items: center;
  }
  .capture {
    flex: 1;
    padding: 6px 10px;
    background: #2a2a2a;
    border: 1px solid #444;
    color: #ddd;
    border-radius: 4px;
    font-family: monospace;
    font-size: 13px;
    cursor: pointer;
    text-align: left;
    min-height: 30px;
  }
  .capture.capturing {
    border-color: #bb55ff;
    color: #bb55ff;
  }
  .capture:hover:not(.capturing) {
    background: #333;
  }
  .clear {
    padding: 6px 10px;
    background: #2a2a2a;
    border: 1px solid #444;
    color: #ddd;
    border-radius: 4px;
    font-size: 13px;
    cursor: pointer;
  }
  .clear:disabled {
    opacity: 0.5;
    cursor: not-allowed;
  }
  .clear:hover:not(:disabled) {
    background: #333;
  }
</style>
