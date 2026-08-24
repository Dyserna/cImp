<script lang="ts">
  // Announcement toggle for the bottom status bar. Bound to
  // `behavior.announcements_enabled`. When false, the notification manager
  // early-returns before queueing — so toggling this gates the whole
  // cross-tab notification system.
  import { get } from 'svelte/store';
  import { settings, applySettings } from '../settings/store';

  function toggle() {
    const s = get(settings);
    void applySettings({
      ...s,
      behavior: {
        ...s.behavior,
        announcements_enabled: !s.behavior.announcements_enabled,
      },
    });
  }
</script>

<button
  type="button"
  class="status-button"
  onclick={toggle}
  title={$settings.behavior.announcements_enabled
    ? 'Disable announcements'
    : 'Enable announcements'}
  aria-pressed={!$settings.behavior.announcements_enabled}
>
  {#if $settings.behavior.announcements_enabled}
    <span class="glyph" aria-hidden="true">🔔</span>
  {:else}
    <span class="glyph muted" aria-hidden="true">🔕</span>
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
