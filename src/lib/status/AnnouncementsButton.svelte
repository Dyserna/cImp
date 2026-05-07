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
