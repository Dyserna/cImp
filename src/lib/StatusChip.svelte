<script lang="ts">
  // One row-status chip for the activity feeds (#48, M-24).
  //
  // **Why a component.** Both the Tool Activity tab and the Events tab render
  // the same store, and both collapsed every `injection_flag` row into a single
  // treatment — so "part of this result was never screened" was drawn exactly
  // like "we blocked an SSRF target", and a user-applied latch override (a
  // GRANT) was drawn like containment firing. Svelte styles are scoped, so
  // keeping the vocabulary in two scoped style blocks is how it drifts. The word
  // and the tooltip come from `activity.ts` (which has a test harness); the
  // pixels come from here, and there is one copy of them.
  //
  // The words are the Events tab's existing `denied` / `flagged` / `ok` /
  // `failed` / `signal` vocabulary, EXTENDED — not replaced. Renaming them would
  // have produced a second vocabulary for the same rows, which is the defect
  // class being fixed.
  //
  // The visual grammar, deliberately not one axis:
  //   • FILLED danger  — `denied`. The only red, and the only "we stopped it".
  //   • FILLED warning — `flagged`, `rejected`: something matched / was refused.
  //   • OUTLINE warning — `held`: nothing was blocked, but it is waiting on you.
  //   • DASHED neutral — `unscreened`, `recorded`: we did not (or cannot) claim
  //     a verdict. Dashed is already this app's "not a confident claim" carrier
  //     (the injection chip and the reduced-features list use it), so
  //     "we did not look at all of it" reads as absence of a verdict rather
  //     than as an alarm — which is the whole of M-24.
  //   • OUTLINE info — `engaged`: containment came on.
  //   • FILLED info — `granted`: capability went back out. A release must be as
  //     visible as a block and must not look like one.
  //   • quiet — `ok`, `update`: ordinary traffic.
  //   • plain warning text — `signal`, `failed` keep the treatments the two
  //     feeds already gave them.
  //
  // Colours are theme tokens only — themes/palettes ship as external files
  // beside the exe, so a hardcoded hex breaks the `tui` and light themes.
  import { STATUS_TITLE, type RowStatus } from './activity';

  let { status }: { status: RowStatus } = $props();
</script>

<span class="schip {status}" title={STATUS_TITLE[status]}>{status}</span>

<style>
  .schip {
    text-transform: uppercase;
    font-size: 0.78em;
    font-weight: 600;
    letter-spacing: 0.02em;
    border: 1px solid transparent;
    border-radius: var(--radius-sm, 2px);
    padding: 0 3px;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }

  /* Ordinary traffic — present, but never competing with a containment row. */
  .schip.ok {
    opacity: 0.45;
  }
  .schip.update {
    color: var(--text-tertiary, #9aa0aa);
    opacity: 0.8;
  }

  /* A call that really failed. */
  .schip.failed {
    color: var(--text-danger-soft, #ffb0c0);
  }
  /* A telemetry channel firing — not a failure (the pre-existing treatment). */
  .schip.signal {
    color: var(--text-warning, #f0c060);
  }

  /* The only "we blocked something". */
  .schip.denied {
    color: var(--text-danger-soft, #ffb0c0);
    background: var(--surface-danger-faint, rgba(240, 96, 128, 0.14));
    border-color: var(--border-danger, #5a3038);
  }

  /* A detector matched / an update was refused: filled amber. */
  .schip.flagged,
  .schip.rejected {
    color: var(--text-warning, #f0c060);
    background: var(--surface-warning-faint, rgba(240, 160, 32, 0.12));
    border-color: var(--border-warning, #6a571a);
  }

  /* Nothing blocked — something is waiting for the user. Amber OUTLINE so it
     is legible beside `flagged` without borrowing its fill. */
  .schip.held {
    color: var(--text-warning, #f0c060);
    border-color: var(--border-warning, #6a571a);
  }

  /* No verdict was reached. Dashed = "not a confident claim", the same carrier
     the injection chip and the reduced-features list already use. Never red:
     nothing was found and nothing was stopped. */
  .schip.unscreened,
  .schip.recorded {
    color: var(--text-tertiary, #9aa0aa);
    border-style: dashed;
    border-color: var(--border-default, #3f4554);
  }

  /* Containment came on (beacon / contamination). */
  .schip.engaged {
    color: var(--text-info, #d8b8ff);
    border-color: var(--border-info-soft, #3a2a55);
  }

  /* Capability went back out on a user's authority — filled, because a release
     has to be as visible as a block while never looking like one. */
  .schip.granted {
    color: var(--text-info, #d8b8ff);
    background: var(--surface-info-faint, rgba(216, 184, 255, 0.12));
    border-color: var(--border-info, #6f42a8);
  }
</style>
