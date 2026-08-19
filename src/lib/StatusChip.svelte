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
  //   • UNDERLINED danger-soft — `down`, `boundary`: a real failure whose cause
  //     is in the row, not a block we performed. Never the filled red.
  //   • STRUCK-THROUGH danger OUTLINE — `withheld`: a tool removed from the
  //     advertised surface. Danger, because this is the one place in cImp where
  //     detection actually takes something away, and the chip must not
  //     under-claim it the way `flagged` ("nothing was blocked") would. An
  //     outline and not the filled red, because that red is `denied`'s alone —
  //     a call we stopped — and here no call was ever made. The line through the
  //     word is the removal.
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

  /* V33 Phase A: the OS boundary was absent for this call. OUTLINE warning,
     not dashed and not red: this IS a confident claim (we know the sandbox did
     not apply, and why), but nothing was blocked and the command ran normally —
     so it must not wear `denied`'s red or `unscreened`'s "we didn't look". */
  .schip.unsandboxed {
    color: var(--warning, #f0a020);
    border-color: color-mix(in srgb, var(--warning, #f0a020) 45%, transparent);
  }

  /* A sandboxed child failed with denial-shaped output. Borrows the `failed`
     treatment plus `down`'s dotted underline (the app's existing "this row
     carries a reason worth opening") — so it reads as a failure, distinctly,
     without taking `denied`'s filled red. That red is the one "we stopped it",
     and this row is a HEURISTIC: the boundary is the likely cause, not an
     observed fact. A chip that claimed more than the row's own wording does
     would be the same over-claim in pixels. */
  .schip.boundary {
    color: var(--text-danger-soft, #ffb0c0);
    border-color: color-mix(in srgb, var(--text-danger-soft, #ffb0c0) 40%, transparent);
    text-decoration: underline dotted;
    text-underline-offset: 2px;
  }

  /* V37 C9: a tool withheld from the advertised surface by description
     screening. Danger OUTLINE plus a line through the word — the removal is
     real (so not `flagged`'s amber, whose sentence promises nothing was
     blocked) and it is not a broken call (so not `failed`'s bare pink text),
     but it is also not `denied`'s filled red: no call was ever made. */
  .schip.withheld {
    color: var(--text-danger-soft, #ffb0c0);
    border-color: var(--border-danger, #5a3038);
    text-decoration: line-through;
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

  /* ── Offload server lifecycle ──────────────────────────────────────────
     `started`/`ready` are the healthy path and stay quiet, like `ok`: a server
     coming up is the expected case and must not compete with a containment row
     for attention. `stopped` is quiet too — cImp stopped it on purpose.

     `down` is the one that is NOT quiet, and it is deliberately not red: red is
     spent on `denied`, the one word meaning "we blocked something", and a
     backend that fell over is a failure to report, not a threat we contained.
     It borrows the `failed` treatment, plus a dotted underline that says the
     row carries a reason worth opening. */
  .schip.started,
  .schip.ready {
    color: var(--text-success, #3fb950);
    opacity: 0.7;
  }
  .schip.stopped {
    color: var(--text-tertiary, #9aa0aa);
    opacity: 0.8;
  }
  .schip.down {
    color: var(--text-danger-soft, #ffb0c0);
    text-decoration: underline dotted;
    text-underline-offset: 2px;
  }
</style>
