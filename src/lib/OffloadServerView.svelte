<script lang="ts">
  // V8-03: the read-only Offload Server tab's content — a live dashboard of
  // the local llama-server (slots, throughput, queue, context, request
  // history), driven by the backend metrics poller via the
  // `offload-server-metrics` event. The raw server log is kept underneath in
  // a collapsible section (model-load progress + errors).
  import { onMount, onDestroy } from 'svelte';
  import {
    offloadServerMetrics,
    onOffloadServerMetrics,
    offloadServerLog,
    onOffloadServerOutput,
    type ServerMetrics,
  } from './offload';

  let metrics = $state<ServerMetrics | null>(null);
  let showLog = $state(false);
  let logLines = $state<string[]>([]);
  let logEl = $state<HTMLElement | null>(null);
  let unlistenMetrics: (() => void) | null = null;
  let unlistenLog: (() => void) | null = null;

  onMount(async () => {
    metrics = await offloadServerMetrics();
    unlistenMetrics = await onOffloadServerMetrics((m) => (metrics = m));
    try {
      logLines = await offloadServerLog();
    } catch {
      /* ignore */
    }
    unlistenLog = await onOffloadServerOutput((l) => {
      logLines = [...logLines, l.line].slice(-800);
    });
  });

  onDestroy(() => {
    unlistenMetrics?.();
    unlistenLog?.();
  });

  // Pin the raw-log view to the newest line while it's open.
  $effect(() => {
    logLines;
    if (showLog && logEl) logEl.scrollTop = logEl.scrollHeight;
  });

  function fmtTime(ms: number): string {
    if (!ms) return '—';
    return new Date(ms).toLocaleTimeString();
  }
  function fmtTps(n: number | null | undefined): string {
    return n == null ? '—' : `${Math.round(n)} tok/s`;
  }
  function clampPct(n: number): number {
    return Math.max(0, Math.min(100, n));
  }

  const running = $derived(metrics?.running ?? false);
  // Prefer the server-computed throughput (/metrics) when present, else the
  // sum of per-slot computed rates.
  const throughput = $derived(metrics?.predicted_tps ?? metrics?.aggregate_tps ?? 0);
  const slotsPct = $derived(
    metrics && metrics.total_slots > 0
      ? clampPct((metrics.busy_slots / metrics.total_slots) * 100)
      : 0,
  );
</script>

<div class="dash">
  {#if !running}
    <div class="offline">
      <span class="dot off"></span>
      <span>llama-server not running — Start the local backend, or it loads on the first offload.</span>
    </div>
  {:else if metrics}
    <div class="header">
      <div class="title">
        <span class="dot on"></span>
        Offload Server
        {#if metrics.n_ctx_per_slot}<span class="muted">· {metrics.n_ctx_per_slot.toLocaleString()} ctx/slot</span>{/if}
      </div>

      <div class="stats">
        <div class="stat">
          <span class="label">Slots</span>
          <div class="bar"><div class="fill" style="width:{slotsPct}%"></div></div>
          <span class="value">{metrics.busy_slots} / {metrics.total_slots} busy</span>
        </div>

        <div class="stat">
          <span class="label">Queue</span>
          <span class="value">
            {metrics.requests_deferred ?? '—'} waiting
            <span class="muted">· {metrics.global_in_flight}/{metrics.global_cap} in flight</span>
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
            <span class="value muted" title="Add --metrics to your server command for queue depth, throughput, and context %">
              add <code>--metrics</code>
            </span>
          </div>
        {/if}
      </div>
      {#if metrics.metrics_available && metrics.kv_cache_pct == null}
        <div class="note">
          Context-fill % isn't exposed by this llama.cpp build — the per-slot
          bars below show generated tokens vs the slot window.
        </div>
      {/if}
    </div>

    <div class="slots">
      {#each metrics.slots as slot (slot.id)}
        <div class="slot" class:active={slot.processing}>
          <span class="slot-id">Slot {slot.id}</span>
          <span class="slot-state">
            {#if slot.processing}<span class="dot on pulse"></span> generating{:else}<span class="dot idle"></span> idle{/if}
          </span>
          <span class="slot-tok">{slot.n_decoded.toLocaleString()} / {slot.n_ctx.toLocaleString()}</span>
          <span class="slot-tps">{slot.processing ? fmtTps(slot.tps) : ''}</span>
          <div class="bar slim">
            {#if slot.n_ctx > 0}
              <div class="fill" style="width:{clampPct((slot.n_decoded / slot.n_ctx) * 100)}%"></div>
            {/if}
          </div>
        </div>
      {/each}
    </div>

    <div class="history">
      <div class="history-head">History <span class="muted">(newest first)</span></div>
      {#if metrics.history.length === 0}
        <div class="history-empty">No requests yet.</div>
      {:else}
        <div class="history-rows">
          {#each metrics.history as r (r.start_ms + '-' + r.slot)}
            <div class="hrow">
              <span class="htime">{fmtTime(r.start_ms)} → {fmtTime(r.end_ms)}</span>
              <span class="hdur">{r.duration_s.toFixed(1)}s</span>
              <span class="hslot">slot {r.slot}</span>
              <span class="htok">{r.tokens.toLocaleString()} tok</span>
              <span class="htps">{Math.round(r.avg_tps)} tok/s</span>
            </div>
          {/each}
        </div>
      {/if}
    </div>
  {/if}

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
</div>

<style>
  .dash {
    position: absolute;
    inset: 0;
    overflow: auto;
    padding: 0.8rem 1rem;
    font-family: var(--font-sans, system-ui, sans-serif);
    font-size: var(--font-size-md, 13px);
    color: var(--text-primary, #c9d1d9);
    background: var(--surface-0, #0d1117);
  }
  .offline {
    display: flex;
    align-items: center;
    gap: 0.5rem;
    color: var(--text-secondary, #8b949e);
    padding: 1rem 0;
  }
  .header {
    border-bottom: 1px solid var(--border-subtle, #21262d);
    padding-bottom: 0.7rem;
    margin-bottom: 0.7rem;
  }
  .title {
    display: flex;
    align-items: center;
    gap: 0.4rem;
    font-weight: 600;
    font-size: 1.05em;
    margin-bottom: 0.6rem;
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
    margin-bottom: 0.9rem;
  }
  .slot {
    display: grid;
    grid-template-columns: 3.5rem 7.5rem 9rem 5.5rem 1fr;
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
  .history-head {
    font-weight: 600;
    margin-bottom: 0.3rem;
  }
  .history-empty {
    color: var(--text-secondary, #8b949e);
    font-style: italic;
  }
  .history-rows {
    display: flex;
    flex-direction: column;
  }
  .hrow {
    display: grid;
    grid-template-columns: minmax(11rem, 1.5fr) 4rem 4rem 1fr 6rem;
    gap: 0.5rem;
    padding: 0.2rem 0.3rem;
    border-bottom: 1px solid var(--border-subtle, #21262d);
    font-variant-numeric: tabular-nums;
    font-size: 0.92em;
  }
  .hrow .htok,
  .hrow .htps {
    text-align: right;
  }
  .hdur,
  .hslot {
    color: var(--text-secondary, #8b949e);
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
</style>
