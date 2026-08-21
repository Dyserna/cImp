<script lang="ts">
  // V39 — the `delegation` chip in the status bar's security section.
  //
  // Counts the tabs another harness is driving RIGHT NOW (Phase B; Phase A
  // counted read-only tabs, which was the placeholder its own comment said it
  // was). Hidden at zero: a delegation is transient, and a permanent "DLG 0" in
  // an already busy bar is noise. It is NOT a toggle — there is no global
  // switch it could flip; a flight ends from the driven tab's own ⇄ popover
  // or its context menu, which is what the tooltip says.
  //
  // The count itself is derived in `delegationChip.ts` (pure, tested), for the
  // reason that file's header gives.
  import { settings } from '../settings/store';
  import { delegationInFlight } from '../delegationState';
  import { delegationChipState } from './delegationChip';

  const chip = $derived(delegationChipState($delegationInFlight, $settings));
</script>

{#if chip.visible}
  <span class="status-chip delegation" role="status" title={chip.title} aria-label={chip.title}>
    <span class="glyph" aria-hidden="true">⇄</span>
    <span class="text">{chip.label}</span>
  </span>
{/if}

<style>
  .status-chip {
    color: var(--awaiting);
    height: 22px;
    padding: 0 8px;
    border-radius: var(--radius-pill);
    display: inline-flex;
    align-items: center;
    gap: 4px;
    justify-content: center;
    line-height: 1;
    font-size: 11px;
    user-select: none;
  }
  .glyph {
    font-size: 12px;
  }
</style>
