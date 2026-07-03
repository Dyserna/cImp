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
    graphHistory,
    onGraphStatus,
    type EmbedderProbe,
    type GraphCall,
    type GraphStatus,
  } from './graph';
  import { listenManaged } from './listenManaged';
  import ToolsReference from './ToolsReference.svelte';

  // Reference list of the graph_* MCP tools this feature exposes to Claude (and
  // the offload worker) while the graph is enabled. Mirrors the descriptions in
  // `src-tauri/src/graph/mcp.rs::tool_specs`; kept here as static docs.
  const GRAPH_TOOLS = [
    { name: 'graph_find_symbol', desc: 'Where a symbol (function/struct/trait/…) is defined — file, line, signature.', example: 'Where is GraphService defined?' },
    { name: 'graph_callers', desc: 'Which functions call the given symbol (its call sites). Impact analysis.', example: 'What calls graphRebuild?' },
    { name: 'graph_callees', desc: 'Which symbols are called by the given symbol.', example: 'What does handle_call call?' },
    { name: 'graph_references', desc: 'Every reference (use site) of a name — file, line, column.', example: 'Find all references to ToolDef.' },
    { name: 'graph_imports', desc: 'The modules/paths a file imports.', example: 'What does src/offload/mcp.rs import?' },
    { name: 'graph_outline', desc: 'Every definition in a file, in source order (a structural outline).', example: 'Outline BackendDashboardCard.svelte.' },
    { name: 'graph_transitive', desc: 'Transitive call chain for a symbol — everything it reaches (callees) or that reaches it (callers).', example: 'What does runOffloadTest transitively call?' },
    { name: 'graph_search_docs', desc: 'Keyword search over docs and doc-comments; returns matching snippets.', example: "Search the docs for 'warm pool'." },
    { name: 'graph_struct_search', desc: 'Find code by AST shape via a tree-sitter query (not text).', example: 'Find every .unwrap() in the Rust code.' },
    { name: 'graph_semantic_docs', desc: 'Meaning-based (embedding) search over docs — only when Semantic search is enabled.', example: 'Find docs about how offload timeouts are handled.' },
  ];

  let roots = $state<GraphStatus[]>([]);
  let paused = $state<boolean>(false);
  let busy = $state<boolean>(false);
  let probe = $state<EmbedderProbe | null>(null);
  let probing = $state<boolean>(false);
  let history = $state<GraphCall[]>([]);
  let poll: ReturnType<typeof setInterval> | null = null;

  function fmtTime(ms: number): string {
    return ms ? new Date(ms).toLocaleTimeString() : '—';
  }
  function fmtSize(chars: number): string {
    return chars >= 1000 ? `${(chars / 1000).toFixed(1)}k chars` : `${chars} chars`;
  }

  function upsert(s: GraphStatus): void {
    const i = roots.findIndex((r) => r.root === s.root);
    if (i >= 0) roots[i] = s;
    else roots = [...roots, s];
    // `watch_paused` is a global toggle mirrored into every status — sync the
    // button state from it so a remount doesn't show the wrong label.
    paused = s.watch_paused;
  }

  async function refresh(): Promise<void> {
    try {
      roots = await graphStatus();
      if (roots.length > 0) paused = roots[0].watch_paused;
    } catch (e) {
      console.warn('graph_status failed', e);
    }
    try {
      history = await graphHistory();
    } catch {
      /* ignore — history is best-effort */
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

  // Registered at component init (not in the async onMount) so its teardown is
  // armed before any await — avoids the unmount-during-await listener leak.
  listenManaged(() => onGraphStatus(upsert));

  onMount(async () => {
    await refresh();
    // A light poll backstops the event for coverage/progress counters that
    // change without a discrete state transition.
    poll = setInterval(refresh, 2000);
    // Probe the embedder once on open so reachability is visible immediately,
    // without waiting for a backfill to populate the per-root embed status.
    void testEmbedder();
  });

  onDestroy(() => {
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

        {#if r.langs && r.langs.length > 0}
          <div class="langs" title="Indexed files per language">
            {#each r.langs as l (l.lang)}
              <span class="lang-cell">
                <span class="lang-name" title={l.lang}>{l.lang}</span>
                <span class="lang-n">{l.files}</span>
              </span>
            {/each}
          </div>
        {/if}

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

  <section class="card history">
    <div class="history-head">Recent calls <span class="muted">(newest first)</span></div>
    <div class="history-body">
      {#if history.length === 0}
        <div class="history-empty">
          No graph calls yet — query the graph from a Claude tab or via offload_task.
        </div>
      {:else}
        <div class="history-rows">
          {#each history as c, i (i)}
            <div class="hrow" class:err={!c.ok}>
              <span class="htime">{fmtTime(c.ts_ms)}</span>
              <span class="hsrc {c.source}">{c.source}</span>
              <span class="htool">{c.tool.replace('graph_', '')}</span>
              <span class="htarget" title={c.target}>{c.target}</span>
              <span class="hmeta">{c.ms}ms · {fmtSize(c.chars)}</span>
            </div>
          {/each}
        </div>
      {/if}
    </div>
  </section>

  <ToolsReference
    title="Graph tools"
    tools={GRAPH_TOOLS}
    note="MCP tools exposed to Claude (and the offload worker) while the graph is enabled. Ask in natural language — Claude picks the tool."
  />
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
  /* Per-language file counts. A grid of auto-filled columns: each cell is a
     single line ("lang  N"); the column count grows/shrinks with the tab
     width, so more languages pack horizontally and vertical growth is
     minimized. */
  .langs {
    display: grid;
    grid-template-columns: repeat(auto-fill, minmax(9.5rem, 1fr));
    gap: 2px 10px;
    margin: 0 0 10px;
  }
  .lang-cell {
    display: flex;
    align-items: baseline;
    justify-content: space-between;
    gap: 6px;
    min-width: 0;
    padding: 1px 6px;
    border-radius: 4px;
    background: rgba(255, 255, 255, 0.03);
    font-size: 11px;
    line-height: 1.5;
    white-space: nowrap;
    font-variant-numeric: tabular-nums;
  }
  .lang-name {
    overflow: hidden;
    text-overflow: ellipsis;
    opacity: 0.8;
  }
  .lang-n {
    flex: 0 0 auto;
    font-weight: 600;
    opacity: 0.95;
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
  .history {
    /* One row's box height; five are reserved below, then it scrolls. */
    --hrow-h: 1.55rem;
  }
  .history-head {
    font-weight: 600;
    margin-bottom: 6px;
  }
  .history-body {
    height: calc(5 * var(--hrow-h));
    overflow-y: auto;
  }
  .history-empty {
    opacity: 0.6;
    font-style: italic;
  }
  .history-rows {
    display: flex;
    flex-direction: column;
  }
  .hrow {
    display: grid;
    grid-template-columns: 5.5rem 4rem 6.5rem 1fr 8.5rem;
    align-items: center;
    gap: 8px;
    height: var(--hrow-h);
    box-sizing: border-box;
    padding: 0 4px;
    border-bottom: 1px solid var(--border, #2a2a2a);
    font-size: 0.86em;
    white-space: nowrap;
    font-variant-numeric: tabular-nums;
  }
  .hrow.err {
    color: #ff8a80;
  }
  .hsrc {
    text-transform: uppercase;
    font-size: 0.82em;
    font-weight: 600;
    opacity: 0.85;
  }
  .hsrc.claude {
    color: #58a6ff;
  }
  .hsrc.offload {
    color: #3fb950;
  }
  .htarget {
    overflow: hidden;
    text-overflow: ellipsis;
  }
  .hmeta {
    text-align: right;
    opacity: 0.7;
  }
  .muted {
    opacity: 0.6;
    font-weight: 400;
  }
</style>
