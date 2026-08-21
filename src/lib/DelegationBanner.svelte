<script lang="ts">
  // V39 Phase B, locked decision 2a — the worker tab's attribution banner.
  //
  // For the WHOLE flight, the tab the engine is driving says who asked, how
  // long it has been going, whether it is stuck on a prompt of the user's, and
  // offers the one control that ends it. The user must be able to tell, by
  // looking at the tab, that a turn was not theirs.
  //
  // **Client-side only.** Every string here is rendered by the frontend; none
  // of it is ever written to the PTY. The worker model receives the task
  // verbatim, with no header and no marker (decision 2a), so nothing on this
  // strip can be read by it as provenance.
  //
  // **An overlay, not a strip in the layout**, for the reason the taint frame
  // gives one line down in `Pane.svelte`: a real element above the terminal
  // slot changes the terminal's height, which refits xterm and resizes the PTY
  // — mid-turn, in a TUI cImp is in the middle of typing into. A repaint the
  // user did not ask for is a worse cost than covering the top row for the
  // duration.
  //
  // The elapsed counter ticks off `delegationClock`, a `readable` whose
  // interval exists only while something is subscribed — and this component is
  // mounted only while its pane's ACTIVE tab is being driven. That is the same
  // rule `appViews.ts` states for the keep-alive views (nothing periodic runs in
  // a view that is off screen); here it falls out of the mount instead of
  // needing a visibility store, because unlike those views this one really is
  // destroyed when it leaves the screen.
  import { attributionLine, elapsedLabel, type InFlightView } from './delegation';
  import { delegationClock } from './delegationState';
  import { delegationTakeOver } from './ipc';
  import { showToast } from './toast';
  import type { TabId } from './tabs/types';

  let { tabId, tabName, inFlight }: { tabId: TabId; tabName: string; inFlight: InFlightView } =
    $props();

  let busy = $state(false);

  const elapsed = $derived(elapsedLabel(inFlight.started_ms, $delegationClock));

  async function takeOver(): Promise<void> {
    if (busy) return;
    busy = true;
    try {
      const wasRunning = await delegationTakeOver(tabId);
      showToast(
        wasRunning
          ? `You took “${tabName}” back. The driver was told the delegation was cancelled; the worker keeps running — cImp sends it no keys.`
          : `“${tabName}” was not being driven any more — the delegation had already finished.`,
        6000,
      );
    } catch (e) {
      showToast(`Take over failed: ${String(e)}`, 6000);
    } finally {
      busy = false;
    }
  }
</script>

<div class="delegation-banner" class:awaiting={inFlight.awaiting_prompt} role="status">
  <span class="attr">{attributionLine(inFlight.driver_agent, inFlight.driver_name)}</span>
  <span class="elapsed" aria-label="Elapsed">{elapsed}</span>
  {#if inFlight.awaiting_prompt}
    <span class="prompt">waiting for your permission</span>
  {/if}
  <span class="spacer"></span>
  <button
    type="button"
    class="takeover"
    disabled={busy}
    title="Stop cImp waiting and unlock your keyboard. The worker is sent nothing — no Escape, no interrupt — so it finishes its turn visibly."
    onclick={() => void takeOver()}
  >
    Take over
  </button>
</div>

<style>
  .delegation-banner {
    position: absolute;
    top: 0;
    left: 0;
    right: 0;
    z-index: 12;
    display: flex;
    align-items: center;
    gap: var(--space-2);
    padding: 2px var(--space-2);
    font-size: var(--font-size-sm);
    line-height: 1.4;
    color: var(--accent);
    background: color-mix(in srgb, var(--accent) 16%, var(--surface-0));
    border-bottom: 1px solid var(--accent);
    /* The strip itself takes clicks (the button is on it); everything below is
       untouched because the strip is only as tall as one row. */
    pointer-events: auto;
    user-select: none;
  }
  /* A prompt is standing: this is the state where the delegation is waiting on
     the USER, not on the worker, and it must not read like ordinary progress. */
  .delegation-banner.awaiting {
    color: var(--awaiting);
    background: color-mix(in srgb, var(--awaiting) 18%, var(--surface-0));
    border-bottom-color: var(--awaiting);
  }
  .attr {
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
    min-width: 0;
  }
  .elapsed {
    flex: 0 0 auto;
    opacity: 0.85;
    font-variant-numeric: tabular-nums;
  }
  .prompt {
    flex: 0 0 auto;
    font-weight: 600;
  }
  .spacer {
    flex: 1 1 auto;
  }
  .takeover {
    flex: 0 0 auto;
    appearance: none;
    font-family: inherit;
    font-size: var(--font-size-sm);
    color: inherit;
    background: transparent;
    border: 1px solid currentColor;
    border-radius: var(--radius-sm);
    padding: 0 6px;
    cursor: pointer;
  }
  .takeover:hover:not([disabled]) {
    background: color-mix(in srgb, currentColor 18%, transparent);
  }
  .takeover:focus-visible {
    outline: 2px solid currentColor;
    outline-offset: 1px;
  }
  .takeover[disabled] {
    opacity: 0.6;
    cursor: default;
  }
</style>
