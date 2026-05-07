<script lang="ts">
  // Settings launcher for the bottom status bar. Replaces the gear icon
  // that used to overlay the avatar — moved here so it's reachable even
  // when the avatar is hidden.
  import { openSettingsWindow } from '../settings/ipc';
</script>

<button
  type="button"
  class="status-button"
  onclick={() => void openSettingsWindow()}
  title="Settings"
  aria-label="Settings"
>
  <span class="glyph" aria-hidden="true">⚙</span>
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
  .status-button:hover {
    background: var(--surface-3);
    color: var(--text-primary);
  }
  .status-button:focus-visible {
    outline: 2px solid var(--accent);
    outline-offset: 2px;
  }
  .glyph {
    font-size: 14px;
  }
</style>
