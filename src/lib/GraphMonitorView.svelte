<script lang="ts">
  // V9-01 Phase I: the read-only Code Graph monitor tab — an app-rendered
  // dashboard (no PTY) of the per-project graph indexer and embedder. Mirrors
  // the Offload Server tab's reserved/feature-gated nature but is fed by the
  // in-process GraphService rather than a child process's output: it seeds from
  // the `graph_status` IPC, then tracks live transitions via the `graph-status`
  // event, and offers the same actions as Settings (rebuild / rebuild
  // embeddings / pause watch).
  import { onMount, onDestroy } from 'svelte';
  import {
    graphStatus,
    graphRebuild,
    graphRebuildEmbeddings,
    graphSetWatchPaused,
    graphTestEmbedder,
    onGraphStatus,
    type EmbedderProbe,
    type GraphStatus,
  } from './graph';

  let roots = $state<GraphStatus[]>([]);
  let paused = $state<boolean>(false);
  let busy = $state<boolean>(false);
  let probe = $state<EmbedderProbe | null>(null);
  let probing = $state<boolean>(false);
  let unlisten: (() => void) | null = null;
  let poll: ReturnType<typeof setInterval> | null = null;

  function upsert(s: GraphStatus): void {
    const i = roots.findIndex((r) => r.root === s.root);
    if (i >= 0) roots[i] = s;
    else roots = [...roots, s];
  }

  async function refresh(): Promise<void> {
    try {
      roots = await graphStatus();
    } catch (e) {
      console.warn('graph_status failed', e);
    }
  }

  async function testEmbedder(): Promise<void> {
    probing = true;
    try {
      probe = await graphTestEmbedder();
    } catch (e) {
      probe = { ok: false, dim: null, message: String(e) };
    } finally {
      probing = false;
    }
  }

  onMount(async () => {
    await refresh();
    unlisten = await onGraphStatus(upsert);
    // A light poll backstops the event for coverage/progress counters that
    // change without a discrete state transition.
    poll = setInterval(refresh, 2000);
    // Probe the embedder once on open so reachability is visible immediately,
    // without waiting for a backfill to populate the per-root embed status.
    void testEmbedder();
  });

  onDestroy(() => {
    unlisten?.();
    if (poll) clearInterval(poll);
  });

  async function doRebuild(): Promise<void> {
    busy = true;
    try {
      await graphRebuild();
      await refresh();
    } finally {
      busy = false;
    }
  }

  async function doRebuildEmbeddings(): Promise<void> {
    busy = true;
    try {
      await graphRebuildEmbeddings();
      await refresh();
    } finally {
      busy = false;
    }
  }

  async function togglePause(): Promise<void> {
    paused = await graphSetWatchPaused(!paused);
  }

  function pct(n: number, d: number): number {
    return d > 0 ? Math.round((n / d) * 100) : 0;
  }

  function stateClass(s: string): string {
    if (s === 'ready' || s === 'idle') return 'ok';
    if (s === 'building' || s === 'embedding') return 'busy';
    if (s === 'degraded') return 'warn';
    return s === 'error' ? 'err' : '';
  }
</script>

