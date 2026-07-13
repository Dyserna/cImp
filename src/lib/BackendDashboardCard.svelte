<script lang="ts">
  // One backend's card in the Offload Server tab. Local owned servers and
  // reachable LAN llama-servers render the full live dashboard (slots,
  // throughput, queue, context); cloud/down backends render a compact status
  // line. The per-request History feed lives in the Tool Activity tab. The
  // raw server log is available only for Local backends (cImp owns their
  // process); remote logs live on the box.
  import { onMount } from 'svelte';
  import {
    offloadServerLog,
    onOffloadServerOutput,
    type BackendDashboard,
  } from './offload';
  import { listenManaged } from './listenManaged';

  let { dash }: { dash: BackendDashboard } = $props();

  const metrics = $derived(dash.metrics);
  const isLocal = $derived(dash.kind === 'local');
  const kindLabel = $derived(
    dash.kind === 'local' ? 'local' : dash.kind === 'lan' ? 'LAN' : 'cloud',
  );

  // The full live dashboard renders only when the backend is actively polled.
  const live = $derived(dash.state === 'ready' && metrics.running);

  function stateText(): string {
    switch (dash.state) {
      case 'ready':
        // Reachable but not polled (cloud) — show the headline.
        return metrics.n_ctx_per_slot
          ? `Ready — ${metrics.n_ctx_per_slot.toLocaleString()} ctx/slot, ${metrics.total_slots} slot${metrics.total_slots === 1 ? '' : 's'} (no live dashboard)`
          : 'Ready (no live dashboard)';
      case 'starting':
        return 'Starting — loading model…';
      case 'stopped':
        return 'Stopped — starts on the first offload, or click Start in Settings.';
      case 'unreachable':
        return 'Unreachable — the endpoint did not answer /health.';
      case 'blocked':
        return 'Needs cloud consent — enable it in Settings → Offload.';
      case 'disabled':
        return 'Disabled.';
      default:
        return dash.state;
    }
  }

  // ── Offload runs (run log) + raw log ──────────────────────────────────
  let showRuns = $state(false);
  let showLog = $state(false);
  let logLines = $state<string[]>([]);
  let logEl = $state<HTMLElement | null>(null);

  // Armed at init (not in the async onMount) so teardown survives an unmount
  // during the await. Local backends only — remote logs live on the box.
  // `dash.kind`/`dash.name` are stable for a card's lifetime (a card is keyed
  // by backend), so reading them at init is intentional.
  // svelte-ignore state_referenced_locally
  if (dash.kind === 'local') {
    listenManaged(() =>
      onOffloadServerOutput((l) => {
        if (l.backend === dash.name) logLines = [...logLines, l.line].slice(-800);
      }),
    );
  }

  onMount(async () => {
    if (!isLocal) return;
    try {
      logLines = await offloadServerLog(dash.name);
    } catch {
      /* ignore */
    }
  });

  $effect(() => {
    logLines;
    if (showLog && logEl) logEl.scrollTop = logEl.scrollHeight;
  });

  function fmtTps(n: number | null | undefined): string {
    return n == null ? '—' : `${Math.round(n)} tok/s`;
  }
  // Outcome → color, applied via a `style:` directive (always wins over the
  // stylesheet, never pruned). Hardcoded hex — NOT theme vars — so it can't be
  // remapped to a theme's coral/magenta. green=success, red=failed,
  // amber=recovered, blue=running.
  function outcomeColor(outcome: string): string {
    switch (outcome) {
      case 'success':
        return '#3fb950';
      case 'failed':
        return '#f85149';
      case 'recovered':
        return '#d29922';
      case 'running':
        return '#58a6ff';
      default:
        return '#6e7681';
    }
  }
  function clampPct(n: number): number {
    return Math.max(0, Math.min(100, n));
  }

  const throughput = $derived(metrics.predicted_tps ?? metrics.aggregate_tps ?? 0);
  const slotsPct = $derived(
    metrics.total_slots > 0 ? clampPct((metrics.busy_slots / metrics.total_slots) * 100) : 0,
  );
  const dotClass = $derived(
    live ? 'on' : dash.state === 'ready' ? 'on' : dash.state === 'starting' ? 'idle' : 'off',
  );
