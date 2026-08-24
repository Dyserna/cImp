<script lang="ts">
  /// Settings → Harness health (#129 (c)) — V35 Phase G's status board.
  ///
  /// It renders and decides nothing: the grouping, the tier order, the coverage
  /// marks, the gate verdicts and every outcome are computed in Rust
  /// (`harness::health`). The five helpers below are display-only — an age in
  /// words, and the badge class for an outcome — and nothing outside this
  /// section read them.
  ///
  /// **The poll stays with the parent.** `SettingsApp` owns `harnessFresh`, the
  /// 2 s interval, its 150-round wedge cap and `stopHarnessPoll()` in
  /// `onDestroy`. Three reasons, each on its own sufficient:
  ///
  ///   1. `onMount` starts the poll when a run is ALREADY in flight — before
  ///      this section has ever been viewed. Owning the poll here would delay
  ///      that to first view, so an automatic post-upgrade run would go
  ///      unwatched until the user happened to open this page.
  ///   2. A child's teardown fires on every section switch. `stopHarnessPoll()`
  ///      in this component's `onDestroy` would kill a live run's poll the
  ///      moment the user navigated away, and remounting would lose
  ///      `harnessStarting` — the optimistic "you clicked Verify" flag — so the
  ///      button would snap back to idle mid-run.
  ///   3. `harnessFresh` is not this section's alone: Code Intelligence reads
  ///      it through `controlBlocked` for the read-advisor gate. One owner.
  ///
  /// So the payload, the two busy flags and the run error arrive as props, and
  /// the Verify button calls back.
  import {
    OUTCOME_NO_FAILURE,
    type CapabilityHealth,
    type HarnessHealth,
    type HarnessStatus,
  } from '../types';

  let {
    health,
    busy,
    starting,
    runError,
    onrun,
  }: {
    /// The fresh `harness_versions_get` payload, or `null` before it lands.
    health: HarnessStatus | null;
    /// A verification run is in flight — this window's or the backend's.
    busy: boolean;
    /// The harness THIS window asked to verify, until the payload confirms a
    /// run (or the poll gives up). Distinct from `busy`: a run can be in
    /// flight because of an automatic version-change check nobody clicked for,
    /// and the page says so rather than pretending the click did it.
    starting: string | null;
    /// The last run-request failure, rendered verbatim.
    runError: string | null;
    /// Ask the parent to start a verification run for one harness.
    onrun: (harness: string) => void;
  } = $props();

  /// Coarse age of a timestamp, for "last verified 3 h ago". Display-only: the
  /// panel needs the shape of the number, not its precision.
  function ageOf(atMs: number): string {
    const delta = Date.now() - atMs;
    if (!Number.isFinite(delta) || delta < 0) return 'just now';
    const mins = Math.floor(delta / 60000);
    if (mins < 1) return 'just now';
    if (mins < 60) return `${mins} min ago`;
    const hours = Math.floor(mins / 60);
    if (hours < 24) return `${hours} h ago`;
    return `${Math.floor(hours / 24)} d ago`;
  }

  /// The badge class for one row's last outcome. `no_failure` deliberately does
  /// NOT get the pass styling — the stored record keeps failures only, so it is
  /// the weaker statement and must not read as a green tick.
  function outcomeClass(outcome: string): string {
    if (outcome === 'fail') return 'bad';
    if (outcome === 'pass') return 'good';
    return 'quiet';
  }

  function outcomeLabel(outcome: string): string {
    if (outcome === OUTCOME_NO_FAILURE) return 'no failure reported';
    return outcome;
  }

  /// The badge class for a stored `AutoVerify.status`. Only the two statuses
  /// Rust writes get a colour; anything else is a hand edit (or a record from a
  /// newer cImp) and stays neutral rather than being guessed into "fine" —
  /// the same direction `harness::verify::tripwire_superseded` takes.
  function recordClass(status: string): string {
    if (status === 'fail') return 'bad';
    if (status === 'pass') return 'good';
    return 'quiet';
  }

  /// Rows whose seam is the one that breaks silently on a cosmetic upstream
  /// change, counted for the header. Display summary only — the rows carry the
  /// facts.
  function brokenNow(h: HarnessHealth): CapabilityHealth[] {
    return h.capabilities.filter(
      (c) => c.last_verify?.outcome === 'fail' || c.gate?.blocked,
    );
  }
</script>

