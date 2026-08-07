<script lang="ts">
  // V32 Phase G (locked decision 16): the reduced-protection indicator on the
  // bottom-right status cluster.
  //
  // Locked decision 16 requires that a reduced-protection state be "visible
  // outside Settings too … so protection cannot be off and forgotten". The tab
  // badge and its popover cover per-tab state; this covers the app: it is the
  // one surface a user sees without opening anything.
  //
  // Deliberately silent when everything is on. A permanent "protected" chip
  // would be noise 99% of the time and would train the eye to skip exactly the
  // spot where the warning appears — the same reason the detection layers are
  // surface-only rather than always-on banners.
  //
  // Two levels of severity, because they mean very different things:
  //   * the MASTER is off  — every V32 control is inert, everywhere;
  //   * something is off   — some feature is disabled at some scope.
  // Clicking opens Settings, where the matrix says exactly which and why.
  import { openSettingsWindow } from '../settings/ipc';
  import { injectionStatus } from '../latch';

  const status = $derived($injectionStatus);
  const masterOff = $derived(!!status && !status.protection);
  const visible = $derived(!!status?.reduced);

  /// How many (scope, feature) pairs are switched off, for the tooltip. Counted
  /// over rows the scope actually HAS — a tab is not "reduced" because the
  /// worker-only canary does not apply to it.
  const offCount = $derived(
    (status?.scopes ?? []).reduce(
      (n, s) => n + s.features.filter((f) => f.in_scope && !f.effective).length,
      0,
    ),
  );

  const title = $derived(
    masterOff
      ? 'Injection protection is OFF — every V32 control is disabled, for every tab and the offload worker. Click to open Settings.'
      : `Injection protection is reduced — ${offCount} control${offCount === 1 ? '' : 's'} switched off. Click to open Settings.`,
  );
</script>

{#if visible}
  <button
    type="button"
    class="status-button injection"
    class:master-off={masterOff}
    onclick={() => void openSettingsWindow()}
    {title}
    aria-label={title}
  >
    <span class="glyph" aria-hidden="true">⛨</span>
    <span class="text">{masterOff ? 'off' : 'reduced'}</span>
  </button>
{/if}

<style>
  .status-button {
    appearance: none;
    background: transparent;
    border: 1px solid transparent;
    color: var(--awaiting);
    cursor: pointer;
    height: 22px;
    padding: 0 8px;
    border-radius: var(--radius-pill);
    display: inline-flex;
    align-items: center;
    gap: 4px;
    justify-content: center;
    line-height: 1;
    font-size: 11px;
    transition:
      background var(--motion-fast) var(--easing-standard),
      color var(--motion-fast) var(--easing-standard),
      border-color var(--motion-fast) var(--easing-standard);
  }
  /* The master switch is the louder of the two states: it is not "one control
     is off", it is "none of them are on". */
  .master-off {
    color: var(--text-danger-soft);
    border-color: var(--text-danger-soft);
  }
  .status-button:hover {
    background: var(--surface-3);
  }
  .status-button:focus-visible {
    outline: 2px solid var(--accent);
    outline-offset: 2px;
  }
  .glyph {
    font-size: 12px;
  }
</style>
