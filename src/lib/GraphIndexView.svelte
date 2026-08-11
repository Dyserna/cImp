<script lang="ts">
  // The graph index dashboard — per-root build status, node/edge counts,
  // language census, and embedding health for the per-project code graph,
  // plus the same actions as Settings (rebuild / rebuild embeddings / test
  // the embedder / pause watch). Formerly the Index group of the Code
  // Intelligence tab's Overview; it now renders as the "Graph index" section
  // INSIDE the Tool Activity tab (ToolActivityView.svelte), which mounts it
  // while the section is selected. Seeds from the `graph_status` IPC, tracks
  // live transitions via the `graph-status` event, and a light poll backstops
  // coverage/progress counters that change without a discrete transition.
  import { onMount, onDestroy } from 'svelte';
  import {
    graphStatus,
    graphRebuild,
    graphRebuildEmbeddings,
    graphSetWatchPaused,
    graphTestEmbedder,
    graphLanguageCensus,
    graphSetLanguageEnabled,
    onGraphStatus,
    type EmbedderProbe,
    type GraphStatus,
    type LangCensus,
  } from './graph';
  import { listenManaged } from './listenManaged';
  import { TOOL_ACTIVITY_TAB_ID } from './tabs/types';
  import { isAppViewVisible, onAppViewShown } from './appViewVisibility';

  let roots = $state<GraphStatus[]>([]);
  // Index cards in a stable, hierarchy-shaped order: shallower paths first
  // (the root project tops the list, sub-projects sit below it), and projects
  // at the same depth alphabetically. `roots` itself keeps backend arrival
  // order — only the display is sorted.
  const sortedRoots = $derived(
    [...roots].sort((a, b) => rootDepth(a.root) - rootDepth(b.root) || cmpPath(a.root, b.root)),
  );
  function rootDepth(p: string): number {
    // Count path segments, tolerant of either separator and a trailing slash.
    return p.replace(/[\\/]+$/, '').split(/[\\/]+/).length;
  }
  function cmpPath(a: string, b: string): number {
    return a.localeCompare(b, undefined, { sensitivity: 'base', numeric: true });
  }
  let paused = $state<boolean>(false);
  let busy = $state<boolean>(false);

  let probe = $state<EmbedderProbe | null>(null);
  let probing = $state<boolean>(false);
  let poll: ReturnType<typeof setInterval> | null = null;

  // Per-root language census (all languages present on disk, classified
  // green/yellow/red). Walking the tree is comparatively expensive, so it's
  // refreshed only on open, when a root first appears, after a build finishes,
  // and right after a toggle — never on the 2s status poll.
  let census = $state<Record<string, LangCensus[]>>({});
  // Key of the language whose add/remove is in flight (disables the whole grid
  // so a double-click can't race two rebuilds).
  let langBusy = $state<string | null>(null);
  // Tracks each root's previous `building` flag so we can refetch the census
  // exactly on the building→done edge (new file counts land after a rebuild).
  let wasBuilding: Record<string, boolean> = {};

  function langColor(e: LangCensus): 'green' | 'yellow' | 'red' {
    if (!e.supported) return 'red';
    return e.enabled ? 'green' : 'yellow';
  }

  function langTitle(e: LangCensus): string {
    if (!e.supported) return `${e.label}: not supported by the code graph`;
    return e.enabled
      ? `${e.label}: indexed — click to remove it from the graph`
      : `${e.label}: supported — click to add it to the graph`;
  }

  // Fetch the census for roots that just appeared or just finished building.
  // Called at the tail of every `refresh()`; the edge checks keep it from
  // walking the tree on every poll tick.
  async function maybeRefreshCensus(): Promise<void> {
    for (const r of roots) {
      const finished = wasBuilding[r.root] && !r.building;
      const missing = !(r.root in census);
      if (missing || finished) {
        try {
          census[r.root] = await graphLanguageCensus(r.root);
        } catch (e) {
          console.warn('graph_language_census failed', e);
        }
      }
      wasBuilding[r.root] = r.building;
    }
  }

  async function toggleLang(root: string, entry: LangCensus): Promise<void> {
    // Red (unsupported) chips are informational; ignore clicks. Serialize
    // toggles so two rebuilds can't stack.
    if (!entry.supported || langBusy !== null) return;
    langBusy = entry.key;
    try {
      await graphSetLanguageEnabled(entry.key, !entry.enabled, root);
      // Settings are mutated synchronously in the command, so the census now
      // reflects the new enabled state — the button flips colour immediately,
      // ahead of the rebuild that indexes the files.
      census[root] = await graphLanguageCensus(root);
      await refresh(); // surface the building badge the rebuild just set
    } catch (e) {
      console.error('graph_set_language_enabled failed', e);
    } finally {
      langBusy = null;
    }
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
    // Refresh the per-root language census only on a root's appear/build-done
    // edge (cheap on a steady poll, fresh counts right after a rebuild).
    await maybeRefreshCensus();
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

  // The Tool Activity tab is keep-alive (appViews.ts), so this section stays
  // mounted while the tab is hidden: the poll idles off-screen and a fresh
  // refresh runs the moment the tab comes back. Re-probe the embedder too —
  // nothing else updates it, so an embedder started while the tab was hidden
  // would otherwise show "unreachable" until a manual probe.
  const unsubShown = onAppViewShown(TOOL_ACTIVITY_TAB_ID, () => {
    void refresh();
    void testEmbedder();
  });

  onMount(async () => {
    await refresh();
    // A light poll backstops the event for coverage/progress counters that
    // change without a discrete state transition.
    poll = setInterval(() => {
      if (isAppViewVisible(TOOL_ACTIVITY_TAB_ID)) void refresh();
    }, 2000);
    // Probe the embedder once on open so reachability is visible immediately,
    // without waiting for a backfill to populate the per-root embed status.
    void testEmbedder();
  });

  onDestroy(() => {
    if (poll) clearInterval(poll);
    unsubShown();
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

<!-- Normal flow (no absolute inset) so the Tool Activity container keeps
     owning the scroll — same convention as OffloadServerView. -->
<div class="graph-index">
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

  {#if probe}
    <p class="probe {probe.ok ? 'ok' : 'err'}">
      <span class="probe-dot"></span>
      Embedder: {probe.message}
    </p>
  {/if}

  {#if roots.length === 0}
    <p class="empty">
      No project indexed yet. Enable the graph in Settings → Code Intelligence and click
      <strong>Rebuild index</strong>.
    </p>
  {:else}
    {#each sortedRoots as r (r.root)}
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

        {#if census[r.root] && census[r.root].length > 0}
          <div class="lang-legend">
            <span><span class="dot green"></span>indexed</span>
            <span><span class="dot yellow"></span>available — click to add</span>
            <span><span class="dot red"></span>unsupported</span>
          </div>
          <div class="langs">
            {#each census[r.root] as l (l.key)}
              <button
                type="button"
                class="lang-btn {langColor(l)}"
                class:busy={langBusy === l.key}
                disabled={!l.supported || langBusy !== null || r.building}
                title={langTitle(l)}
                onclick={() => toggleLang(r.root, l)}
              >
                <span class="lang-name">{l.label}</span>
                <span class="lang-n">{l.files}</span>
              </button>
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
              {#if r.code_embed_total > 0}<span>· code: {r.code_embedded}/{r.code_embed_total} chunks</span>{/if}
              <span>· embedder: {r.embedder_configured ? (r.embedder_ready ? 'ready' : 'unreachable') : 'not configured'}</span>
            </div>
          {/if}
          {#if r.digests > 0}
            <div class="embed-meta"><span>{r.digests} context digest{r.digests === 1 ? '' : 's'} cached</span></div>
          {/if}
          {#if r.semantic_enabled && r.embed_error}
            <!-- A message on a healthy state (e.g. "N chunks skipped") is an
                 advisory, not an outage: render it as a warning so a completed
                 run with a few rejected chunks doesn't read as a failure. -->
            {@const fatal = r.embed_state === 'degraded' || r.embed_state === 'error'}
            <p class="error" class:notice={!fatal}>
              {fatal ? 'Embedder:' : 'Note:'}
              {r.embed_error}
            </p>
          {/if}
        </div>
      </section>
    {/each}
  {/if}
</div>

<style>
  .graph-index {
    font-size: 13px;
    color: var(--text-primary, #ddd);
  }
  .actions {
    display: flex;
    gap: 8px;
    margin-bottom: 12px;
  }
  button {
    padding: 4px 10px;
    border-radius: 5px;
    border: 1px solid var(--border-default, #444);
    background: var(--accent, #3b6ea5);
    color: var(--accent-fg, #fff);
    cursor: pointer;
    font-size: 12px;
  }
  button.secondary {
    background: transparent;
    color: var(--text-primary, #ddd);
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
    border: 1px solid var(--border-default, #444);
  }
  .probe.ok {
    background: var(--surface-success, rgba(46, 125, 50, 0.18));
    border-color: var(--success, #2e7d32);
    color: var(--text-success, #b8e6bb);
  }
  .probe.err {
    background: var(--surface-danger, rgba(179, 38, 30, 0.18));
    border-color: var(--border-danger, #b3261e);
    color: var(--text-danger-soft, #ffb4ab);
  }
  .probe-dot {
    width: 8px;
    height: 8px;
    border-radius: 50%;
    flex: 0 0 auto;
    background: currentColor;
  }
  .card {
    border: 1px solid var(--border-subtle, #3a3a3a);
    border-radius: 8px;
    padding: 12px;
    margin-bottom: 12px;
    background: var(--surface-card, #1e1e1e);
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
    display: inline-block;
    margin-left: 6px;
    padding: 1px 8px;
    border-radius: 10px;
    background: var(--surface-4, #444);
    color: var(--text-bright, #fff);
    font-size: 11px;
    font-weight: 600;
    line-height: 16px;
    vertical-align: middle;
    text-transform: capitalize;
  }
  .badge.ok {
    background: var(--surface-success, #2e7d32);
    color: var(--text-success, #fff);
  }
  .badge.busy {
    background: var(--surface-info, #1565c0);
    color: var(--text-info, #fff);
  }
  .badge.warn {
    background: var(--surface-warning, #b26a00);
    color: var(--text-warning, #fff);
  }
  .badge.err {
    background: var(--surface-danger-strong, #b3261e);
    color: var(--text-danger-strong, #fff);
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
  /* Language buttons. A grid of auto-filled columns: each cell is a single
     line ("Lang  N") with a colour-coded outline — green = indexed, yellow =
     supported-but-off (click to add), red = unsupported. The column count
     grows/shrinks with the tab width so languages pack horizontally. */
  .lang-legend {
    display: flex;
    flex-wrap: wrap;
    gap: 4px 14px;
    margin: 2px 0 8px;
    font-size: 10.5px;
    opacity: 0.7;
  }
  .lang-legend span {
    display: inline-flex;
    align-items: center;
    gap: 5px;
  }
  .lang-legend .dot {
    width: 8px;
    height: 8px;
    border-radius: 2px;
    border: 1.5px solid;
    display: inline-block;
  }
  .lang-legend .dot.green {
    border-color: var(--success, #2e7d32);
  }
  .lang-legend .dot.yellow {
    border-color: var(--warning, #b26a00);
  }
  .lang-legend .dot.red {
    border-color: var(--danger, #b3261e);
  }
  .langs {
    display: grid;
    grid-template-columns: repeat(auto-fill, minmax(9.5rem, 1fr));
    gap: 6px 8px;
    margin: 0 0 10px;
  }
  .lang-btn {
    display: flex;
    align-items: baseline;
    justify-content: space-between;
    gap: 6px;
    min-width: 0;
    padding: 3px 8px;
    border-radius: 5px;
    border: 1.5px solid var(--border-default, #444);
    background: transparent;
    color: inherit;
    font-size: 11px;
    line-height: 1.5;
    white-space: nowrap;
    font-variant-numeric: tabular-nums;
    cursor: pointer;
    transition:
      background 0.12s ease,
      filter 0.12s ease,
      opacity 0.12s ease;
  }
  .lang-btn .lang-name {
    overflow: hidden;
    text-overflow: ellipsis;
  }
  .lang-btn .lang-n {
    flex: 0 0 auto;
    font-weight: 600;
  }
  .lang-btn.green {
    border-color: var(--success, #2e7d32);
    color: var(--text-success, #b8e6bb);
  }
  .lang-btn.yellow {
    border-color: var(--warning, #b26a00);
    color: var(--text-warning, #f0c674);
  }
  .lang-btn.red {
    border-color: var(--danger, #b3261e);
    color: var(--text-danger-soft, #ffb4ab);
    cursor: default;
  }
  .lang-btn.green:hover:not(:disabled),
  .lang-btn.yellow:hover:not(:disabled) {
    background: rgba(255, 255, 255, 0.06);
    filter: brightness(1.12);
  }
  .lang-btn:focus-visible {
    outline: 2px solid var(--accent, #3b6ea5);
    outline-offset: 1px;
  }
  /* Only the toggleable (green/yellow) chips dim while disabled; red is purely
     informational so it stays at full readability. */
  .lang-btn.green:disabled,
  .lang-btn.yellow:disabled {
    opacity: 0.5;
  }
  .lang-btn.busy {
    animation: lang-pulse 1s ease-in-out infinite;
  }
  @keyframes lang-pulse {
    0%,
    100% {
      opacity: 0.5;
    }
    50% {
      opacity: 0.85;
    }
  }
  .section-label {
    font-size: 11px;
    text-transform: uppercase;
    letter-spacing: 0.04em;
    opacity: 0.7;
  }
  .embed {
    margin-top: 10px;
    border-top: 1px solid var(--border-subtle, #333);
    padding-top: 10px;
  }
  .bar {
    height: 6px;
    border-radius: 3px;
    background: var(--surface-3, #333);
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
    color: var(--text-danger-soft, #ff8a80);
    font-size: 12px;
    margin: 6px 0 0;
  }
  /* Advisory variant of `.error` — same slot, warning colour. */
  .error.notice {
    color: var(--text-warning, #f0c674);
  }
</style>