<section>
  <!--
    V35 Phase G — the matrix draft's third consumer (§ 3.3): the screen
    that answers "what is actually broken right now" without reading
    source. Everything below is RENDERED, not decided: the grouping,
    the tier order, the coverage marks, the gate verdicts and every
    outcome come from `harness::health::health()`.

    Restyled 2026-08-23 (user decision): STATUS FIRST. The registry's
    columns — tier, degradation, coverage, TCB, wired-in — are the
    maintainer's bookkeeping ("which rows need a canary"), not a
    status, and a page full of "Breaks silently" marks read as a to-do
    list the user could do nothing about. The user's view is now one
    verdict line per harness, plus the rows that are ACTUALLY failing
    or gated off, each written as consequence + what they can do. The
    full matrix is still here, behind a maintainer disclosure — the
    same data, nothing removed from the wire.
  -->
  <h2>Harness health</h2>
  <small class="hint top">
    cImp rides user-installed CLIs it does not pin, and they self-update.
    When a CLI update changes something cImp depends on, a feature can
    stop working with no error — this page says when that has happened,
    what stops working, and what you can do about it.
  </small>
  {#if !health}
    <small class="hint">Reading the capability registry…</small>
  {:else}
    {#if runError}
      <small class="error">{runError}</small>
    {/if}
    {#if busy && starting === null}
      <small class="hint">
        A verification run is already in progress — most likely the automatic
        check that follows a CLI version change. This page updates when it
        finishes.
      </small>
    {/if}
    {#each health.harness_health as panel (panel.harness)}
      {@const broken = brokenNow(panel)}
      {@const stale = panel.stale_plugins?.length ?? 0}
      {@const behind =
        !!panel.last_seen &&
        !!panel.last_verified &&
        panel.last_seen !== panel.last_verified}
      <div
        class="harness-panel"
        class:harness-panel-bad={broken.length > 0}
        class:harness-panel-warn={broken.length === 0 && (stale > 0 || behind)}
      >
        <div class="harness-head">
          <span class="harness-title">{panel.label}</span>
          <!--
            Every button is disabled while ANY run is in flight — the
            single flight is process-wide (one set of probe children at
            a time), so a second harness's click would be dropped
            rather than queued.
          -->
          <button
            onclick={() => onrun(panel.harness)}
            disabled={busy}
          >
            {starting === panel.harness
              ? 'Running checks…'
              : 'Run checks now'}
          </button>
        </div>
        <!--
          The verdict line. One sentence, computed from the same facts
          the old facts-list showed; the facts themselves moved into
          the maintainer disclosure.
        -->
        <p class="harness-verdict">
          {#if broken.length > 0}
            <span class="badge bad">{broken.length} broken</span>
          {:else if panel.last_verified == null}
            <span class="badge quiet">nothing to verify</span>
          {:else if !panel.last_seen}
            <span class="badge quiet">not seen yet</span>
          {:else if behind}
            <span class="badge warn">not yet verified</span>
          {:else}
            <span class="badge good">all checks passed</span>
          {/if}
          <span class="fact-detail">
            {#if panel.last_seen}
              <code>{panel.last_seen}</code> installed
            {/if}
            {#if panel.auto_verify}
              · last automatic check {ageOf(panel.auto_verify.at_ms)}
            {/if}
            {#if panel.last_run}
              · last run {ageOf(panel.last_run.at_ms)}
              ({panel.last_run.pass} pass, {panel.last_run.fail} fail{#if panel.last_run.unknown > 0},
                {panel.last_run.unknown} could not be checked{/if})
            {/if}
          </span>
        </p>
        {#if behind && broken.length === 0}
          <small class="hint">
            <code>{panel.last_seen}</code> is installed but cImp last verified
            its contracts against <code>{panel.last_verified || 'nothing'}</code>.
            An automatic check runs after an update; <em>Run checks now</em>
            runs it immediately.
          </small>
        {/if}
        {#if stale > 0}
          <!--
            V35 Phase I. The plugin/overlay a tab runs is baked at
            LAUNCH, so upgrading cImp with a tab open leaves an old
            artifact talking to new loopback code. This one the user
            CAN fix — open a fresh tab — so it stays in the user view.
          -->
          <div class="harness-issue warn">
            <div class="issue-title">
              {stale === 1 ? 'One tab is' : `${stale} tabs are`} running an
              out-of-date cImp plugin
            </div>
            <small class="hint">
              {#each panel.stale_plugins as sp (sp.tab)}
                <div><code>{sp.tab}</code> — {sp.note}</div>
              {/each}
            </small>
            <small class="hint issue-action">
              <strong>What to do:</strong> close and reopen
              {stale === 1 ? 'that tab' : 'those tabs'}; a fresh tab
              gets the current plugin.
            </small>
          </div>
        {/if}
        {#each broken as cap (cap.id)}
          <!--
            A failing or gated-off row, in user terms: what it is, what
            that costs, what can be done. The contract sentence is the
            registry's own, the effect is the degradation sentence, and
            the remedy is the one the user actually has: reinstall the
            verified CLI version, wait for a cImp update, or report it.
          -->
          <div class="harness-issue bad">
            <div class="issue-title">
              <code class="cap-id">{cap.id}</code>
              {#if cap.gate?.blocked}
                <span class="badge bad">gated off</span>
              {:else}
                <span class="badge bad">failed</span>
              {/if}
            </div>
            <p class="cap-contract issue-effect">{cap.user_effect}</p>
            {#if cap.gate?.blocked}
              <small class="error">{cap.gate.reason}</small>
            {/if}
            {#if cap.last_verify?.outcome === 'fail'}
              <small class="hint">
                {cap.last_verify.detail}
                — {ageOf(cap.last_verify.at_ms)}, against
                <code>{cap.last_verify.version || 'no recorded version'}</code>
              </small>
            {/if}
            <small class="hint">
              <strong>Detail:</strong> {cap.contract}
              {cap.degradation.label}{#if cap.degradation.user_message}
                — “{cap.degradation.user_message}”{/if}{#if cap.degradation.fallback_to}
                — <code>{cap.degradation.fallback_to}</code> takes over{/if}.
              {#if cap.wired_in.length > 0}
                Affects
                {#each cap.wired_in as path, i (path)}<code>{path}</code>{i < cap.wired_in.length - 1 ? ', ' : ''}{/each}.
              {/if}
            </small>
            <small class="hint issue-action">
              <strong>What to do:</strong>
              {#if cap.gate?.blocked}
                the feature is switched off until this is resolved.
              {/if}
              If this started after the CLI updated, installing the last
              version cImp verified{#if panel.last_verified}
                (<code>{panel.last_verified}</code>){/if}
              brings it back; otherwise wait for a cImp update, or report it
              together with the output of <em>Run checks now</em>.
            </small>
          </div>
        {/each}
        <!--
          The maintainer view: the whole registry, as before. Same
          data, same marks; only the default visibility changed.
        -->
        <details class="cap-more harness-matrix">
          <summary>
            All {panel.capabilities.length} dependencies (maintainer view)
          </summary>
          <small class="hint">
            Each row is one thing cImp depends on from this CLI, ranked by
            the <strong>seam</strong> it sits in — Tier D (scraped UI,
            undocumented behavior) is most fragile and listed first; Tier A
            (MCP) has never broken cImp. "Breaks silently" is the
            classification of a row, not its status: it says a break would
            produce no error, which is why cImp checks it.
          </small>
          <ul class="harness-facts">
            <li>
              <span class="fact-key">Version seen</span>
              <code>{panel.last_seen || 'not observed yet'}</code>
            </li>
            {#if panel.last_verified != null}
              <li>
                <span class="fact-key">Contracts verified against</span>
                <code>{panel.last_verified || 'never verified'}</code>
                {#if behind}
                  <span class="badge warn">behind the installed build</span>
                {/if}
              </li>
            {/if}
            {#if panel.auto_verify}
              <li>
                <span class="fact-key">Last automatic run</span>
                <span class="badge {recordClass(panel.auto_verify.status)}"
                  >{panel.auto_verify.status}</span
                >
                <span class="fact-detail">
                  against <code>{panel.auto_verify.version || 'no version'}</code>,
                  {ageOf(panel.auto_verify.at_ms)}
                </span>
              </li>
            {/if}
            {#if panel.last_run}
              <li>
                <span class="fact-key">Last run this session</span>
                <span class="fact-detail">
                  {panel.last_run.pass} pass · {panel.last_run.fail} fail ·
                  {panel.last_run.unknown} unknown · {panel.last_run.transition}
                  transition, {ageOf(panel.last_run.at_ms)}
                  {#if panel.last_run.capped}
                    — the live-probe half was skipped for time
                  {/if}
                </span>
              </li>
            {/if}
            {#if stale > 0}
              <li>
                <span class="fact-key">Out-of-step tabs</span>
                <span class="badge warn">{stale}</span>
                <span class="fact-detail">
                  {#each panel.stale_plugins as sp (sp.tab)}
                    <div>
                      <code>{sp.tab}</code> — sends CHP {sp.seen_chp}, this build
                      writes CHP {sp.expected}. {sp.note}
                    </div>
                  {/each}
                </span>
              </li>
            {/if}
          </ul>
          <ul class="cap-list">
            {#each panel.capabilities as cap (cap.id)}
              <li
                class="cap"
                class:cap-bad={cap.last_verify?.outcome === 'fail' ||
                  cap.gate?.blocked}
              >
                <div class="cap-head">
                  <span class="badge tier tier-{cap.tier}">Tier {cap.tier}</span>
                  <code class="cap-id">{cap.id}</code>
                  {#if cap.controls.length > 0}
                    <!--
                      Matrix decision 10: a TCB row does not merely carry
                      data for a security control, the control EXECUTES
                      inside it.
                    -->
                    <span class="badge tcb" title="Security control executes here"
                      >TCB</span
                    >
                  {/if}
                  {#if cap.last_verify}
                    <span class="badge {outcomeClass(cap.last_verify.outcome)}"
                      >{outcomeLabel(cap.last_verify.outcome)}</span
                    >
                  {:else}
                    <span class="badge quiet">never checked</span>
                  {/if}
                </div>
                <p class="cap-contract">{cap.contract}</p>
                <div class="cap-marks">
                  <span class="mark {cap.degradation.kind === 'silent' ? 'bad' : ''}"
                    >{cap.degradation.label}</span
                  >
                  {#if cap.degradation.user_message}
                    <span class="mark quiet">“{cap.degradation.user_message}”</span>
                  {/if}
                  {#if cap.degradation.fallback_to}
                    <span class="mark quiet"
                      >Falls back to <code>{cap.degradation.fallback_to}</code></span
                    >
                  {/if}
                </div>
                <div class="cap-marks">
                  {#if cap.coverage.canary}
                    <span class="badge good">canary L1</span>
                  {/if}
                  {#if cap.coverage.probe}
                    <span class="badge good">live probe L2</span>
                  {/if}
                  {#if cap.coverage.unproven}
                    <span class="badge warn">waiver only — nothing checks this</span>
                  {:else if cap.coverage.waiver}
                    <span class="badge quiet">waiver</span>
                  {/if}
                  {#if !cap.coverage.canary && !cap.coverage.probe && !cap.coverage.waiver}
                    <span class="badge quiet">no automatic check</span>
                  {/if}
                  {#each cap.controls as control (control)}
                    <span class="badge tcb">{control}</span>
                  {/each}
                </div>
                {#if cap.gate?.blocked}
                  <small class="error">Gated off: {cap.gate.reason}</small>
                {/if}
                {#if cap.last_verify}
                  <small class="hint">
                    {cap.last_verify.detail}
                    <br />
                    {ageOf(cap.last_verify.at_ms)}, against
                    <code>{cap.last_verify.version || 'no recorded version'}</code>
                    {#if cap.last_verify.evidence}
                      · <code>{cap.last_verify.evidence}</code>
                    {/if}
                  </small>
                {/if}
                {#if cap.coverage.waiver}
                  <details class="cap-more">
                    <summary>Why nothing automatic covers it</summary>
                    <small class="hint">{cap.coverage.waiver}</small>
                  </details>
                {/if}
                <details class="cap-more">
                  <summary>What breaks if this drifts</summary>
                  <small class="hint">
                    {#each cap.wired_in as path (path)}
                      <code>{path}</code>{' '}
                    {/each}
                  </small>
                </details>
              </li>
            {/each}
          </ul>
        </details>
      </div>
    {/each}
    <small class="hint down">
      <em>Run checks now</em> drives this harness's embedded fixture canaries
      (L1) and then the installed CLI itself (L2); it takes up to 90 seconds
      and only one run happens at a time across the whole app. For a
      harness with a recorded auto-verify path it records the same result
      an automatic post-update run would, and advances the verified
      version when nothing failed. Recording a
      <em>manual</em> contract spike (the D0 / E1 behaviours no payload can
      reveal) is still the Advisor card's <em>Mark verified</em>, not this
      button.
    </small>
  {/if}
</section>

<style>
  /* V35 Phase G — Harness health. Deliberately built out of the idiom already
     here (.badge, the card border/radius of .policy-card, .backend-card's
     sunken surface) rather than a new visual language: this panel is a status
     board inside Settings, not a dashboard of its own. */
  .badge.good {
    color: var(--accent, #6abf69);
    border-color: var(--accent, #6abf69);
  }
  .badge.bad {
    color: var(--text-danger-soft, #d06b6b);
    border-color: var(--text-danger-soft, #d06b6b);
  }
  .badge.quiet {
    color: var(--text-quiet, #999);
  }
  /* The TCB mark. Filled rather than outlined so a security control reads
     differently from a data pipe at a glance (matrix decision 10). */
  .badge.tcb {
    color: var(--text-warning, #d08770);
    border-color: var(--border-warning, #d08770);
    font-weight: 600;
    letter-spacing: 0.03em;
  }
  .badge.tier {
    font-variant-numeric: tabular-nums;
  }
  /* Tier D is the riskiest seam and leads each list; the colour repeats the
     ordering so a scroll past the top still reads. */
  .badge.tier-D {
    color: var(--text-danger-soft, #d06b6b);
    border-color: var(--text-danger-soft, #d06b6b);
  }
  .badge.tier-C {
    color: var(--text-warning, #d08770);
    border-color: var(--border-warning, #d08770);
  }
  /* Status-first restyle (2026-08-23): the panel border repeats the verdict so
     a scroll past the header still reads; the issue cards are the only thing
     a user sees besides the verdict line. */
  .harness-panel-bad {
    border-left: 3px solid var(--text-danger-soft, #d06b6b);
  }
  .harness-panel-warn {
    border-left: 3px solid var(--text-warning, #d08770);
  }
  .harness-verdict {
    display: flex;
    gap: 0.5rem;
    align-items: baseline;
    flex-wrap: wrap;
    margin: 0.5rem 0 0.25rem;
    font-size: var(--font-size-sm);
  }
  .harness-issue {
    border: 1px solid var(--border-subtle);
    border-radius: 6px;
    padding: 0.5rem 0.6rem;
    margin: 0.5rem 0;
    background: var(--surface-sunken);
  }
  .harness-issue.bad {
    border-left: 3px solid var(--text-danger-soft, #d06b6b);
  }
  .harness-issue.warn {
    border-left: 3px solid var(--text-warning, #d08770);
  }
  .issue-title {
    display: flex;
    gap: 0.4rem;
    align-items: center;
    flex-wrap: wrap;
    font-weight: 600;
  }
  .issue-effect {
    font-size: var(--font-size-md);
  }
  .issue-action {
    display: block;
    margin-top: 0.3rem;
  }
  .harness-matrix {
    margin-top: 0.5rem;
  }
  .harness-matrix > summary {
    font-weight: 600;
  }
  .harness-panel {
    border: 1px solid var(--border-subtle);
    border-radius: 6px;
    padding: 0.75rem;
    margin: 0.75rem 0;
  }
  .harness-head {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 0.5rem;
    flex-wrap: wrap;
  }
  .harness-title {
    font-weight: 600;
  }
  .harness-facts {
    list-style: none;
    margin: 0.5rem 0 0.75rem;
    padding: 0;
    display: flex;
    flex-direction: column;
    gap: 0.25rem;
    font-size: var(--font-size-sm);
  }
  .harness-facts li {
    display: flex;
    gap: 0.5rem;
    align-items: baseline;
    flex-wrap: wrap;
  }
  .fact-key {
    min-width: 12rem;
    color: var(--text-quiet, #999);
  }
  .fact-detail {
    color: var(--text-quiet, #999);
  }
  .cap-list {
    list-style: none;
    margin: 0;
    padding: 0;
    display: flex;
    flex-direction: column;
    gap: 0.5rem;
  }
  .cap {
    border: 1px solid var(--border-subtle);
    border-radius: 6px;
    padding: 0.5rem 0.6rem;
    background: var(--surface-sunken);
  }
  /* A row that FAILED or is gated off gets a left rule — the panel's whole
     question is "what is broken right now", so the answer must be findable
     without reading every row. */
  .cap-bad {
    border-left: 3px solid var(--text-danger-soft, #d06b6b);
  }
  .cap-head {
    display: flex;
    gap: 0.4rem;
    align-items: center;
    flex-wrap: wrap;
  }
  .cap-id {
    font-weight: 600;
  }
  .cap-contract {
    margin: 0.35rem 0 0.25rem;
    font-size: var(--font-size-sm);
  }
  .cap-marks {
    display: flex;
    gap: 0.4rem;
    align-items: center;
    flex-wrap: wrap;
    font-size: var(--font-size-sm);
    margin-bottom: 0.2rem;
  }
  .cap-marks .mark {
    color: var(--text-quiet, #999);
  }
  .cap-marks .mark.bad {
    color: var(--text-danger-soft, #d06b6b);
  }
  .cap-more {
    font-size: var(--font-size-sm);
    margin-top: 0.2rem;
  }
  .cap-more summary {
    cursor: pointer;
    color: var(--text-quiet, #999);
  }
</style>