</script>

<div class="card" class:live>
  <div class="card-head">
    <span class="dot {dotClass}"></span>
    <span class="cname">{dash.name}</span>
    <span class="badge {dash.kind}">{kindLabel}</span>
    {#if live && metrics.n_ctx_per_slot}
      <span class="muted">· {metrics.n_ctx_per_slot.toLocaleString()} ctx/slot</span>
    {/if}
  </div>

  {#if live}
    <div class="stats">
      <div class="stat">
        <span class="label">Slots</span>
        <div class="bar"><div class="fill" style="width:{slotsPct}%"></div></div>
        <span class="value">{metrics.busy_slots} / {metrics.total_slots} busy</span>
      </div>

      <div class="stat">
        <span class="label">Queue</span>
        <span class="value">
          {metrics.queue_depth} queued
          <span class="muted"
            >· {metrics.global_in_flight}/{metrics.global_cap} in flight{#if metrics.requests_deferred}
              · {metrics.requests_deferred} server-deferred{/if}</span
          >
        </span>
      </div>

      <div class="stat">
        <span class="label">Generation</span>
        <span class="value strong">{fmtTps(throughput)}</span>
      </div>

      {#if metrics.prompt_tps != null}
        <div class="stat">
          <span class="label">Prefill</span>
          <span class="value">{fmtTps(metrics.prompt_tps)}</span>
        </div>
      {/if}

      {#if metrics.kv_cache_pct != null}
        <div class="stat">
          <span class="label">Context</span>
          <div class="bar"><div class="fill ctx" style="width:{clampPct(metrics.kv_cache_pct)}%"></div></div>
          <span class="value">{Math.round(metrics.kv_cache_pct)}%</span>
        </div>
      {:else if !metrics.metrics_available}
        <div class="stat">
          <span class="label">Context</span>
          <span class="value muted" title="Add --metrics to the server command for queue depth, throughput, and context %">
            add <code>--metrics</code>
          </span>
        </div>
      {/if}
    </div>

    {#if metrics.metrics_available && metrics.kv_cache_pct == null}
      <div class="note">
        Context-fill % isn't exposed by this llama.cpp build — the per-slot bars
        below show generated tokens vs the slot window.
      </div>
    {/if}

    <div class="slots">
      {#each metrics.slots as slot (slot.id)}
        <div class="slot" class:active={slot.processing}>
          <span class="slot-id">Slot {slot.id}</span>
          <span class="slot-state">
            {#if slot.processing}<span class="dot on pulse"></span> generating{:else}<span class="dot idle"></span> idle{/if}
          </span>
          <span
            class="slot-tok"
            title="{slot.n_prompt.toLocaleString()} prompt + {slot.n_decoded.toLocaleString()} generated = total context in use"
          >
            {(slot.n_prompt + slot.n_decoded).toLocaleString()} / {slot.n_ctx.toLocaleString()}
          </span>
          <span class="slot-tps">{slot.processing ? fmtTps(slot.tps) : ''}</span>
          <div class="bar slim">
            {#if slot.n_ctx > 0}
              <div
                class="fill"
                style="width:{clampPct(((slot.n_prompt + slot.n_decoded) / slot.n_ctx) * 100)}%"
              ></div>
            {/if}
          </div>
        </div>
      {/each}
    </div>

    <!-- The per-request History feed moved to the Tool Activity tab's unified
         Activities view (ToolActivityView.svelte). -->
  {:else}
    <div class="status">{stateText()}</div>
  {/if}

  {#if metrics.runs.length > 0}
    <div class="runs">
      <button type="button" class="runs-toggle" onclick={() => (showRuns = !showRuns)}>
        <span class="caret" class:open={showRuns}>▸</span> Offload runs
        <span class="muted">({metrics.runs.length})</span>
      </button>
      {#if showRuns}
        <div class="runs-view">
          {#each metrics.runs as run (run.id)}
            <details class="run" style:border-left-color={outcomeColor(run.outcome)}>
              <summary class="run-sum">
                <span
                  class="run-dot"
                  class:running={run.outcome === 'running'}
                  style:background={outcomeColor(run.outcome)}
                ></span>
                <span class="run-id">#{run.id}</span>
                <span class="run-mode" title="thinking mode">{run.thinking}</span>
                <span class="run-instr" title={run.instructions}>{run.instructions || '(no instructions)'}</span>
                {#if run.escalated_from}
                  <span
                    class="run-escalated"
                    title="Re-run on the quality backend after a partial {run.escalated_from}-tier answer"
                    >↑ escalated</span
                  >
                {/if}
                <span class="run-meta">
                  {run.calls.length} call{run.calls.length === 1 ? '' : 's'}{#if run.ended_ms}
                    · {((run.ended_ms - run.started_ms) / 1000).toFixed(1)}s{/if}
                  · <span class="run-outcome" style:color={outcomeColor(run.outcome)}>{run.outcome}</span>
                </span>
              </summary>
              <div class="run-calls">
                {#if run.calls.length === 0}
                  <div class="crow muted">No calls recorded{run.outcome === 'running' ? ' yet…' : '.'}</div>
                {:else}
                  {#each run.calls as c, i (i)}
                    <div class="crow">
                      <span class="ckind {c.kind}">{c.kind}{#if c.thinking} 🧠{/if}</span>
                      <span class="cstep">step {c.step}</span>
                      <span
                        class="ctok"
                        title="{c.prompt_tokens.toLocaleString()} prompt + {c.output_tokens.toLocaleString()} output"
                      >
                        {(c.prompt_tokens + c.output_tokens).toLocaleString()} tok
                        <span class="muted">· {c.output_tokens.toLocaleString()} out</span>
                      </span>
                      <span class="cdur">{(c.duration_ms / 1000).toFixed(1)}s</span>
                      <span class="ctps">{Math.round(c.tps)} tok/s</span>
                      <span
                        class="cresult"
                        class:bad={c.result === 'empty' ||
                          c.result === 'leaked' ||
                          c.result.startsWith('error')}>{c.result}</span
                      >
                    </div>
                  {/each}
                {/if}
              </div>
            </details>
          {/each}
        </div>
      {/if}
    </div>
  {/if}

  {#if isLocal}
    <div class="rawlog">
      <button type="button" class="rawlog-toggle" onclick={() => (showLog = !showLog)}>
        <span class="caret" class:open={showLog}>▸</span> Raw server log
      </button>
      {#if showLog}
        <div class="rawlog-view" bind:this={logEl}>
          {#if logLines.length === 0}
            <span class="muted">No output captured yet.</span>
          {:else}
            {#each logLines as line, i (i)}<div class="rawline">{line}</div>{/each}
          {/if}
        </div>
      {/if}
    </div>
  {/if}
</div>

<style>
  .card {
    border: 1px solid var(--border-subtle, #21262d);
    border-radius: 7px;
    background: var(--surface-1, #11161d);
    padding: 0.7rem 0.8rem;
  }
  .card.live {
    border-color: var(--border-default, #30363d);
  }
  .card-head {
    display: flex;
    align-items: center;
    gap: 0.45rem;
    font-weight: 600;
    margin-bottom: 0.5rem;
  }
  .cname {
    font-size: 1.02em;
  }
  .badge {
    font-size: 0.72em;
    font-weight: 600;
    text-transform: uppercase;
    letter-spacing: 0.03em;
    padding: 0.08rem 0.4rem;
    border-radius: 4px;
    background: var(--surface-sunken, #161b22);
    color: var(--text-secondary, #8b949e);
    border: 1px solid var(--border-subtle, #21262d);
  }
  .badge.local {
    color: var(--success, #3fb950);
  }
  .badge.cloud {
    color: var(--accent, #58a6ff);
  }
  .status {
    color: var(--text-secondary, #8b949e);
    padding: 0.2rem 0 0.3rem;
    font-size: 0.95em;
  }
  .stats {
    display: grid;
    grid-template-columns: repeat(auto-fit, minmax(13rem, 1fr));
    gap: 0.5rem 1.2rem;
  }
  .stat {
    display: flex;
    align-items: center;
    gap: 0.5rem;
  }
  .stat .label {
    color: var(--text-secondary, #8b949e);
    min-width: 5rem;
    flex: 0 0 auto;
  }
  .stat .value {
    white-space: nowrap;
  }
  .stat .value.strong {
    font-weight: 600;
  }
  .bar {
    flex: 1 1 auto;
    height: 8px;
    min-width: 3rem;
    background: var(--surface-sunken, #161b22);
    border-radius: 4px;
    overflow: hidden;
  }
  .bar.slim {
    height: 5px;
  }
  .fill {
    height: 100%;
    background: var(--success, #3fb950);
    border-radius: 4px;
    transition: width 0.4s ease;
  }
  .fill.ctx {
    background: var(--accent, #58a6ff);
  }
  .slots {
    display: flex;
    flex-direction: column;
    gap: 0.35rem;
    margin: 0.7rem 0 0.8rem;
  }
  .slot {
    display: grid;
    grid-template-columns: 3.5rem 7.5rem 11rem 5.5rem 1fr;
    align-items: center;
    gap: 0.5rem;
    padding: 0.3rem 0.5rem;
    border: 1px solid var(--border-subtle, #21262d);
    border-radius: 5px;
    background: var(--surface-sunken, #161b22);
  }
  .slot.active {
    border-color: var(--success, #3fb950);
  }
  .slot-id {
    font-weight: 600;
  }
  .slot-state {
    display: flex;
    align-items: center;
    gap: 0.3rem;
    color: var(--text-secondary, #8b949e);
  }
  .slot-tok,
  .slot-tps {
    text-align: right;
    font-variant-numeric: tabular-nums;
  }
  .dot {
    width: 0.55rem;
    height: 0.55rem;
    border-radius: 50%;
    flex: 0 0 auto;
    display: inline-block;
    background: var(--text-secondary, #8b949e);
  }
  .dot.on {
    background: var(--success, #3fb950);
  }
  .dot.idle {
    background: var(--text-tertiary, #6e7681);
  }
  .dot.off {
    background: var(--danger, #d08770);
  }
  .dot.pulse {
    animation: pulse 1.1s ease-in-out infinite;
  }
  @keyframes pulse {
    0%,
    100% {
      opacity: 1;
    }
    50% {
      opacity: 0.35;
    }
  }
  .muted {
    color: var(--text-secondary, #8b949e);
    font-weight: 400;
  }
  .note {
    margin-top: 0.5rem;
    font-size: 0.85em;
    color: var(--text-tertiary, #6e7681);
    font-style: italic;
  }
  .rawlog {
    margin-top: 0.6rem;
    border-top: 1px solid var(--border-subtle, #21262d);
    padding-top: 0.5rem;
  }
  .rawlog-toggle {
    background: none;
    border: none;
    color: var(--text-secondary, #8b949e);
    cursor: pointer;
    font: inherit;
    display: inline-flex;
    align-items: center;
    gap: 0.4rem;
    padding: 0.2rem 0;
  }
  .caret {
    display: inline-block;
    transition: transform 0.12s ease;
  }
  .caret.open {
    transform: rotate(90deg);
  }
  .rawlog-view {
    margin-top: 0.4rem;
    max-height: 14rem;
    overflow: auto;
    background: var(--surface-sunken, #161b22);
    border: 1px solid var(--border-subtle, #21262d);
    border-radius: 4px;
    padding: 0.5rem;
    font-family: var(--font-mono, ui-monospace, monospace);
    font-size: 0.85em;
    line-height: 1.35;
  }
  .rawline {
    white-space: pre-wrap;
    word-break: break-word;
  }
  /* ── Offload runs (run log) ── */
  .runs {
    margin-top: 0.6rem;
    border-top: 1px solid var(--border-subtle, #21262d);
    padding-top: 0.5rem;
  }
  .runs-toggle {
    background: none;
    border: none;
    color: var(--text-secondary, #8b949e);
    cursor: pointer;
    font: inherit;
    display: inline-flex;
    align-items: center;
    gap: 0.4rem;
    padding: 0.2rem 0;
  }
  .runs-view {
    margin-top: 0.4rem;
    max-height: 18rem;
    overflow: auto;
    display: flex;
    flex-direction: column;
    gap: 0.2rem;
  }
  .run {
    border: 1px solid var(--border-subtle, #21262d);
    /* Outcome accent set inline (border-left-color) — green=success,
       red=failed, amber=recovered, blue=running. */
    border-left: 3px solid var(--text-tertiary, #6e7681);
    border-radius: 4px;
    background: var(--surface-sunken, #161b22);
  }
  .run-sum {
    display: flex;
    align-items: center;
    gap: 0.5rem;
    padding: 0.35rem 0.5rem;
    cursor: pointer;
    font-size: 0.85em;
    list-style: none;
  }
  .run-sum::-webkit-details-marker {
    display: none;
  }
  .run-dot {
    width: 0.5rem;
    height: 0.5rem;
    border-radius: 50%;
    flex: 0 0 auto;
    /* background color set inline from the outcome. */
    background: var(--text-tertiary, #6e7681);
  }
  .run-dot.running {
    animation: pulse 1.1s ease-in-out infinite;
  }
  /* Run-log text uses hardcoded neutrals (not the --text-* tokens, which
     follow the terminal palette's --term-fg and can render pink/magenta) so the
     diagnostic panel stays legible in any palette. */
  .run-id {
    font-family: var(--font-mono, ui-monospace, monospace);
    color: #6e7681;
    flex: 0 0 auto;
  }
  .run-mode {
    flex: 0 0 auto;
    text-transform: uppercase;
    font-size: 0.78em;
    letter-spacing: 0.04em;
    color: #8b949e;
    border: 1px solid var(--border-subtle, #21262d);
    border-radius: 3px;
    padding: 0 0.25rem;
  }
  .run-instr {
    flex: 1 1 auto;
    min-width: 0;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
    color: #adbac7;
  }
  .run-meta {
    flex: 0 0 auto;
    color: #768390;
    font-size: 0.92em;
  }
  .run-outcome {
    /* color set inline from the outcome. */
    font-weight: 600;
  }
  .run-escalated {
    flex: none;
    padding: 0 0.3rem;
    border-radius: 3px;
    background: rgba(210, 153, 34, 0.18);
    color: #d29922;
    font-size: 0.82em;
    font-weight: 600;
    white-space: nowrap;
  }
  .run-calls {
    border-top: 1px solid var(--border-subtle, #21262d);
    padding: 0.25rem 0.5rem 0.4rem;
    font-family: var(--font-mono, ui-monospace, monospace);
    font-size: 0.82em;
    line-height: 1.5;
    /* Hardcoded neutral text — NOT the --text-* tokens, which follow the
       terminal palette's --term-fg and can be pink/magenta. The run log is a
       diagnostic table; it should stay legible in any palette. */
    color: #adbac7;
  }
  .run-calls .muted {
    color: #6e7681;
  }
  .crow {
    display: flex;
    align-items: baseline;
    gap: 0.6rem;
    white-space: nowrap;
  }
  .ckind {
    flex: 0 0 6.5rem;
    color: #8b949e;
  }
  .ckind.final {
    color: #58a6ff;
  }
  .ckind.verify {
    color: #d29922;
  }
  .cstep {
    flex: 0 0 3.5rem;
    color: #6e7681;
  }
  .ctok {
    flex: 0 0 auto;
  }
  .cdur,
  .ctps {
    flex: 0 0 auto;
    color: #768390;
  }
  .cresult {
    flex: 1 1 auto;
    text-align: right;
    color: #768390;
  }
  .cresult.bad {
    color: #f85149;
    font-weight: 600;
  }
  code {
    font-family: var(--font-mono, ui-monospace, monospace);
    font-size: 0.9em;
  }
</style>
