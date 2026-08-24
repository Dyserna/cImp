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
  /* Shell + focus ring: `.status-button` in `src/app.css`. State only here. */
  .status-button:hover {
    background: var(--surface-3);
    color: var(--text-primary);
  }
  .glyph {
    font-size: 14px;
  }
</style>
