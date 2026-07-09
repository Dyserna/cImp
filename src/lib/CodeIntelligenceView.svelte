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
    graphLanguageCensus,
    graphSetLanguageEnabled,
    graphDeadExports,
    graphCycles,
    graphImpact,
    graphMemory,
    graphMemoryClear,
    graphNoteSetPinned,
    graphContextPreview,
    onGraphStatus,
    type EmbedderProbe,
    type GraphCall,
    type GraphStatus,
    type LangCensus,
    type DeadExportRow,
    type ImpactResult,
    type MemorySnapshot,
    type RetrieveResult,
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
    { name: 'graph_snippet', desc: "Fetch just one definition's body (by symbol, or file+line) instead of reading the whole file. Pair with graph_outline for big files.", example: 'Show the body of dispatch_recorded.' },
    { name: 'graph_repo_map', desc: 'A budget-bounded map of the most call-central files with their top signatures — orient fast at the start of a task.', example: 'Give me a project map.' },
    { name: 'graph_transitive', desc: 'Transitive call chain for a symbol — everything it reaches (callees) or that reaches it (callers).', example: 'What does runOffloadTest transitively call?' },
    { name: 'graph_search_docs', desc: 'Keyword search over docs and doc-comments; returns matching snippets.', example: "Search the docs for 'warm pool'." },
    { name: 'graph_struct_search', desc: 'Find code by AST shape via a tree-sitter query (not text).', example: 'Find every .unwrap() in the Rust code.' },
    { name: 'graph_semantic_docs', desc: 'Meaning-based (embedding) search over docs — only when Semantic search is enabled.', example: 'Find docs about how offload timeouts are handled.' },
    { name: 'graph_semantic_code', desc: 'Meaning-based (embedding) search over symbol bodies — only when "Embed code bodies" is enabled. Returns file:line/kind/signature/distance, never the body; pair with graph_snippet.', example: 'Find code that retries a failed network request.' },
    { name: 'graph_dead_exports', desc: 'Candidate unused public symbols (no reference, no inbound call). Candidates only — may include false positives.', example: 'List candidate dead exports.' },
    { name: 'graph_cycles', desc: 'Import cycles between files (loops of files that import one another).', example: 'Are there any import cycles?' },
    { name: 'graph_impact', desc: 'Blast radius: what could this change break? Defaults to the working-tree diff vs HEAD; pass symbols to analyze specific names instead. include_tests appends an affected-tests block. Results are approximate (name-keyed).', example: 'What would break if I change GraphIndex::dependents_transitive?' },
    { name: 'graph_tests_for', desc: 'Which tests (candidates) would exercise a symbol or file if it changed — the transitive dependents tagged as tests. Candidates only — dynamic dispatch/fixtures aren\'t captured.', example: 'What tests cover dependents_transitive?' },
    { name: 'context_recall', desc: "Recall this session's working set — the files it read/edited/queried and the symbols touched.", example: 'What has this session been working on?' },
    { name: 'context_note', desc: 'Remember a non-obvious decision/fact for this project (pin to keep it across sessions).', example: 'Note: we chose FNV hashing for stability.' },
    { name: 'context_notes', desc: "List this session's notes plus every pinned note for the project.", example: 'Show my remembered notes.' },
  ];

  // Analyses (Phase B2): on-demand dead-export + import-cycle results. Run only
  // when the user clicks — walking the graph is comparatively expensive.
  let deadExports = $state<DeadExportRow[] | null>(null);
  let cycles = $state<string[][] | null>(null);
  let impact = $state<ImpactResult | null>(null);
  let analysisBusy = $state<'dead' | 'cycles' | 'impact' | null>(null);
  let analysisError = $state<string | null>(null);

  async function runDeadExports(): Promise<void> {
    analysisBusy = 'dead';
    analysisError = null;
    try {
      deadExports = await graphDeadExports();
    } catch (e) {
      analysisError = String(e);
    } finally {
      analysisBusy = null;
    }
  }

  async function runCycles(): Promise<void> {
    analysisBusy = 'cycles';
    analysisError = null;
    try {
      cycles = await graphCycles();
    } catch (e) {
      analysisError = String(e);
    } finally {
      analysisBusy = null;
    }
  }

  async function runImpact(): Promise<void> {
    analysisBusy = 'impact';
    analysisError = null;
    try {
      impact = await graphImpact();
    } catch (e) {
      analysisError = String(e);
    } finally {
      analysisBusy = null;
    }
  }

  // Memory (Phase C): per-project session/action memory. Fetched while the
  // Memory section is open (via refresh()'s poll) and on demand.
  let memory = $state<MemorySnapshot | null>(null);

  async function refreshMemory(): Promise<void> {
    try {
      memory = await graphMemory();
    } catch (e) {
      console.warn('graph_memory failed', e);
    }
  }

  async function togglePin(noteId: string, pinned: boolean): Promise<void> {
    try {
      await graphNoteSetPinned(noteId, pinned);
      await refreshMemory();
    } catch (e) {
      console.error('graph_note_set_pinned failed', e);
    }
  }

  async function clearMemory(session?: string): Promise<void> {
    const msg = session
      ? 'Clear this session’s memory?'
      : 'Clear ALL memory for this project?';
    if (!confirm(msg)) return;
    try {
      await graphMemoryClear(session);
      await refreshMemory();
    } catch (e) {
      console.error('graph_memory_clear failed', e);
    }
  }

  function fmtKind(k: string): string {
    return k === 'edit' ? '✎ edit' : k === 'query' ? '⌕ query' : '👁 read';
  }

  // Context (Phase D): a preview surface to see what injection would prepend.
  let previewPrompt = $state('');
  let preview = $state<RetrieveResult | null>(null);
  let previewBusy = $state(false);

  async function runPreview(): Promise<void> {
    if (!previewPrompt.trim()) return;
    previewBusy = true;
    try {
      preview = await graphContextPreview(previewPrompt);
    } catch (e) {
      console.error('graph_context_preview failed', e);
      preview = null;
    } finally {
      previewBusy = false;
    }
  }

  // V10: the tab now hosts five sections. Index/Activity carry the V9 content;
  // Memory/Context/Analyses are filled by later V10 phases. The internal tab id
  // (`graph-monitor`) is unchanged — this is purely the in-view section router.
  type Section = 'index' | 'activity' | 'memory' | 'context' | 'analyses';
  const SECTIONS: { id: Section; label: string }[] = [
    { id: 'index', label: 'Index' },
    { id: 'activity', label: 'Activity' },
    { id: 'memory', label: 'Memory' },
    { id: 'context', label: 'Context' },
    { id: 'analyses', label: 'Analyses' },
  ];
  let section = $state<Section>('index');

  let roots = $state<GraphStatus[]>([]);
  let paused = $state<boolean>(false);
  let busy = $state<boolean>(false);
  let probe = $state<EmbedderProbe | null>(null);
  let probing = $state<boolean>(false);
  let history = $state<GraphCall[]>([]);
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
    // Refresh the per-root language census only on a root's appear/build-done
    // edge (cheap on a steady poll, fresh counts right after a rebuild).
    await maybeRefreshCensus();
    // Memory is only fetched while its section is visible (opens the warm index).
    if (section === 'memory') {
      await refreshMemory();
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
    <h2>Code Intelligence</h2>
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

  <nav class="sections">
    {#each SECTIONS as s (s.id)}
      <button
        type="button"
        class="seg"
        class:active={section === s.id}
        onclick={() => {
          section = s.id;
          if (s.id === 'memory') refreshMemory();
        }}
      >{s.label}</button>
    {/each}
  </nav>

  {#if section === 'index'}
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
            <p class="error">Embedder: {r.embed_error}</p>
          {/if}
        </div>
      </section>
    {/each}
  {/if}

  <ToolsReference
    title="Graph tools"
    tools={GRAPH_TOOLS}
    note="MCP tools exposed to Claude (and the offload worker) while the graph is enabled. Ask in natural language — Claude picks the tool."
  />
  {:else if section === 'activity'}
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
  {:else if section === 'memory'}
    {#if !memory || (memory.working_set.length === 0 && memory.notes.length === 0 && memory.sessions.length === 0)}
      <p class="placeholder">
        No session memory yet. As Claude reads, edits, and queries files (with the
        graph enabled), its working set and notes appear here.
      </p>
    {:else}
      <section class="card">
        <div class="history-head">
          Working set
          <span class="muted">
            {#if memory.current_session}(current session){/if}
          </span>
          <button
            class="mini danger"
            disabled={!memory.current_session}
            onclick={() => memory?.current_session && clearMemory(memory.current_session)}
          >
            Clear session
          </button>
        </div>
        {#if memory.working_set.length === 0}
          <p class="placeholder">Nothing touched in this session yet.</p>
        {:else}
          <div class="rows">
            {#each memory.working_set as w (w.path)}
              <div class="arow ws">
                <span class="aname" title={w.top_symbols.join(', ')}>{w.path}</span>
                <span class="akind">{fmtKind(w.last_kind)}</span>
                <span class="aloc">{w.touches}×{w.top_symbols.length ? ' · ' + w.top_symbols.slice(0, 3).join(', ') : ''}</span>
              </div>
            {/each}
          </div>
        {/if}
      </section>

      <section class="card">
        <div class="history-head">Notes <span class="muted">({memory.notes.length})</span></div>
        {#if memory.notes.length === 0}
          <p class="placeholder">No notes. Ask Claude to <code>context_note</code> a decision.</p>
        {:else}
          <div class="rows">
            {#each memory.notes as n (n.note_id)}
              <div class="arow note">
                <button
                  class="pin"
                  class:pinned={n.pinned}
                  title={n.pinned ? 'Unpin' : 'Pin (keep across sessions)'}
                  onclick={() => togglePin(n.note_id, !n.pinned)}
                >{n.pinned ? '📌' : '📍'}</button>
                <span class="ntext">{n.text}</span>
                <span class="aloc">{fmtTime(n.ts_ms)}</span>
              </div>
            {/each}
          </div>
        {/if}
      </section>

      <section class="card">
        <div class="history-head">
          Recent sessions <span class="muted">({memory.sessions.length})</span>
          <button class="mini danger" onclick={() => clearMemory()}>Clear all</button>
        </div>
        <div class="rows">
          {#each memory.sessions as s (s.session_id)}
            <div class="arow sess" class:current={s.session_id === memory.current_session}>
              <span class="aname" title={s.session_id}>{s.agent}</span>
              <span class="akind">{s.events} events</span>
              <span class="aloc">{fmtTime(s.last_ms)}</span>
            </div>
          {/each}
        </div>
      </section>
    {/if}
  {:else if section === 'context'}
    <div class="context-sec">
      <p class="caveat">
        When enabled (Settings → Code Intelligence → Context injection), cImp
        prepends a budget-bounded digest of the most relevant files to each
        prompt — for Claude via a <code>UserPromptSubmit</code> hook, for OpenCode
        via a generated plugin. Preview below shows what <em>would</em> be injected
        for a prompt, regardless of the toggle.
      </p>

      <section class="card">
        <div class="history-head">Preview injection</div>
        <div class="preview-in">
          <input
            type="text"
            placeholder="Type a prompt to see what would be injected…"
            bind:value={previewPrompt}
            onkeydown={(e) => e.key === 'Enter' && runPreview()}
          />
          <button onclick={runPreview} disabled={previewBusy || !previewPrompt.trim()}>
            {previewBusy ? 'Ranking…' : 'Preview'}
          </button>
        </div>

        {#if preview}
          {#if preview.chars === 0}
            <p class="placeholder">
              Nothing would be injected — no file cleared the relevance threshold.
            </p>
          {:else}
            <p class="preview-meta">
              {preview.files_used.length} file{preview.files_used.length === 1 ? '' : 's'} ·
              {preview.chars} chars · ~{preview.tokens_est} tokens injected
            </p>
            <pre class="preview-md">{preview.context_md}</pre>
          {/if}
        {/if}
      </section>
    </div>
  {:else if section === 'analyses'}
    <div class="analyses">
      <div class="actions">
        <button onclick={runDeadExports} disabled={analysisBusy !== null}>
          {analysisBusy === 'dead' ? 'Scanning…' : 'Find dead exports'}
        </button>
        <button onclick={runCycles} disabled={analysisBusy !== null}>
          {analysisBusy === 'cycles' ? 'Scanning…' : 'Find import cycles'}
        </button>
        <button onclick={runImpact} disabled={analysisBusy !== null}>
          {analysisBusy === 'impact' ? 'Scanning…' : 'Impact of working-tree changes'}
        </button>
      </div>

      {#if analysisError}
        <p class="error">{analysisError}</p>
      {/if}

      {#if deadExports !== null}
        <section class="card">
          <div class="history-head">
            Dead exports <span class="muted">({deadExports.length})</span>
          </div>
          <p class="caveat">
            Candidates only — a symbol reached via dynamic dispatch, an external
            consumer, a macro, or reflection has no static edge and can appear
            here as a false positive; conversely a dead symbol sharing its name
            with a used one is missed. Detection covers languages with visibility
            info: <strong>Rust, JavaScript/TypeScript, Python, Go</strong> (other
            languages report nothing here yet).
          </p>
          {#if deadExports.length === 0}
            <p class="placeholder">No candidate dead exports.</p>
          {:else}
            <div class="rows">
              {#each deadExports as d (d.file + ':' + d.line)}
                <div class="arow">
                  <span class="aname">{d.name}</span>
                  <span class="akind">{d.kind}</span>
                  <span class="aloc" title={d.signature}>{d.file}:{d.line}</span>
                </div>
              {/each}
            </div>
          {/if}
        </section>
      {/if}

      {#if cycles !== null}
        <section class="card">
          <div class="history-head">
            Import cycles <span class="muted">({cycles.length})</span>
          </div>
          <p class="caveat">
            Import resolution covers <strong>JavaScript/TypeScript, Python,
            Rust</strong>; other languages aren't analyzed for cycles yet, so an
            empty result for them means "not checked," not "cycle-free."
          </p>
          {#if cycles.length === 0}
            <p class="placeholder">No import cycles found.</p>
          {:else}
            <div class="rows">
              {#each cycles as c, i (i)}
                <div class="arow cycle">
                  {c.join(' → ')} → {c[0]}
                </div>
              {/each}
            </div>
          {/if}
        </section>
      {/if}

      {#if impact !== null}
        <section class="card">
          <div class="history-head">
            Impact of working-tree changes
            <span class="muted">({impact.changed.length} changed, {impact.dependents.length} dependent{impact.dependents.length === 1 ? '' : 's'})</span>
          </div>
          <p class="caveat">
            Approximate (name-keyed) — call edges aren't id-resolved, so this
            can both miss dynamic-dispatch callers and, more rarely, match a
            same-named symbol elsewhere. Diff vs <code>HEAD</code>; requires a
            git repository.
          </p>
          {#if impact.changed.length === 0}
            <p class="placeholder">No changes detected (working tree matches HEAD).</p>
          {:else}
            <div class="rows">
              {#each impact.changed as s (s.file + ':' + s.line)}
                <div class="arow">
                  <span class="aname">{s.name}</span>
                  <span class="akind">{s.kind}</span>
                  <span class="aloc">{s.file}:{s.line}</span>
                </div>
              {/each}
            </div>
            {#if impact.dependents.length === 0}
              <p class="placeholder">No dependents found (nothing in the index transitively calls the changed symbol(s)).</p>
            {:else}
              <div class="history-head">Dependents</div>
              <div class="rows">
                {#each impact.dependents as d, i (d.file + ':' + d.line + ':' + i)}
                  <div class="arow dep">
                    <span class="aname">{d.approx ? '~' : ''}{d.name}</span>
                    <span class="akind">{d.kind}</span>
                    <span class="aloc">{d.file}:{d.line}</span>
                    <span class="muted">depth {d.depth}</span>
                  </div>
                {/each}
              </div>
            {/if}
          {/if}
          {#if impact.unindexed.length > 0}
            <p class="caveat">
              Changed but not indexed ({impact.unindexed.length}): {impact.unindexed.join(', ')}
            </p>
          {/if}
        </section>
      {/if}
    </div>
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
  .placeholder {
    opacity: 0.6;
    font-style: italic;
    padding: 8px 2px;
  }
  /* Segmented section nav under the header. */
  nav.sections {
    display: flex;
    gap: 4px;
    margin-bottom: 14px;
    border-bottom: 1px solid var(--border, #333);
    padding-bottom: 8px;
    flex-wrap: wrap;
  }
  .seg {
    padding: 4px 12px;
    border-radius: 6px;
    border: 1px solid transparent;
    background: transparent;
    color: var(--text, #ddd);
    font-size: 12px;
    cursor: pointer;
    opacity: 0.7;
  }
  .seg:hover {
    background: rgba(255, 255, 255, 0.06);
    opacity: 1;
  }
  .seg.active {
    background: var(--accent, #3b6ea5);
    color: #fff;
    opacity: 1;
    border-color: var(--accent, #3b6ea5);
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
    border-color: #2e7d32;
  }
  .lang-legend .dot.yellow {
    border-color: #b26a00;
  }
  .lang-legend .dot.red {
    border-color: #b3261e;
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
    border: 1.5px solid var(--border, #444);
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
    border-color: #2e7d32;
    color: #b8e6bb;
  }
  .lang-btn.yellow {
    border-color: #b26a00;
    color: #f0c674;
  }
  .lang-btn.red {
    border-color: #b3261e;
    color: #ffb4ab;
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
  .hsrc.opencode {
    color: #d2a8ff;
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
  .analyses .actions {
    margin-bottom: 12px;
  }
  .caveat {
    font-size: 11px;
    opacity: 0.65;
    margin: 2px 0 8px;
    line-height: 1.4;
  }
  .rows {
    display: flex;
    flex-direction: column;
  }
  .arow {
    display: grid;
    grid-template-columns: 1fr 6rem 2fr;
    gap: 8px;
    align-items: baseline;
    padding: 3px 4px;
    border-bottom: 1px solid var(--border, #2a2a2a);
    font-size: 12px;
    white-space: nowrap;
  }
  .arow.cycle {
    display: block;
    font-family: monospace;
    font-size: 11.5px;
    white-space: normal;
    word-break: break-all;
  }
  .arow.dep {
    grid-template-columns: 1fr 6rem 2fr auto;
  }
  .aname {
    font-weight: 600;
    overflow: hidden;
    text-overflow: ellipsis;
  }
  .akind {
    opacity: 0.7;
    font-size: 11px;
  }
  .aloc {
    font-family: monospace;
    font-size: 11px;
    opacity: 0.8;
    overflow: hidden;
    text-overflow: ellipsis;
  }
  .arow.note,
  .arow.sess,
  .arow.ws {
    grid-template-columns: 1fr auto auto;
    white-space: normal;
  }
  .arow.note {
    grid-template-columns: auto 1fr auto;
  }
  .arow.sess.current .aname {
    color: var(--accent, #3b6ea5);
    font-weight: 700;
  }
  .ntext {
    font-size: 12px;
    word-break: break-word;
  }
  .pin {
    background: transparent;
    border: none;
    padding: 0 4px 0 0;
    cursor: pointer;
    font-size: 12px;
    opacity: 0.55;
  }
  .pin.pinned {
    opacity: 1;
  }
  .history-head {
    display: flex;
    align-items: center;
    gap: 8px;
  }
  .history-head .muted {
    margin-right: auto;
  }
  button.mini {
    padding: 2px 8px;
    font-size: 11px;
  }
  button.mini.danger {
    background: transparent;
    border-color: #b3261e;
    color: #ffb4ab;
  }
  button.mini.danger:hover {
    background: rgba(179, 38, 30, 0.15);
  }
  .preview-in {
    display: flex;
    gap: 8px;
    margin: 6px 0 10px;
  }
  .preview-in input {
    flex: 1;
    min-width: 0;
    padding: 5px 8px;
    border-radius: 5px;
    border: 1px solid var(--border, #444);
    background: var(--panel, #1e1e1e);
    color: var(--text, #ddd);
    font-size: 12px;
  }
  .preview-meta {
    font-size: 11px;
    opacity: 0.75;
    margin: 4px 0;
    font-variant-numeric: tabular-nums;
  }
  .preview-md {
    background: rgba(0, 0, 0, 0.25);
    border: 1px solid var(--border, #333);
    border-radius: 6px;
    padding: 8px 10px;
    font-size: 11.5px;
    line-height: 1.5;
    white-space: pre-wrap;
    word-break: break-word;
    max-height: 320px;
    overflow-y: auto;
    margin: 4px 0 0;
  }
</style>
