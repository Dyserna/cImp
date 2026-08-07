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
  //   * something is off   — some feature is disabled at some scope;
  //   * the state is UNKNOWN — the poll behind this chip has failed for several
  //     ticks running (#48, G-3). Rendering nothing there would be the one
  //     failure this surface cannot have: "no chip" reads as "fully protected",
  //     which is exactly the off-and-forgotten state decision 16 forbids.
  // Clicking opens Settings, where the matrix says exactly which and why.
  import { openSettingsWindow } from '../settings/ipc';
  import { injectionStatus, injectionStatusUnknown, reducedSummary } from '../latch';

  const status = $derived($injectionStatus);
  const unknown = $derived($injectionStatusUnknown);
  const masterOff = $derived(!unknown && !!status && !status.protection);
  const visible = $derived(unknown || !!status?.reduced);

  /// What is reduced, in the tooltip's words. The rule is `latch.ts`'s
  /// `isReducedRow` — the same one the tab badge beside this chip uses — rather
  /// than a third copy of it (#48, G-2): the chip previously counted
  /// `in_scope && !effective`, omitting the `default_on` clause Phase H
  /// published specifically to prevent the two disagreeing in one viewport.
  const summary = $derived(reducedSummary(status));

  const title = $derived(
    unknown
      ? 'Injection protection state is UNKNOWN — cImp has not been able to read it for several polls, so this app cannot tell you what is switched on. Check the console. Click to open Settings.'
      : masterOff
        ? 'Injection protection is OFF — every V32 control is disabled, for every tab and the offload worker. Click to open Settings.'
        : `Injection protection is reduced — ${summary}. Click to open Settings.`,
  );
</script>

{#if visible}
  <button
    type="button"
    class="status-button injection"
    class:master-off={masterOff}
    class:unknown
    onclick={() => void openSettingsWindow()}
    {title}
    aria-label={title}
  >
    <span class="glyph" aria-hidden="true">⛨</span>
    <span class="text">{unknown ? 'unknown' : masterOff ? 'off' : 'reduced'}</span>
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
  /* "Unknown" is not "off": it is a broken instrument, not a posture the user
     chose. Same weight as the reduced state, dashed so it does not read as a
     confident claim about anything (#48, G-3). */
  .unknown {
    border-style: dashed;
    border-color: var(--awaiting);
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
