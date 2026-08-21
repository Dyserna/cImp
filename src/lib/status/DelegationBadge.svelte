<script lang="ts">
  // V39 Phase A — the `delegation` chip in the status bar's security section.
  //
  // Counts the tabs whose keyboard is currently refused. Hidden at zero: the
  // states it reports are transient, and a permanent "RO 0" in an already busy
  // bar is noise. It is NOT a toggle — unlike the sandbox chips, there is no
  // single global switch it could flip; the lock is per tab and is set from the
  // tab's own ⇄ glyph, which is what the tooltip says.
  //
  // The count itself is derived in `delegationChip.ts` (pure, tested), for the
  // reason that file's header gives.
  import { settings } from '../settings/store';
  import { delegationChipState } from './delegationChip';

  const chip = $derived(delegationChipState($settings));
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