<div class="graph-monitor">
  <header>
    <h2>Code Graph</h2>
    <div class="actions">
      <button onclick={doRebuild} disabled={busy}>Rebuild index</button>
      <button onclick={doRebuildEmbeddings} disabled={busy}>Rebuild embeddings</button>
      <button class="secondary" onclick={testEmbedder} disabled={probing}>
        {probing ? 'Testing…' : 'Test connection'}
      </button>
      <button class="secondary" onclick={togglePause}>
        {paused ? 'Resume watch' : 'Pause watch'}
      </button>
    </div>
  </header>

  {#if probe}
    <p class="probe {probe.ok ? 'ok' : 'err'}">
      <span class="probe-dot"></span>
      Embedder: {probe.message}
    </p>
  {/if}

  {#if roots.length === 0}
    <p class="empty">
      No project indexed yet. Enable the graph in Settings → Code graph and click
      <strong>Rebuild index</strong>.
    </p>
  {:else}
    {#each roots as r (r.root)}
      <section class="card">
        <div class="row title">
          <span class="root" title={r.root}>{r.root}</span>
          <span class="badge {stateClass(r.state)}">
            {r.building ? 'building…' : r.state}
          </span>
        </div>

        <div class="counts">
          <div><span class="num">{r.files}</span><span class="lbl">files</span></div>
          <div><span class="num">{r.symbols}</span><span class="lbl">symbols</span></div>
          <div><span class="num">{r.edges}</span><span class="lbl">edges</span></div>
          <div><span class="num">{r.files_indexed}</span><span class="lbl">last scan</span></div>
        </div>

        {#if r.last_error}
          <p class="error">Index error: {r.last_error}</p>
        {/if}

        <div class="embed">
          <div class="row">
            <span class="section-label">Semantic search</span>
            {#if !r.semantic_enabled}
              <span class="badge">off</span>
            {:else}
              <span class="badge {stateClass(r.embed_state)}">{r.embed_state}</span>
            {/if}
          </div>

          {#if r.semantic_enabled}
            <div class="bar" title="{r.embedded} / {r.embed_total} chunks embedded">
              <div class="fill" style="width: {pct(r.embedded, r.embed_total)}%"></div>
            </div>
            <div class="embed-meta">
              <span>{r.embedded}/{r.embed_total} embedded ({pct(r.embedded, r.embed_total)}%)</span>
              {#if r.embed_pending > 0}<span>· {r.embed_pending} pending</span>{/if}
              <span>· embedder: {r.embedder_configured ? (r.embedder_ready ? 'ready' : 'unreachable') : 'not configured'}</span>
            </div>
            {#if r.embed_error}
              <p class="error">Embedder: {r.embed_error}</p>
            {/if}
          {/if}
        </div>
      </section>
    {/each}
  {/if}
</div>

<style>
  .graph-monitor {
    /* Sit ABOVE the pane's absolutely-positioned (empty) terminal slot, the
       same way OffloadServerView does — otherwise that transparent slot paints
       on top of this static content and swallows every button click. */
    position: absolute;
    inset: 0;
    overflow-y: auto;
    padding: 16px;
    font-size: 13px;
    color: var(--text, #ddd);
    box-sizing: border-box;
  }
  header {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 12px;
    margin-bottom: 14px;
  }
  header h2 {
    margin: 0;
    font-size: 15px;
  }
  .actions {
    display: flex;
    gap: 8px;
  }
  button {
    padding: 4px 10px;
    border-radius: 5px;
    border: 1px solid var(--border, #444);
    background: var(--accent, #3b6ea5);
    color: #fff;
    cursor: pointer;
    font-size: 12px;
  }
  button.secondary {
    background: transparent;
    color: var(--text, #ddd);
  }
  button:disabled {
    opacity: 0.5;
    cursor: default;
  }
  .empty {
    opacity: 0.7;
  }
  .probe {
    display: flex;
    align-items: center;
    gap: 8px;
    margin: -4px 0 14px;
    padding: 7px 10px;
    border-radius: 6px;
    font-size: 12px;
    border: 1px solid var(--border, #444);
  }
  .probe.ok {
    background: rgba(46, 125, 50, 0.18);
    border-color: #2e7d32;
    color: #b8e6bb;
  }
  .probe.err {
    background: rgba(179, 38, 30, 0.18);
    border-color: #b3261e;
    color: #ffb4ab;
  }
  .probe-dot {
    width: 8px;
    height: 8px;
    border-radius: 50%;
    flex: 0 0 auto;
    background: currentColor;
  }
  .card {
    border: 1px solid var(--border, #3a3a3a);
    border-radius: 8px;
    padding: 12px;
    margin-bottom: 12px;
    background: var(--panel, #1e1e1e);
  }
  .row {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 8px;
  }
  .title .root {
    font-family: monospace;
    font-size: 12px;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .badge {
    padding: 1px 8px;
    border-radius: 10px;
    font-size: 11px;
    background: #444;
    text-transform: capitalize;
  }
  .badge.ok {
    background: #2e7d32;
  }
  .badge.busy {
    background: #1565c0;
  }
  .badge.warn {
    background: #b26a00;
  }
  .badge.err {
    background: #b3261e;
  }
  .counts {
    display: flex;
    gap: 18px;
    margin: 10px 0;
  }
  .counts .num {
    font-size: 18px;
    font-weight: 600;
    display: block;
  }
  .counts .lbl {
    font-size: 11px;
    opacity: 0.6;
  }
  .section-label {
    font-size: 11px;
    text-transform: uppercase;
    letter-spacing: 0.04em;
    opacity: 0.7;
  }
  .embed {
    margin-top: 10px;
    border-top: 1px solid var(--border, #333);
    padding-top: 10px;
  }
  .bar {
    height: 6px;
    border-radius: 3px;
    background: #333;
    overflow: hidden;
    margin: 8px 0 6px;
  }
  .bar .fill {
    height: 100%;
    background: var(--accent, #3b6ea5);
    transition: width 0.3s;
  }
  .embed-meta {
    font-size: 11px;
    opacity: 0.75;
    display: flex;
    flex-wrap: wrap;
    gap: 4px;
  }
  .error {
    color: #ff8a80;
    font-size: 12px;
    margin: 6px 0 0;
  }
</style>
