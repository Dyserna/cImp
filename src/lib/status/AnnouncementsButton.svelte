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
  .status-button {
    appearance: none;
    background: transparent;
    border: none;
    color: #c0c0c0;
    cursor: pointer;
    width: 24px;
    height: 24px;
    border-radius: 4px;
    padding: 0;
    display: inline-flex;
    align-items: center;
    justify-content: center;
    line-height: 1;
  }
  .status-button:hover {
    background: #303030;
    color: #ffffff;
  }
  .glyph {
    font-size: 14px;
  }
  .glyph.muted {
    opacity: 0.55;
  }
</style>
