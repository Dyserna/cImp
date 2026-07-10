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
    graphLanguageCensus,
    graphSetLanguageEnabled,
    graphDeadExports,
    graphCycles,
    graphImpact,
    graphMemory,
    graphMemoryClear,
    graphNoteSetPinned,
    graphFacts,
    graphFactUpdate,
    graphFactAdd,
    graphContextPreview,
    graphUsage,
    graphUsageAdvice,
    advisorDismiss,
    graphPath,
    graphArchitecture,
    onGraphStatus,
    onGraphAnalyses,
    type EmbedderProbe,
    type GraphStatus,
    type LangCensus,
    type DeadExportRow,
    type ImpactResult,
    type MemorySnapshot,
    type ProjectFact,
    type RetrieveResult,
    type UsageSnapshot,
    type AdvisorSnapshot,
    type AdvisorProposal,
    type PathResult,
    type PathNodeRow,
    type ArchResult,
  } from './graph';
  import { turnTotal, maxTurnTotal, barHeightPct, cacheHitRatio, fmtTok } from './usageMath';
  import { fmtTime } from './format';
  import { listenManaged } from './listenManaged';
  import { settings, applySettings } from './settings/store';

  // The graph_* tool reference list and the recent-calls activity feed both
  // moved to the Tool Activity tab (ToolActivityView.svelte).

  // Facts (V12 Phase E): durable project facts distilled from session memory
  // (or added manually). Fetched alongside memory while the Memory section is
  // open.
  let facts = $state<ProjectFact[]>([]);
  let newFactText = $state('');
  let newFactPin = $state(false);
  let factBusy = $state(false);

  async function refreshFacts(): Promise<void> {
    try {
      facts = await graphFacts();
    } catch (e) {
      console.warn('graph_facts failed', e);
    }
  }

  async function toggleFactPin(id: string, pinned: boolean): Promise<void> {
    try {
      await graphFactUpdate(id, pinned ? 'pin' : 'unpin');
      await refreshFacts();
    } catch (e) {
      console.error('graph_fact_update (pin) failed', e);
    }
  }

  async function deleteFact(id: string): Promise<void> {
    try {
      await graphFactUpdate(id, 'delete');
      await refreshFacts();
    } catch (e) {
      console.error('graph_fact_update (delete) failed', e);
    }
  }

  async function addFact(): Promise<void> {
    const text = newFactText.trim();
    if (!text || factBusy) return;
    factBusy = true;
    try {
      await graphFactAdd(text, newFactPin);
      newFactText = '';
      newFactPin = false;
      await refreshFacts();
    } catch (e) {
      console.error('graph_fact_add failed', e);
    } finally {
      factBusy = false;
    }
  }

  // Analyses (Phase B2): on-demand dead-export + import-cycle results. Run only
  // when the user clicks — walking the graph is comparatively expensive.
  let deadExports = $state<DeadExportRow[] | null>(null);
  let cycles = $state<string[][] | null>(null);
  let impact = $state<ImpactResult | null>(null);
  let analysisBusy = $state<'dead' | 'cycles' | 'impact' | null>(null);
  let analysisError = $state<string | null>(null);

  // V12 Phase F (6c): live counts from the `graph-analyses` event (the
  // analyses-auto trigger) vs. the counts the user last actually VIEWED
  // (`analysesAck*`) — the difference badges the section tab + buttons
  // ("+N since last pass"). The first event this session seeds the ack
  // baseline too, so a project with a long-standing backlog doesn't flash a
  // huge badge the moment the tab opens — only genuine growth counts.
  let analysesLive = $state<{ dead: number; cycles: number } | null>(null);
  let analysesAckDead = $state<number | null>(null);
  let analysesAckCycles = $state<number | null>(null);
  let deadBadge = $derived(
    analysesLive && analysesAckDead !== null && analysesLive.dead > analysesAckDead
      ? analysesLive.dead - analysesAckDead
      : null,
  );
  let cyclesBadge = $derived(
    analysesLive && analysesAckCycles !== null && analysesLive.cycles > analysesAckCycles
      ? analysesLive.cycles - analysesAckCycles
      : null,
  );
  let analysesBadgeTotal = $derived((deadBadge ?? 0) + (cyclesBadge ?? 0));

  async function runDeadExports(): Promise<void> {
    analysisBusy = 'dead';
    analysisError = null;
    try {
      deadExports = await graphDeadExports();
      analysesAckDead = analysesLive?.dead ?? deadExports.length;
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
      analysesAckCycles = analysesLive?.cycles ?? cycles.length;
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

  // V15 Feature 1: path tracing — the "Trace path" section. `from`/`to` accept
  // a symbol name, `file:line`, or bare file path (resolved backend-side).
  // Edge-kind toggles default to all three kinds checked.
  let pathFrom = $state('');
  let pathTo = $state('');
  let pathSymmetric = $state(false);
  let pathKindCall = $state(true);
  let pathKindImport = $state(true);
  let pathKindContains = $state(true);
  let pathResult = $state<PathResult | null>(null);
  let pathBusy = $state(false);
  let pathError = $state<string | null>(null);

  async function runPath(): Promise<void> {
    if (!pathFrom.trim() || !pathTo.trim() || pathBusy) return;
    pathBusy = true;
    pathError = null;
    try {
      const kinds = [
        pathKindCall ? 'call' : null,
        pathKindImport ? 'import' : null,
        pathKindContains ? 'contains' : null,
      ].filter((k): k is string => k !== null);
      pathResult = await graphPath(pathFrom.trim(), pathTo.trim(), {
        kinds,
        symmetric: pathSymmetric,
      });
    } catch (e) {
      pathError = String(e);
      pathResult = null;
    } finally {
      pathBusy = false;
    }
  }

  // A file node's `label` is just its path; a symbol node shows name + loc + kind.
  function pathNodeText(n: PathNodeRow): string {
    return n.kind === 'file' ? n.file : `${n.label} (${n.file}:${n.line}) [${n.kind}]`;
  }

  // V15 Feature 2: the "Architecture" section — god nodes, subsystems,
  // surprising (cross-subsystem) edges. Heuristic, advisory only.
  let arch = $state<ArchResult | null>(null);
  let archBusy = $state(false);
  let archError = $state<string | null>(null);

  async function runArchitecture(): Promise<void> {
    archBusy = true;
    archError = null;
    try {
      arch = await graphArchitecture();
    } catch (e) {
      archError = String(e);
    } finally {
      archBusy = false;
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

  // Usage (V14 Phase D/D2): the token X-ray + the budget-tuning advisor's
  // proposals card. Fetched only while the section is open (same posture as
  // Memory) — folded into `refresh()`'s poll below.
  let usage = $state<UsageSnapshot | null>(null);
  let advice = $state<AdvisorSnapshot | null>(null);
  let advisorBusy = $state<string | null>(null); // rule_id currently applying/dismissing

  async function refreshUsage(): Promise<void> {
    // Independent fetches — run concurrently so the 2s Overview poll pays
    // one round-trip of wall time, not two. Each keeps its last good value
    // on failure, same as before.
    const [u, a] = await Promise.all([
      graphUsage().catch((e) => {
        console.warn('graph_usage failed', e);
        return null;
      }),
      graphUsageAdvice().catch((e) => {
        console.warn('graph_usage_advice failed', e);
        return null;
      }),
    ]);
    if (u) usage = u;
    if (a) advice = a;
  }

  // Applies a proposal by writing the ONE named `graph.*` field it targets
  // through the normal settings round-trip (`applySettings` — visible in
  // Settings, undoable, migration-safe). There is no bespoke "apply" IPC —
  // the advisor never mutates settings itself (milestone Feature 1b: never
  // silent self-modification).
  async function applyProposal(p: AdvisorProposal): Promise<void> {
    advisorBusy = p.rule_id;
    try {
      const next = structuredClone($settings);
      const val = Number(p.proposed);
      switch (p.setting) {
        case 'graph.context_min_score':
          next.graph.context_min_score = val;
          break;
        case 'graph.read_advisor_min_lines':
          next.graph.read_advisor_min_lines = val;
          break;
        case 'graph.context_turn_budget_chars':
          next.graph.context_turn_budget_chars = val;
          break;
        default:
          console.warn('advisor: unrecognized proposal setting', p.setting);
          return;
      }
      await applySettings(next);
      // The applied change itself will shift the underlying rate, so drop
      // this proposal locally rather than waiting for the next poll.
      advice = advice && { ...advice, proposals: advice.proposals.filter((x) => x.rule_id !== p.rule_id) };
    } finally {
      advisorBusy = null;
    }
  }

  async function dismissProposal(p: AdvisorProposal): Promise<void> {
    advisorBusy = p.rule_id;
    try {
      await advisorDismiss(p.rule_id, p.signature);
      advice = advice && { ...advice, proposals: advice.proposals.filter((x) => x.rule_id !== p.rule_id) };
    } catch (e) {
      console.error('advisor_dismiss failed', e);
    } finally {
      advisorBusy = null;
    }
  }

  // Stacked-bar chart derived state (This Session). Pure math lives in
  // `./usageMath` (unit-tested); this just wires it to the current turns.
  let usageTurns = $derived(usage?.current?.turns ?? []);
  let usageMax = $derived(maxTurnTotal(usageTurns));

  const ADVISOR_RULES_TOOLTIP =
    'advisor.raise_context_min_score.v1: ≥5 sessions, ≥200 injections, ≥70% never re-touched → raise context_min_score.\n' +
    'advisor.raise_read_advisor_min_lines.v1: ≥5 sessions, ≥20 reminders, ≥50% re-read anyway → raise read_advisor_min_lines.\n' +
    'advisor.lower_context_turn_budget_chars.v1: ≥5 sessions, ≥200 injections, ≥50 turns, ≥70% unread AND ≥50% turns maxed → lower context_turn_budget_chars.';

  // The Overview section stacks the status groups (Usage, then Index) as one
  // at-a-glance dashboard. The activity feed + graph-tools reference moved to
  // the Tool Activity tab; Memory/Context/Analyses/Usage content is filled by
  // V10/V14 phases. The internal tab id (`graph-monitor`) is unchanged — this
  // is purely the in-view section router.
  type Section = 'overview' | 'memory' | 'context' | 'analyses' | 'path' | 'architecture';
  const SECTIONS: { id: Section; label: string }[] = [
    { id: 'overview', label: 'Overview' },
    { id: 'memory', label: 'Memory' },
    { id: 'context', label: 'Context' },
    { id: 'analyses', label: 'Analyses' },
    { id: 'path', label: 'Trace path' },
    { id: 'architecture', label: 'Architecture' },
  ];
  let section = $state<Section>('overview');

  let roots = $state<GraphStatus[]>([]);
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
    // Memory is only fetched while its section is visible (opens the warm index).
    if (section === 'memory') {
      await refreshMemory();
      await refreshFacts();
    }
    // Usage (V14 Phase D/D2): same "only while visible" posture — the Usage
    // cards now render inside the Overview section.
    if (section === 'overview') {
      await refreshUsage();
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
  // V12 Phase F (6c): analyses-auto trigger badges (see the state block above).
  listenManaged(() =>
    onGraphAnalyses((a) => {
      if (analysesAckDead === null) analysesAckDead = a.dead_exports;
      if (analysesAckCycles === null) analysesAckCycles = a.import_cycles;
      analysesLive = { dead: a.dead_exports, cycles: a.import_cycles };
    }),
  );

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
          if (s.id === 'memory') {
            refreshMemory();
            refreshFacts();
          }
          if (s.id === 'overview') {
            refreshUsage();
          }
        }}
      >{s.label}{#if s.id === 'analyses' && analysesBadgeTotal > 0}<span class="badge" title="New since last pass">+{analysesBadgeTotal}</span>{/if}</button>
    {/each}
  </nav>

  {#if section === 'overview'}
  <h3 class="group-head">Usage</h3>
  <div class="usage-sec">
    <!-- V14 Phase D2: Advisor card, always first. -->
    <section class="card advisor">
      <div class="history-head">
        Budget-tuning advisor
        <span class="muted" title={ADVISOR_RULES_TOOLTIP}>ⓘ rules</span>
      </div>
      {#if !advice}
        <p class="placeholder">Loading…</p>
      {:else if advice.collecting}
        <p class="placeholder">
          Collecting data — the advisor needs at least 5 sessions and enough injections/reminders
          before it proposes anything.
        </p>
      {:else if advice.proposals.length === 0}
        <p class="placeholder">No changes suggested — data looks healthy.</p>
      {:else}
        <div class="rows">
          {#each advice.proposals as p (p.rule_id)}
            <div class="proposal">
              <div class="prop-head">
                <span class="aname">{p.setting}</span>
                <span class="prop-vals"><code>{p.current}</code> → <code>{p.proposed}</code></span>
              </div>
              <p class="prop-rationale">{p.rationale}</p>
              <div class="prop-actions">
                <button
                  class="mini"
                  disabled={advisorBusy !== null}
                  onclick={() => applyProposal(p)}
                >{advisorBusy === p.rule_id ? 'Applying…' : 'Apply'}</button>
                <button
                  class="mini secondary"
                  disabled={advisorBusy !== null}
                  onclick={() => dismissProposal(p)}
                >Dismiss</button>
              </div>
            </div>
          {/each}
        </div>
      {/if}
    </section>

    <!-- This session: per-turn stacked bars + top consumers. -->
    <section class="card">
      <div class="history-head">This session</div>
      {#if !usage || !usage.current || usage.current.turns.length === 0}
        <p class="placeholder">No usage recorded yet this session.</p>
      {:else}
        <div class="ubars-legend">
          <span><span class="dot in"></span>input</span>
          <span><span class="dot cache"></span>cache-read</span>
          <span><span class="dot out"></span>output</span>
          <span><span class="dot tool"></span>est. tool-result</span>
        </div>
        <div class="ubars">
          {#each usageTurns as t, i (i)}
            {@const total = turnTotal(t)}
            {@const est_tool = Math.round(t.tool_chars / 4)}
            <div class="ubar-col">
              <div
                class="ubar"
                style="height: {barHeightPct(total, usageMax)}%"
                title="turn {i + 1}: {t.in_tok} in / {t.cache_read} cache-read / {t.out_tok} out / ~{est_tool} est. tool"
              >
                {#if total > 0}
                  <span class="useg in" style="flex-grow: {t.in_tok}"></span>
                  <span class="useg cache" style="flex-grow: {t.cache_read}"></span>
                  <span class="useg out" style="flex-grow: {t.out_tok}"></span>
                  <span class="useg tool" style="flex-grow: {est_tool}"></span>
                {/if}
              </div>
            </div>
          {/each}
        </div>

        <div class="history-head">Top consumers</div>
        {#if usage.current.top_tools.length === 0}
          <p class="placeholder">No tool-result usage recorded yet.</p>
        {:else}
          <div class="rows">
            {#each usage.current.top_tools as t (t.tool)}
              <div class="arow tool">
                <span class="aname">{t.tool}</span>
                <span class="akind">~{t.est_tokens.toLocaleString()} tok <span class="est-badge">est</span></span>
                <span class="aloc">{t.calls} call{t.calls === 1 ? '' : 's'}</span>
              </div>
            {/each}
          </div>
        {/if}
      {/if}
    </section>

    <!-- Sessions: project-wide totals table. -->
    <section class="card">
      <div class="history-head">Sessions <span class="muted">({usage?.sessions.length ?? 0})</span></div>
      {#if !usage || usage.sessions.length === 0}
        <p class="placeholder">No sessions recorded yet.</p>
      {:else}
        <div class="rows">
          {#each usage.sessions as s (s.session_id)}
            <div
              class="arow sessrow"
              title={`${s.totals.in_tok.toLocaleString()} input · ${s.totals.out_tok.toLocaleString()} output · ${s.totals.cache_read.toLocaleString()} cache-read · ${s.totals.cache_make.toLocaleString()} cache-write tokens`}
            >
              <span class="aname">{s.agent}{#if s.est_only}<span class="est-badge" title="No exact usage data for this agent — chars-only estimate">est</span>{/if}</span>
              <span class="sess-stats tnum">
                <span><b>{fmtTok(s.totals.in_tok)}</b> in</span>
                <span><b>{fmtTok(s.totals.out_tok)}</b> out</span>
                <span><b>{fmtTok(s.totals.cache_read)}</b> cache-read</span>
                <span><b>{fmtTok(s.totals.cache_make)}</b> cache-write</span>
              </span>
              <span class="aloc">cache-hit {Math.round(cacheHitRatio(s.totals.cache_read, s.totals.in_tok) * 100)}%</span>
            </div>
          {/each}
        </div>
      {/if}
    </section>

    <!-- Effectiveness: measured counters, never fabricated savings. -->
    <section class="card">
      <div class="history-head">Effectiveness</div>
      <p class="caveat">
        Measured characters, not fabricated savings — every token figure below is
        the same honest <code>chars / 4</code> estimate used everywhere else in this tab.
      </p>
      {#if usage}
        <div class="eff-counters">
          <div>
            <span class="num">{usage.effectiveness.injected_chars.toLocaleString()}</span>
            <span class="lbl">chars injected <span class="est-badge">est. ~{Math.round(usage.effectiveness.injected_chars / 4).toLocaleString()} tok</span></span>
          </div>
          <div>
            <span class="num">{usage.effectiveness.deduped_chars.toLocaleString()}</span>
            <span class="lbl">chars suppressed by dedup <span class="est-badge">est. ~{Math.round(usage.effectiveness.deduped_chars / 4).toLocaleString()} tok</span></span>
          </div>
          <div>
            <span class="num">{usage.effectiveness.advisor_displaced_chars.toLocaleString()}</span>
            <span class="lbl">chars displaced by read-advisor <span class="est-badge">est. ~{Math.round(usage.effectiveness.advisor_displaced_chars / 4).toLocaleString()} tok</span></span>
          </div>
          <div>
            <span class="num">{usage.offload_local_tasks.toLocaleString()}</span>
            <span class="lbl">tasks served locally — see the <em>Offload Server</em> tab</span>
          </div>
        </div>
      {/if}
    </section>
  </div>

  <h3 class="group-head">Index</h3>
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

  {:else if section === 'memory'}
    <section class="card">
      <div class="history-head">Facts <span class="muted">({facts.length})</span></div>
      <p class="caveat">
        Durable project facts — distilled from idle sessions (when Memory
        distillation is on) or added here. Pinned facts survive indefinitely and,
        with Fact promotion on, ride along in the launch-time guidance for every
        new tab.
      </p>
      <div class="preview-in">
        <input
          type="text"
          placeholder="Add a fact this project should remember…"
          bind:value={newFactText}
          onkeydown={(e) => e.key === 'Enter' && addFact()}
        />
        <label class="pin-toggle" title="Pin (keep indefinitely, eligible for promotion)">
          <input type="checkbox" bind:checked={newFactPin} />
          📌
        </label>
        <button onclick={addFact} disabled={factBusy || !newFactText.trim()}>
          {factBusy ? 'Adding…' : 'Add'}
        </button>
      </div>
      {#if facts.length === 0}
        <p class="placeholder">No facts yet.</p>
      {:else}
        <div class="rows">
          {#each facts as f (f.fact_id)}
            <div class="arow fact">
              <button
                class="pin"
                class:pinned={f.pinned}
                title={f.pinned ? 'Unpin' : 'Pin (keep indefinitely, eligible for promotion)'}
                onclick={() => toggleFactPin(f.fact_id, !f.pinned)}
              >{f.pinned ? '📌' : '📍'}</button>
              <span class="ntext" title={`source: ${f.source_session}`}>{f.text}</span>
              <span class="aloc">{fmtTime(f.ts_ms)}</span>
              <button class="mini danger fact-del" onclick={() => deleteFact(f.fact_id)}>✕</button>
            </div>
          {/each}
        </div>
      {/if}
    </section>

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
          {analysisBusy === 'dead' ? 'Scanning…' : 'Find dead exports'}{#if deadBadge}<span class="badge" title="New since last pass">+{deadBadge}</span>{/if}
        </button>
        <button onclick={runCycles} disabled={analysisBusy !== null}>
          {analysisBusy === 'cycles' ? 'Scanning…' : 'Find import cycles'}{#if cyclesBadge}<span class="badge" title="New since last pass">+{cyclesBadge}</span>{/if}
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
              <p class="caveat">
                Confidence along the discovery chain:
                <span class="conf extracted">extracted</span> (most certain) →
                <span class="conf inferred">inferred</span> →
                <span class="conf ambiguous">ambiguous</span> (least certain).
              </p>
              <div class="rows">
                {#each impact.dependents as d, i (d.file + ':' + d.line + ':' + i)}
                  <div class="arow dep">
                    <span class="aname">{d.approx ? '~' : ''}{d.name}</span>
                    <span class="akind">{d.kind}</span>
                    <span class="aloc">{d.file}:{d.line}</span>
                    <span class="muted">depth {d.depth}</span>
                    <span class="conf {d.confidence}" title="edge confidence: {d.confidence}">{d.confidence}</span>
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
  {:else if section === 'path'}
    <div class="path-sec">
      <p class="caveat">
        Traces the shortest path between two entities over the code graph's
        call/import/contains edges. Heuristic — a missing edge (dynamic
        dispatch, an unindexed language) can hide a real path.
      </p>
      <section class="card">
        <div class="history-head">Trace path</div>
        <div class="preview-in path-in">
          <input
            type="text"
            placeholder="symbol name, file:line, or file path"
            bind:value={pathFrom}
            onkeydown={(e) => e.key === 'Enter' && runPath()}
          />
          <span class="path-sep">→</span>
          <input
            type="text"
            placeholder="symbol name, file:line, or file path"
            bind:value={pathTo}
            onkeydown={(e) => e.key === 'Enter' && runPath()}
          />
          <button onclick={runPath} disabled={pathBusy || !pathFrom.trim() || !pathTo.trim()}>
            {pathBusy ? 'Tracing…' : 'Trace'}
          </button>
        </div>
        <div class="path-opts">
          <label class="pin-toggle">
            <input type="checkbox" bind:checked={pathSymmetric} /> Undirected (related at all?)
          </label>
          <label class="pin-toggle"><input type="checkbox" bind:checked={pathKindCall} /> call</label>
          <label class="pin-toggle"><input type="checkbox" bind:checked={pathKindImport} /> import</label>
          <label class="pin-toggle"><input type="checkbox" bind:checked={pathKindContains} /> contains</label>
        </div>

        {#if pathError}
          <p class="error">{pathError}</p>
        {/if}

        {#if pathResult}
          {#if !pathResult.found}
            <p class="placeholder">No path found within the hop limit (or an endpoint isn't indexed).</p>
          {:else}
            <div class="path-chain">
              {#each pathResult.nodes as n, i (n.id + ':' + i)}
                <div class="path-node" title={n.file}>{pathNodeText(n)}</div>
                {#if n.edge_to_next}
                  <div class="path-edge">
                    ──{n.edge_to_next}{#if n.confidence}<span class="conf {n.confidence}" title="edge confidence: {n.confidence}">{n.confidence}</span>{/if}──▶
                  </div>
                {/if}
              {/each}
            </div>
            <p class="preview-meta">
              {pathResult.hops} hop{pathResult.hops === 1 ? '' : 's'}{#if pathResult.equal_alternatives > 0}
                &nbsp;(+{pathResult.equal_alternatives} other path{pathResult.equal_alternatives === 1 ? '' : 's'} of equal length)
              {/if}
            </p>
          {/if}
        {/if}
      </section>
    </div>
  {:else if section === 'architecture'}
    <div class="arch-sec">
      <p class="caveat">
        Heuristic system-shape overview — hub degree + label-propagation
        clustering. Advisory, not authoritative; verify before acting on it.
      </p>
      <div class="actions">
        <button onclick={runArchitecture} disabled={archBusy}>
          {archBusy ? 'Analyzing…' : 'Recompute'}
        </button>
      </div>

      {#if archError}
        <p class="error">{archError}</p>
      {/if}

      {#if arch}
        <section class="card">
          <div class="history-head">God nodes <span class="muted">({arch.god_nodes.length})</span></div>
          <p class="caveat">Hubs the system flows through.</p>
          {#if arch.god_nodes.length === 0}
            <p class="placeholder">No standout hubs found.</p>
          {:else}
            <div class="rows">
              {#each arch.god_nodes as g (g.id)}
                <div class="arow god">
                  <span class="aname">{g.label}</span>
                  <span class="akind">{g.kind}</span>
                  <span class="aloc">{g.file}</span>
                  <span class="muted">degree {g.degree}</span>
                </div>
              {/each}
            </div>
          {/if}
        </section>

        <section class="card">
          <div class="history-head">Subsystems <span class="muted">({arch.subsystems.length})</span></div>
          {#if arch.subsystems.length === 0}
            <p class="placeholder">Single cohesive module — no distinct subsystems detected.</p>
          {:else}
            <div class="subsys-list">
              {#each arch.subsystems as s (s.name)}
                <details class="subsys">
                  <summary>{s.name} — {s.size} file{s.size === 1 ? '' : 's'} · hub {s.hub}</summary>
                  <div class="subsys-files">
                    {#each s.files as f (f)}
                      <div class="aloc">{f}</div>
                    {/each}
                  </div>
                </details>
              {/each}
            </div>
          {/if}
        </section>

        <section class="card">
          <div class="history-head">
            Surprising connections <span class="muted">({arch.surprising.length})</span>
          </div>
          <p class="caveat">
            Candidate accidental coupling — heuristic, verify before acting.
          </p>
          {#if arch.surprising.length === 0}
            <p class="placeholder">No cross-subsystem surprises found.</p>
          {:else}
            <div class="rows">
              {#each arch.surprising as s, i (s.from + ':' + s.to + ':' + i)}
                <div class="arow surprising">
                  <span class="aname">{s.from_subsystem} ✗ {s.to_subsystem}</span>
                  <span class="aloc">{s.from} ──{s.kind}──▶ {s.to}</span>
                </div>
              {/each}
            </div>
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
  /* V12 Phase F (6c): "+N since last pass" badge on the Analyses tab + its
     buttons — a small pill, never wraps, doesn't disturb button sizing. */
  .badge {
    display: inline-block;
    margin-left: 6px;
    padding: 0 6px;
    border-radius: 999px;
    background: var(--warn, #c9820a);
    color: #fff;
    font-size: 10px;
    font-weight: 600;
    line-height: 16px;
    vertical-align: middle;
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
  /* Group divider inside the Overview section (Usage / Index). */
  .group-head {
    margin: 18px 0 8px;
    font-size: 11px;
    font-weight: 600;
    text-transform: uppercase;
    letter-spacing: 0.06em;
    opacity: 0.7;
    border-bottom: 1px solid var(--border, #333);
    padding-bottom: 4px;
  }
  .group-head:first-of-type {
    margin-top: 0;
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
  .history-head {
    font-weight: 600;
    margin-bottom: 6px;
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
  .arow.fact {
    grid-template-columns: auto 1fr auto auto;
  }
  .fact-del {
    opacity: 0.6;
  }
  .fact-del:hover {
    opacity: 1;
  }
  .pin-toggle {
    display: flex;
    align-items: center;
    gap: 3px;
    font-size: 12px;
    cursor: pointer;
    user-select: none;
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

  /* ── V14 Phase D/D2: Usage section + Advisor card ─────────────────────── */

  .est-badge {
    display: inline-block;
    margin-left: 4px;
    padding: 0 5px;
    border-radius: 8px;
    background: rgba(255, 255, 255, 0.1);
    color: var(--text, #ddd);
    font-size: 9.5px;
    font-weight: 600;
    text-transform: uppercase;
    letter-spacing: 0.03em;
    vertical-align: middle;
    opacity: 0.85;
  }

  /* Advisor card. */
  .advisor .history-head .muted {
    margin-left: auto;
    cursor: help;
    font-size: 10.5px;
  }
  .proposal {
    padding: 8px 4px;
    border-bottom: 1px solid var(--border, #2a2a2a);
  }
  .proposal:last-child {
    border-bottom: none;
  }
  .prop-head {
    display: flex;
    align-items: baseline;
    justify-content: space-between;
    gap: 8px;
    flex-wrap: wrap;
  }
  .prop-head .aname {
    font-family: monospace;
    font-size: 12px;
  }
  .prop-vals {
    font-size: 12px;
    font-variant-numeric: tabular-nums;
  }
  .prop-vals code {
    background: rgba(255, 255, 255, 0.08);
    padding: 1px 5px;
    border-radius: 4px;
  }
  .prop-rationale {
    margin: 4px 0 6px;
    font-size: 11.5px;
    opacity: 0.8;
    line-height: 1.4;
  }
  .prop-actions {
    display: flex;
    gap: 8px;
  }

  /* This-session stacked-bar chart: pure CSS/flex, no chart dependency.
     Each turn is a column whose overall height is normalized to the tallest
     turn (`barHeightPct`); within a bar, the four honest-accounting segments
     stack via flex-grow proportional to their token share. */
  .ubars-legend {
    display: flex;
    flex-wrap: wrap;
    gap: 4px 14px;
    margin: 2px 0 10px;
    font-size: 10.5px;
    opacity: 0.75;
  }
  .ubars-legend span {
    display: inline-flex;
    align-items: center;
    gap: 5px;
  }
  .ubars-legend .dot {
    width: 8px;
    height: 8px;
    border-radius: 2px;
    display: inline-block;
  }
  .dot.in,
  .useg.in {
    background: #58a6ff;
  }
  .dot.cache,
  .useg.cache {
    background: #d2a8ff;
  }
  .dot.out,
  .useg.out {
    background: #3fb950;
  }
  .dot.tool,
  .useg.tool {
    background: #f0c674;
  }
  .ubars {
    display: flex;
    align-items: flex-end;
    gap: 3px;
    height: 130px;
    margin-bottom: 14px;
    border-bottom: 1px solid var(--border, #333);
    padding-bottom: 2px;
  }
  .ubar-col {
    flex: 1 1 0;
    min-width: 2px;
    display: flex;
    align-items: flex-end;
    height: 100%;
  }
  .ubar {
    width: 100%;
    display: flex;
    flex-direction: column-reverse;
    min-height: 1px;
  }
  .useg {
    flex-basis: 0;
    min-height: 0;
  }

  .arow.tool {
    grid-template-columns: 1fr 1fr auto;
  }
  /* agent | the four billing stats | cache-hit % */
  .arow.sessrow {
    grid-template-columns: minmax(6rem, 1fr) auto auto;
  }
  .sess-stats {
    display: flex;
    gap: 12px;
    font-size: 11px;
    opacity: 0.85;
  }
  .sess-stats b {
    font-weight: 600;
  }

  .eff-counters {
    display: grid;
    grid-template-columns: repeat(auto-fill, minmax(11rem, 1fr));
    gap: 12px;
    margin-top: 6px;
  }
  .eff-counters .num {
    font-size: 18px;
    font-weight: 600;
    display: block;
    font-variant-numeric: tabular-nums;
  }
  .eff-counters .lbl {
    font-size: 11px;
    opacity: 0.7;
    line-height: 1.4;
  }

  /* ── V15: confidence badges (impact dependents, path edges) ──────────── */
  .conf {
    display: inline-block;
    margin-left: 4px;
    padding: 0 6px;
    border-radius: 8px;
    font-size: 9.5px;
    font-weight: 600;
    text-transform: uppercase;
    letter-spacing: 0.03em;
    line-height: 15px;
    vertical-align: middle;
    white-space: nowrap;
  }
  .conf.extracted {
    background: rgba(255, 255, 255, 0.08);
    color: var(--text, #ddd);
    opacity: 0.75;
  }
  .conf.inferred {
    background: rgba(178, 106, 0, 0.28);
    color: #f0c674;
  }
  .conf.ambiguous {
    background: rgba(179, 38, 30, 0.28);
    color: #ffb4ab;
  }
  .arow.dep {
    grid-template-columns: 1fr 6rem 2fr auto auto;
  }
  .arow.god {
    grid-template-columns: 1fr 6rem 2fr auto;
  }
  .arow.surprising {
    grid-template-columns: 1fr 2fr;
    white-space: normal;
  }

  /* ── V15 Feature 1: Trace path ─────────────────────────────────────────── */
  .path-in {
    align-items: center;
  }
  .path-sep {
    opacity: 0.6;
    flex: 0 0 auto;
  }
  .path-opts {
    display: flex;
    flex-wrap: wrap;
    gap: 4px 16px;
    margin: 2px 0 10px;
    font-size: 12px;
  }
  .path-chain {
    display: flex;
    flex-direction: column;
    font-size: 12px;
    margin: 6px 0;
  }
  .path-node {
    font-family: monospace;
    padding: 2px 0;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .path-edge {
    padding: 1px 0 1px 1.2em;
    opacity: 0.75;
    font-size: 11px;
    font-family: monospace;
  }

  /* ── V15 Feature 2: Architecture ───────────────────────────────────────── */
  .subsys-list {
    display: flex;
    flex-direction: column;
    gap: 4px;
  }
  .subsys {
    border-bottom: 1px solid var(--border, #2a2a2a);
    padding: 4px 2px;
    font-size: 12px;
  }
  .subsys summary {
    cursor: pointer;
    font-weight: 600;
  }
  .subsys-files {
    margin: 6px 0 4px 1.2em;
    display: flex;
    flex-direction: column;
    gap: 2px;
  }
</style>
