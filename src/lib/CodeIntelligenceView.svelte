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
    advisorMarkApplied,
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
    type SessionUsageRow,
  } from './graph';
  import {
    turnTotal,
    maxTurnTotal,
    barHeightPct,
    cacheHitRatio,
    fmtTok,
    sessionCost,
    fmtUsd,
    matchPricing,
    turnCost,
    type PriceRates,
  } from './usageMath';
  import { fmtDate, fmtTime } from './format';
  import { listenManaged } from './listenManaged';
  import { settings, applySettings } from './settings/store';
  import { llmPricingGet, openSettingsWindowToSection } from './settings/ipc';
  import type { LlmPricingModel, ChecksSuggestion } from './settings/types';
  import { checksSuggestion, checksDismissSuggestion } from './checks';
  import { computeChip } from './settings/checksEditor';
  import { workbenchSessionCommitCounts, openSessionCommits } from './workbench';
  import { revealTab } from './tabs/visibility';
  import { GRAPH_MONITOR_TAB_ID, WORKBENCH_TAB_ID } from './tabs/types';
  import { isAppViewVisible, onAppViewShown } from './appViewVisibility';
  import {
    loadCardOpen,
    loadViewSection,
    loadViewString,
    saveCardOpen,
    saveViewSection,
    saveViewString,
  } from './viewSection';
  import { harnessMarkVerified } from './graph';

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
  // Rendered in the Advisor card when an Apply can't be honored (e.g. a
  // proposal names a setting this build has no case for) — cleared on the
  // next successful apply.
  let advisorError = $state<string | null>(null);

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
    // V16 Feature 8: the cost view's auto-match price table. Fetched once
    // per view lifetime here (not per poll — prices change rarely); the
    // cost POPUP still refetches on every open, so Settings edits apply
    // there immediately either way.
    if (costMode === 'cost' && pricingTable === null) void refreshPricingTable();
    // Unawaited on purpose: commitCounts is independent $state that renders
    // when it lands — the poll's critical path shouldn't wait on a git
    // subprocess round trip.
    if ($settings.workbench.enabled) void refreshCommitCounts();
  }

  // Per-session commit counts (session_id → count) for the Sessions card's
  // "commits" button — zero (or unknown) disables it. The backend caches the
  // underlying git walk per root (WorkbenchService::COMMIT_TIMES_TTL); this
  // 10s throttle just keeps the 2s Overview poll from paying IPC churn for
  // answers that can't have changed yet.
  let commitCounts = $state<Record<string, number>>({});
  let commitCountsAt = 0;

  async function refreshCommitCounts(): Promise<void> {
    if (!usage || usage.sessions.length === 0) return;
    const now = Date.now();
    if (now - commitCountsAt < 10_000) return;
    commitCountsAt = now;
    try {
      commitCounts = await workbenchSessionCommitCounts(
        usage.sessions.map((s) => ({
          session_id: s.session_id,
          from_ms: s.started_ms,
          to_ms: s.last_ms,
        })),
      );
    } catch (e) {
      console.warn('workbench_session_commit_counts failed', e);
    }
  }

  // Jump to the Workbench tab's Session-commits section scoped to `s` — the
  // same store-bus + revealTab pattern as DiffView's jump-to-graph button.
  function openCommits(s: SessionUsageRow): void {
    openSessionCommits(s.session_id, s.agent, s.started_ms, s.last_ms);
    revealTab(WORKBENCH_TAB_ID);
  }

  // Session-cost popup: clicking a row in the Sessions card opens a modal
  // that prices that session's token totals against a provider/model entry
  // from the global LLM price table (Settings → LLM pricing), or against
  // hand-typed custom rates. The table is fetched fresh on every open so
  // edits made in the Settings window apply without reopening the tab.
  let costSession = $state<SessionUsageRow | null>(null);
  let costPricing = $state<LlmPricingModel[]>([]);
  // Index into `costPricing`; `costPricing.length` is the sentinel for the
  // trailing "Custom" option. Remembered across opens within this tab.
  let costSelIdx = $state(0);
  let costCustom = $state<PriceRates>({ input: 0, cache_write: 0, cache_read: 0, output: 0 });

  async function openCostPopup(s: SessionUsageRow): Promise<void> {
    costSession = s;
    try {
      costPricing = await llmPricingGet();
    } catch (e) {
      console.warn('llm_pricing_get failed', e);
      costPricing = [];
    }
    // A shrunken table can strand the remembered index past the Custom
    // sentinel — clamp back to the first provider (or Custom when empty).
    if (costSelIdx > costPricing.length) costSelIdx = 0;
  }
  function closeCostPopup(): void {
    costSession = null;
  }
  function onCostKeydown(e: KeyboardEvent): void {
    if (e.key === 'Escape' && costSession) {
      e.stopPropagation();
      closeCostPopup();
    }
  }
  const costRates = $derived<PriceRates>(
    costSelIdx < costPricing.length ? costPricing[costSelIdx] : costCustom,
  );
  const costRows = $derived(costSession ? sessionCost(costSession.totals, costRates) : null);
  // Which model ran the session: the single model when there's one, or
  // "mixed (<top model>)" when several — `models` arrives ranked by tokens
  // desc, so [0] is the top consumer. Empty when no turn carried a model.
  const costModel = $derived(
    !costSession || costSession.models.length === 0
      ? ''
      : costSession.models.length === 1
        ? costSession.models[0]
        : `mixed (${costSession.models[0]})`,
  );

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
        // V16 drift rules (drift.read_reason.v1 / drift.read_bypass.v1):
        // a boolean the advisor can propose — a disable. V17 Phase F
        // (adopt.read_advisor.v1) reuses the same case to propose ENABLING it,
        // so parse the proposed bool rather than assuming a disable.
        case 'graph.read_advisor':
          next.graph.read_advisor = p.proposed === 'true';
          break;
        // V17 Phase E (surface.lean.v1): hide the cold-tail graph tools.
        case 'graph.lean_tools':
          next.graph.lean_tools = p.proposed === 'true';
          break;
        // V17 Phase F (adopt.read_advisor_substitute.v1): the first
        // string-valued proposal — write the proposed mode string as-is
        // (e.g. "advise" → "substitute"), not the numeric `val` above.
        case 'graph.read_advisor_mode':
          next.graph.read_advisor_mode = p.proposed;
          break;
        default:
          // A rule proposing a setting this switch doesn't know is a bug
          // (a new advisor rule shipped without its Apply case) — surface
          // it in the card instead of a silent console-only no-op behind a
          // working-looking Apply button.
          advisorError = `Can't apply "${p.setting}" — this build has no Apply handler for it (report this).`;
          console.warn('advisor: unrecognized proposal setting', p.setting);
          return;
      }
      await applySettings(next);
      advisorError = null;
      // Start the rule's Apply cooldown: it stays quiet for a few sessions
      // so fresh post-change data can accumulate before it re-evaluates —
      // the underlying rates are cumulative, and without this the rule
      // would re-propose on the very next poll off data collected almost
      // entirely under the OLD value. Best-effort: a failure here just
      // means the old always-re-propose behavior for this one apply.
      await advisorMarkApplied(p.rule_id).catch((e) => console.warn('advisor_mark_applied failed', e));
      // Drop the proposal locally rather than waiting for the next poll.
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
  // The chart shows the newest turns that FIT the card's width (each bar
  // needs ~5px: 2px min column + 3px gap) — an unbounded session used to
  // push the flex row right out of the card on narrow panes.
  let usageTurns = $derived(usage?.current?.turns ?? []);
  let ubarsWidth = $state(0);
  const BAR_MIN_PX = 5;
  let shownTurns = $derived.by(() => {
    const cap = ubarsWidth > 0 ? Math.max(10, Math.floor(ubarsWidth / BAR_MIN_PX)) : 60;
    return usageTurns.length > cap ? usageTurns.slice(-cap) : usageTurns;
  });
  let usageMax = $derived(maxTurnTotal(shownTurns));

  // Chart segment colors: each legend dot is a native color input. The
  // committed value lives in settings (`graph.usage_color_*`, persisted by
  // the backend); `chartPreview` holds the live value while a picker is open
  // (`oninput` fires per drag tick) so the chart recolors immediately without
  // a settings round-trip per tick.
  const CHART_SEGS = [
    { key: 'in', label: 'input', field: 'usage_color_in' },
    { key: 'cache', label: 'cache-read', field: 'usage_color_cache' },
    { key: 'write', label: 'cache-write', field: 'usage_color_write' },
    { key: 'out', label: 'output', field: 'usage_color_out' },
    { key: 'tool', label: 'est. tool-result', field: 'usage_color_tool' },
  ] as const;
  type ChartSegKey = (typeof CHART_SEGS)[number]['key'];
  let chartPreview = $state<Partial<Record<ChartSegKey, string>>>({});
  let chartColors = $derived({
    in: chartPreview.in ?? $settings.graph.usage_color_in,
    cache: chartPreview.cache ?? $settings.graph.usage_color_cache,
    write: chartPreview.write ?? $settings.graph.usage_color_write,
    out: chartPreview.out ?? $settings.graph.usage_color_out,
    tool: chartPreview.tool ?? $settings.graph.usage_color_tool,
  });
  async function commitChartColor(
    seg: (typeof CHART_SEGS)[number],
    value: string,
  ): Promise<void> {
    const next = structuredClone($settings);
    next.graph[seg.field] = value;
    // applySettings updates the store optimistically (and rolls back on
    // failure), so the preview can be dropped afterwards either way.
    await applySettings(next);
    chartPreview[seg.key] = undefined;
  }

  const ADVISOR_RULES_TOOLTIP =
    'advisor.raise_context_min_score.v1: ≥5 sessions, ≥200 injections, ≥70% never re-touched → raise context_min_score.\n' +
    'advisor.raise_read_advisor_min_lines.v1: ≥5 sessions, ≥20 reminders, ≥50% re-read anyway → raise read_advisor_min_lines.\n' +
    'advisor.lower_context_turn_budget_chars.v1: ≥5 sessions, ≥200 injections, ≥50 turns, ≥70% unread AND ≥50% turns maxed → lower context_turn_budget_chars.\n' +
    'drift.harness_version.v1: Claude Code version ≠ last-verified → re-verify the hook contracts (Mark verified).\n' +
    'drift.read_reason.v1: ≥15 reminders, ≥90% immediately re-read → the deny reason isn’t reaching the model; disable read_advisor.\n' +
    'drift.read_hook_silent.v1: ≥3 sessions, ≥10 large re-reads (est.), 0 reminders → the PreToolUse hook isn’t firing.\n' +
    'drift.injection_unseen.v1: ≥5 sessions, ≥30 injections, ≤2% follow → injected context likely never reaches the model.\n' +
    'drift.usage_fields_gone.v1: ≥2 Claude sessions, all without token fields → the transcript usage schema changed.\n' +
    'drift.payload.v1: any shim-reported payload missing required fields.\n' +
    'drift.read_bypass.v1: ≥10 reminders, ≥40% answered via shell reads (est.) → disable read_advisor.\n' +
    'surface.lean.v1: ≥10 sessions, 0 calls to any cold-tail graph tool (cycles, dead_exports, struct_search, path, architecture) → enable lean_tools (hide them from the advertised surface; they still answer if called).\n' +
    'adopt.read_advisor.v1: ≥5 sessions, E1 verified (pass), ≥3 redundant large re-reads per session across ≥10 sessions (est.; external tools may have changed the file between reads) → enable read_advisor.\n' +
    'adopt.read_advisor_substitute.v1: read_advisor on in advise mode, ≥20 reminders, ≤20% re-read anyway, low shell bypass → switch read_advisor_mode to substitute.\n' +
    'After an Apply, that rule stays quiet for 3 further sessions so fresh post-change data can accumulate before it re-evaluates.';

  // ── V16 Feature 8: tokens | est. cost toggle ─────────────────────────
  // Cost mode multiplies each bar segment by its $/MTok price before
  // stacking — same segments, same colors, different heights. Prices come
  // from the global LLM price table, auto-matched by the longest
  // `model_prefix` against each turn's / session's model id. No match ⇒
  // token bars with a hint (never a made-up cost). Per-user choice,
  // persisted like the section selection.
  let costMode = $state<'tokens' | 'cost'>(
    loadViewString('code-intelligence', 'usage-cost-mode') === 'cost' ? 'cost' : 'tokens',
  );
  $effect(() => saveViewString('code-intelligence', 'usage-cost-mode', costMode));
  // `null` = not fetched yet (fetch on first need); `[]` = fetched, empty.
  let pricingTable = $state<LlmPricingModel[] | null>(null);
  async function refreshPricingTable(): Promise<void> {
    try {
      pricingTable = await llmPricingGet();
    } catch (e) {
      console.warn('llm_pricing_get failed', e);
      pricingTable = [];
    }
  }
  function setCostMode(mode: 'tokens' | 'cost'): void {
    costMode = mode;
    if (mode === 'cost' && pricingTable === null) void refreshPricingTable();
  }
  // The current session's dominant model: the newest turn that carried one.
  const currentModel = $derived.by(() => {
    for (let i = usageTurns.length - 1; i >= 0; i--) {
      const m = usageTurns[i].model;
      if (m) return m;
    }
    return null;
  });
  const currentRates = $derived(matchPricing(currentModel, pricingTable ?? []));
  const costActive = $derived(costMode === 'cost' && currentRates !== null);
  // Cost-mode normalization denominator (max per-turn dollar total).
  const usageCostMax = $derived.by(() => {
    if (!costActive || !currentRates) return 1;
    return Math.max(1e-9, ...shownTurns.map((t) => turnCost(t, currentRates).total));
  });

  // V16 Feature 4 honest-accounting: the panel's displaced figure is NET of
  // bypassed reminds (a displaced Read that came back via `cat` displaced
  // nothing). Subtract `bypassed_advice_chars` — the reminder-TEXT chars of
  // the bypassed reminders, the same unit `advisor_displaced_chars` sums —
  // never the whole-file `bypassed_chars` (one big-file bypass would zero
  // the entire metric).
  const netDisplacedChars = $derived(
    Math.max(
      0,
      (usage?.effectiveness.advisor_displaced_chars ?? 0) -
        (usage?.effectiveness.bypassed_advice_chars ?? 0),
    ),
  );

  async function markVerified(p: AdvisorProposal): Promise<void> {
    advisorBusy = p.rule_id;
    try {
      await harnessMarkVerified();
      advice = advice && { ...advice, proposals: advice.proposals.filter((x) => x.rule_id !== p.rule_id) };
    } catch (e) {
      console.error('harness_mark_verified failed', e);
    } finally {
      advisorBusy = null;
    }
  }

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
  // The selection survives the component's destroy/recreate cycle (tab
  // switch, hide/un-hide) and app restarts — see viewSection.ts. The
  // section-gated fetches (memory, usage) still run on mount because
  // `refresh()` re-checks `section` itself.
  let section = $state<Section>(
    loadViewSection('code-intelligence', SECTIONS.map((s) => s.id), 'overview'),
  );
  $effect(() => saveViewSection('code-intelligence', section));

  // Open/collapsed state of the Overview usage cards — native <details>
  // elements, so a remount would otherwise snap them back to collapsed.
  let sessionCardOpen = $state(loadCardOpen('code-intelligence', 'usage-this-session'));
  let advisorCardOpen = $state(loadCardOpen('code-intelligence', 'usage-advisor'));
  let sessionsCardOpen = $state(loadCardOpen('code-intelligence', 'usage-sessions'));
  $effect(() => saveCardOpen('code-intelligence', 'usage-this-session', sessionCardOpen));
  $effect(() => saveCardOpen('code-intelligence', 'usage-advisor', advisorCardOpen));
  $effect(() => saveCardOpen('code-intelligence', 'usage-sessions', sessionsCardOpen));

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

  // V22 Phase D/E: the "N suggested checks" passive nudge. Fetched once a graph
  // index is complete (a `ready` root exists); the chip derives from the
  // suggestion payload + the current `checks` setting (`computeChip` — either the
  // propose nudge, or an "auto-configure applied these" report), and a dismiss
  // is remembered per project. Kept lightweight, consistent with the analyses
  // "+N" badges elsewhere in this view.
  let checksSug = $state<ChecksSuggestion | null>(null);
  let checksSugFetched = false;
  let checksChipDismissed = $state(false);
  const checksChip = $derived(
    checksSug && !checksChipDismissed ? computeChip(checksSug, $settings.checks) : null,
  );

  async function maybeFetchChecksSuggestion(): Promise<void> {
    if (checksSugFetched) return;
    if (!roots.some((r) => r.state === 'ready')) return;
    checksSugFetched = true;
    try {
      checksSug = await checksSuggestion();
    } catch (e) {
      console.warn('checks_suggestion failed', e);
    }
  }

  async function dismissChecksChip(): Promise<void> {
    checksChipDismissed = true;
    try {
      await checksDismissSuggestion();
    } catch (e) {
      console.warn('checks_dismiss_suggestion failed', e);
    }
  }

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
    // V22: fetch the checks suggestion once an index is complete (guarded so it
    // runs at most once — the chip then recomputes reactively from settings).
    await maybeFetchChecksSuggestion();
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

  // Keep-alive (appViews.ts): this component now lives for the app's
  // lifetime, so the poll idles while the tab is off-screen and a fresh
  // refresh runs the moment it comes back. Re-probe the embedder too —
  // pre-keep-alive, the remount re-ran the reachability probe on every tab
  // visit, and nothing else ever updates it (an embedder started while the
  // tab was hidden would otherwise show "unreachable" until a manual probe).
  const unsubShown = onAppViewShown(GRAPH_MONITOR_TAB_ID, () => {
    void refresh();
    void testEmbedder();
  });

  onMount(async () => {
    await refresh();
    // A light poll backstops the event for coverage/progress counters that
    // change without a discrete state transition.
    poll = setInterval(() => {
      if (isAppViewVisible(GRAPH_MONITOR_TAB_ID)) void refresh();
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

<svelte:window onkeydown={onCostKeydown} />

<div class="graph-monitor">
  <header>
    <h2>Code Intelligence</h2>
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

  {#if checksChip}
    <div class="checks-chip">
      <button
        type="button"
        class="chip-body"
        onclick={() => void openSettingsWindowToSection('checks')}
        title="Open Settings → Checks"
      >
        <span class="chip-icon" aria-hidden="true">✓</span>
        {#if checksChip.mode === 'suggest'}
          <span
            >run_check: {checksChip.count} suggested check{checksChip.count === 1 ? '' : 's'} for this
            project</span
          >
        {:else}
          <span
            >run_check auto-configured: {checksChip.names.join(', ')} — review in Settings</span
          >
        {/if}
      </button>
      <button
        type="button"
        class="chip-x"
        aria-label="Dismiss"
        title="Dismiss"
        onclick={() => void dismissChecksChip()}>×</button
      >
    </div>
  {/if}

  {#if section === 'overview'}
  <h3 class="group-head">Usage</h3>
  <div class="usage-sec">
    <!-- Effectiveness: measured counters, never fabricated savings. -->
    <section class="card">
      <div class="history-head">Effectiveness</div>
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
            <span class="num" title={usage.effectiveness.bypassed_advice_chars > 0
              ? `${usage.effectiveness.advisor_displaced_chars.toLocaleString()} displaced − ${usage.effectiveness.bypassed_advice_chars.toLocaleString()} from reminders answered via shell reads (${usage.effectiveness.bypassed_chars.toLocaleString()} file chars re-read, est.)`
              : undefined}>{netDisplacedChars.toLocaleString()}</span>
            <span class="lbl">chars displaced by read-advisor{#if usage.effectiveness.bypassed_advice_chars > 0}&nbsp;(net of bypasses){/if} <span class="est-badge">est. ~{Math.round(netDisplacedChars / 4).toLocaleString()} tok</span></span>
          </div>
          <div>
            <span
              class="num"
              title="Content kept out of context is saved again on every later turn — the API re-sends the whole conversation per turn, so displaced chars are re-counted once per subsequent retrieve turn. Measured as the session runs; no projection."
            >{usage.effectiveness.compounded_chars.toLocaleString()}</span>
            <span class="lbl">chars of cache-reads avoided (compounding)
              <span class="est-badge">est. ~{Math.round(usage.effectiveness.compounded_chars / 4).toLocaleString()} tok</span>
              {#if costActive && currentRates}
                <span class="est-badge" title="At the matched model's cache-read rate">est. {fmtUsd((usage.effectiveness.compounded_chars / 4 / 1_000_000) * currentRates.cache_read)}</span>
              {/if}
            </span>
          </div>
          <div>
            <span class="num">{usage.offload_local_tasks.toLocaleString()}</span>
            <span class="lbl">tasks served locally — see the <em>Offload Server</em> tab</span>
          </div>
          <div>
            <span
              class="num"
              title="Serialized size of the graph tool descriptors advertised to the cloud session and the offload worker — cache-written once per session. Toggle Settings → Code Graph → lean tool surface to trim the cold-tail tools."
            >{usage.surface.mcp_tools.toLocaleString()}</span>
            <span class="lbl">tool surface: {usage.surface.mcp_chars.toLocaleString()} chars, cache-written once per session
              <span class="est-badge">est. ~{Math.round(usage.surface.mcp_chars / 4).toLocaleString()} tok</span>
            </span>
          </div>
        </div>
      {/if}
    </section>

    <!-- This session: per-turn stacked bars + top consumers. The segment
         colors flow from settings (via the legend's color pickers) into CSS
         vars scoped to this card. -->
    <details
      class="card"
      bind:open={sessionCardOpen}
      style="--ubar-in: {chartColors.in}; --ubar-cache: {chartColors.cache}; --ubar-write: {chartColors.write}; --ubar-out: {chartColors.out}; --ubar-tool: {chartColors.tool}"
    >
      <summary class="history-head">
        This session
        {#if shownTurns.length < usageTurns.length}
          <span class="muted">(last {shownTurns.length} of {usageTurns.length} turns)</span>
        {/if}
      </summary>
      {#if !usage || !usage.current || usage.current.turns.length === 0}
        <p class="placeholder">No usage recorded yet this session.</p>
      {:else}
        <!-- V16 Feature 8: tokens | est. cost. Cost mode reprices the same
             segments by $/MTok (auto-matched on the session's model);
             tokens stays the default. -->
        <div class="cost-toggle">
          <button
            class="mini {costMode === 'tokens' ? '' : 'secondary'}"
            onclick={() => setCostMode('tokens')}
          >tokens</button>
          <button
            class="mini {costMode === 'cost' ? '' : 'secondary'}"
            onclick={() => setCostMode('cost')}
          >est. cost</button>
          {#if costMode === 'cost' && !costActive}
            <span class="muted">
              {#if pricingTable === null}
                loading prices…
              {:else if !currentModel}
                no model id on this session's turns — showing tokens
              {:else}
                no price row matches <code>{currentModel}</code> (Settings → LLM pricing) — showing tokens
              {/if}
            </span>
          {/if}
        </div>
        <div class="ubars-legend">
          {#each CHART_SEGS as s (s.key)}
            <span>
              <input
                type="color"
                class="dot"
                value={chartColors[s.key]}
                title="{s.label} — click to pick a color"
                oninput={(e) => (chartPreview[s.key] = e.currentTarget.value)}
                onchange={(e) => commitChartColor(s, e.currentTarget.value)}
              />
              {s.label}
            </span>
          {/each}
        </div>
        <div class="ubars" bind:clientWidth={ubarsWidth}>
          {#each shownTurns as t, i (i)}
            {@const est_tool = Math.round(t.tool_chars / 4)}
            {@const cost = costActive && currentRates ? turnCost(t, currentRates) : null}
            {@const total = cost ? cost.total : turnTotal(t)}
            {@const max = cost ? usageCostMax : usageMax}
            {@const turnNo = usageTurns.length - shownTurns.length + i + 1}
            <div class="ubar-col">
              <div
                class="ubar"
                style="height: {barHeightPct(total, max)}%"
                title={cost
                  ? `turn ${turnNo} (est.): ${fmtUsd(cost.input)} in / ${fmtUsd(cost.cache_read)} cache-read / ${fmtUsd(cost.cache_write)} cache-write / ${fmtUsd(cost.output)} out / ${fmtUsd(cost.tool)} est. tool — ${fmtUsd(cost.total)}`
                  : `turn ${turnNo}: ${t.in_tok} in / ${t.cache_read} cache-read / ${t.cache_make} cache-write / ${t.out_tok} out / ~${est_tool} est. tool`}
              >
                {#if total > 0}
                  <!-- One segment list for both modes (order matches
                       CHART_SEGS: in / cache-read / cache-write / out /
                       tool) so a new segment can't be added to one mode and
                       missed in the other. -->
                  {@const segs = cost
                    ? [cost.input, cost.cache_read, cost.cache_write, cost.output, cost.tool].map(
                        (v) => v * 1e6,
                      )
                    : [t.in_tok, t.cache_read, t.cache_make, t.out_tok, est_tool]}
                  {#each CHART_SEGS as s, si (s.key)}
                    <span class="useg {s.key}" style="flex-grow: {segs[si]}"></span>
                  {/each}
                {/if}
              </div>
            </div>
          {/each}
        </div>

        <div class="history-head">Top consumers</div>
        {#if usage.current.top_tools.length === 0}
          <p class="placeholder">No tool-result usage recorded yet.</p>
        {:else}
          <div class="rows scroll5">
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
    </details>

    <!-- V14 Phase D2: Advisor card. -->
    <details class="card advisor" bind:open={advisorCardOpen}>
      <summary class="history-head">
        Budget-tuning advisor
        <span class="muted" title={ADVISOR_RULES_TOOLTIP}>ⓘ rules</span>
      </summary>
      {#if advisorError}
        <p class="placeholder">{advisorError}</p>
      {/if}
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
                <span class="aname">{p.setting || p.rule_id}</span>
                <span class="prop-vals"><code>{p.current}</code> → <code>{p.proposed}</code></span>
              </div>
              <p class="prop-rationale">{p.rationale}</p>
              <div class="prop-actions">
                {#if p.action === 'mark_verified'}
                  <button
                    class="mini"
                    disabled={advisorBusy !== null}
                    title="Stamp the currently-seen Claude Code version as verified — do this AFTER re-running the MAINTENANCE.md contract checks"
                    onclick={() => markVerified(p)}
                  >{advisorBusy === p.rule_id ? 'Marking…' : 'Mark verified'}</button>
                {:else if !p.warn_only}
                  <button
                    class="mini"
                    disabled={advisorBusy !== null}
                    onclick={() => applyProposal(p)}
                  >{advisorBusy === p.rule_id ? 'Applying…' : 'Apply'}</button>
                {/if}
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
    </details>

    <!-- Sessions: project-wide totals table. -->
    <details class="card" bind:open={sessionsCardOpen}>
      <summary class="history-head">Sessions <span class="muted">({usage?.sessions.length ?? 0})</span></summary>
      {#if !usage || usage.sessions.length === 0}
        <p class="placeholder">No sessions recorded yet.</p>
      {:else}
        <div class="rows scroll10">
          {#each usage.sessions as s (s.session_id)}
            {@const nCommits = commitCounts[s.session_id] ?? 0}
            <div class="sessrow-wrap">
              <button
                type="button"
                class="arow sessrow"
                title={`${s.totals.in_tok.toLocaleString()} input · ${s.totals.cache_make.toLocaleString()} cache-write · ${s.totals.cache_read.toLocaleString()} cache-read · ${s.totals.out_tok.toLocaleString()} output tokens — click for cost`}
                onclick={() => void openCostPopup(s)}
              >
                <span class="aname">{s.agent}{#if s.est_only}<span class="est-badge" title="No exact usage data for this agent — chars-only estimate">est</span>{/if}<span class="sess-date">{fmtDate(s.started_ms)}</span></span>
                {#if costMode === 'cost'}
                  {@const rowRates = matchPricing(s.models[0] ?? null, pricingTable ?? [])}
                  {#if rowRates}
                    <span class="sess-stats tnum">
                      {#each Object.entries(sessionCost(s.totals, rowRates)) as [cat, usd] (cat)}
                        {#if cat !== 'total'}
                          <span><b>{fmtUsd(usd)}</b> {cat.replace('_', '-').replace('input', 'in').replace('output', 'out')}</span>
                        {/if}
                      {/each}
                      <span><b>{fmtUsd(sessionCost(s.totals, rowRates).total)}</b> total <span
                          class="est-badge"
                          title={s.models.length > 1
                            ? `Mixed models — the whole session is priced at ${s.models[0]}'s rates (its top consumer by tokens); the other models' tokens are mispriced. Click for the manual cost popup.`
                            : 'Estimated from the auto-matched price row'}
                        >{s.models.length > 1 ? 'est · mixed' : 'est'}</span></span>
                    </span>
                  {:else}
                    <span class="sess-stats tnum muted" title="No price row auto-matches this session's model — add a model_prefix in Settings → LLM pricing, or click for the manual cost popup">
                      no price match{#if s.models[0]}&nbsp;(<code>{s.models[0]}</code>){/if}
                    </span>
                  {/if}
                {:else}
                  <span class="sess-stats tnum">
                    <span><b>{fmtTok(s.totals.in_tok)}</b> in</span>
                    <span><b>{fmtTok(s.totals.cache_make)}</b> cache-write</span>
                    <span><b>{fmtTok(s.totals.cache_read)}</b> cache-read</span>
                    <span><b>{fmtTok(s.totals.out_tok)}</b> out</span>
                  </span>
                {/if}
                <span class="aloc">cache-hit {Math.round(cacheHitRatio(s.totals.cache_read, s.totals.in_tok) * 100)}%</span>
              </button>
              {#if $settings.workbench.enabled}
                <button
                  type="button"
                  class="mini secondary commits-btn"
                  disabled={nCommits === 0}
                  title={nCommits === 0
                    ? 'No commits during this session'
                    : `Show the ${nCommits} commit${nCommits === 1 ? '' : 's'} made during this session in the Workbench`}
                  onclick={() => openCommits(s)}
                >⎇ {nCommits}</button>
              {/if}
            </div>
          {/each}
        </div>
      {/if}
    </details>
  </div>

  <div class="group-row">
    <h3 class="group-head">Index</h3>
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
  </div>
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

  <!-- Session-cost popup: tokens × $/MTok for the clicked Sessions row. -->
  {#if costSession && costRows}
    <div class="cost-backdrop" onclick={closeCostPopup} role="presentation"></div>
    <div class="cost-dialog" role="dialog" aria-modal="true" aria-label="Session cost">
      <div class="cost-title">
        <span>
          Session cost <span class="muted">· {costSession.agent}</span>
          {#if costSession.est_only}
            <span class="est-badge" title="No exact usage data for this agent — chars-only estimate; the cost below is an estimate too">est</span>
          {/if}
        </span>
        <button type="button" class="cost-close" aria-label="Close" onclick={closeCostPopup}>×</button>
      </div>

      <div class="cost-when muted">
        {fmtDate(costSession.started_ms)} · {fmtTime(costSession.started_ms)} – {fmtTime(costSession.last_ms)}
        {#if costModel}
          <span title={costSession.models.length > 1 ? `All models: ${costSession.models.join(', ')}` : undefined}>· {costModel}</span>
        {/if}
      </div>

      <label class="cost-provider">
        <span>Pricing</span>
        <select bind:value={costSelIdx}>
          {#each costPricing as p, i (i)}
            <option value={i}>{p.provider} — {p.model}</option>
          {/each}
          <option value={costPricing.length}>Custom…</option>
        </select>
      </label>

      {#if costSelIdx >= costPricing.length}
        <div class="cost-custom">
          <label><span>Input $/MTok</span><input type="number" min="0" step="0.01" bind:value={costCustom.input} /></label>
          <label><span>Cache write $/MTok</span><input type="number" min="0" step="0.01" bind:value={costCustom.cache_write} /></label>
          <label><span>Cache read $/MTok</span><input type="number" min="0" step="0.01" bind:value={costCustom.cache_read} /></label>
          <label><span>Output $/MTok</span><input type="number" min="0" step="0.01" bind:value={costCustom.output} /></label>
        </div>
      {/if}

      <table class="cost-table tnum">
        <thead>
          <tr>
            <th></th>
            <th>Input</th>
            <th>Cache write</th>
            <th>Cache read</th>
            <th>Output</th>
          </tr>
        </thead>
        <tbody>
          <tr>
            <th>Session tokens</th>
            <td title={costSession.totals.in_tok.toLocaleString()}>{fmtTok(costSession.totals.in_tok)}</td>
            <td title={costSession.totals.cache_make.toLocaleString()}>{fmtTok(costSession.totals.cache_make)}</td>
            <td title={costSession.totals.cache_read.toLocaleString()}>{fmtTok(costSession.totals.cache_read)}</td>
            <td title={costSession.totals.out_tok.toLocaleString()}>{fmtTok(costSession.totals.out_tok)}</td>
          </tr>
          <tr>
            <th>$ / MTok</th>
            <td>{costRates.input}</td>
            <td>{costRates.cache_write}</td>
            <td>{costRates.cache_read}</td>
            <td>{costRates.output}</td>
          </tr>
          <tr>
            <th>Cost</th>
            <td>{fmtUsd(costRows.input)}</td>
            <td>{fmtUsd(costRows.cache_write)}</td>
            <td>{fmtUsd(costRows.cache_read)}</td>
            <td>{fmtUsd(costRows.output)}</td>
          </tr>
        </tbody>
      </table>

      <div class="cost-total">
        Total session cost <b>{fmtUsd(costRows.total)}</b>
      </div>
      <small class="cost-hint">
        Prices are editable in Settings → LLM pricing.
      </small>
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
  /* V22: the run_check suggestion nudge — a lightweight, dismissable chip. */
  .checks-chip {
    display: flex;
    align-items: stretch;
    gap: 0;
    margin-bottom: 14px;
    border: 1px solid var(--accent, #3b6ea5);
    border-radius: 8px;
    overflow: hidden;
    background: rgba(59, 110, 165, 0.12);
    font-size: 12px;
    max-width: max-content;
  }
  .checks-chip .chip-body {
    display: flex;
    align-items: center;
    gap: 8px;
    padding: 6px 12px;
    background: transparent;
    border: none;
    color: var(--text, #ddd);
    cursor: pointer;
    text-align: left;
  }
  .checks-chip .chip-body:hover {
    background: rgba(59, 110, 165, 0.22);
  }
  .checks-chip .chip-icon {
    color: var(--accent, #3b6ea5);
    font-weight: 700;
  }
  .checks-chip .chip-x {
    padding: 0 10px;
    background: transparent;
    border: none;
    border-left: 1px solid var(--border, #333);
    color: var(--text, #ddd);
    opacity: 0.6;
    cursor: pointer;
    font-size: 15px;
  }
  .checks-chip .chip-x:hover {
    opacity: 1;
    background: rgba(255, 255, 255, 0.06);
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
  /* Group divider variant carrying the index actions on its right edge. */
  .group-row {
    display: flex;
    align-items: flex-end;
    justify-content: space-between;
    gap: 12px;
    margin: 18px 0 8px;
    border-bottom: 1px solid var(--border, #333);
    padding-bottom: 4px;
  }
  .group-row .group-head {
    margin: 0;
    border-bottom: none;
    padding-bottom: 0;
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
  /* Bounded lists: show ~N rows (an .arow is ~21.5px), scroll the rest.
     Horizontal overflow scrolls too — the sessrow stat columns are fixed-
     width for cross-row alignment, so on a narrow pane the rows would
     otherwise escape the card. width: max-content makes each row (and its
     border) span the full scrollable width, not just the visible strip. */
  .rows.scroll5,
  .rows.scroll10 {
    overflow-y: auto;
    overflow-x: auto;
  }
  .rows.scroll5 > .arow,
  .rows.scroll10 > .sessrow-wrap {
    min-width: 100%;
    width: max-content;
    box-sizing: border-box;
  }
  .rows.scroll5 {
    max-height: 108px;
  }
  .rows.scroll10 {
    max-height: 215px;
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

  /* Collapsible usage cards. The flex .history-head summary suppresses the
     native disclosure marker, so draw our own chevron. */
  .usage-sec details.card > summary {
    cursor: pointer;
    user-select: none;
    list-style: none;
  }
  .usage-sec details.card > summary::-webkit-details-marker {
    display: none;
  }
  .usage-sec details.card:not([open]) > summary {
    margin-bottom: 0;
  }
  .usage-sec details.card > summary::before {
    content: '▸';
    display: inline-block;
    opacity: 0.55;
    font-size: 11px;
    transition: transform 0.12s ease;
  }
  .usage-sec details.card[open] > summary::before {
    transform: rotate(90deg);
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
  /* Each legend dot is a native color input stripped down to its swatch —
     click to recolor that segment (persisted via settings). */
  .ubars-legend input.dot {
    width: 11px;
    height: 11px;
    padding: 0;
    border: 1px solid var(--border, #444);
    border-radius: 2px;
    background: none;
    cursor: pointer;
    -webkit-appearance: none;
    appearance: none;
  }
  .ubars-legend input.dot::-webkit-color-swatch-wrapper {
    padding: 0;
  }
  .ubars-legend input.dot::-webkit-color-swatch {
    border: none;
    border-radius: 1px;
  }
  .useg.in {
    background: var(--ubar-in, #58a6ff);
  }
  .useg.cache {
    background: var(--ubar-cache, #d2a8ff);
  }
  /* V16 Feature 8: cache-write got its own segment. */
  .useg.write {
    background: var(--ubar-write, #e3738d);
  }
  .useg.out {
    background: var(--ubar-out, #3fb950);
  }
  /* V16 Feature 8: the tokens | est. cost mode toggle above the bars. */
  .cost-toggle {
    display: flex;
    align-items: center;
    gap: 6px;
    margin: 2px 0 8px;
    font-size: 10.5px;
  }
  .useg.tool {
    background: var(--ubar-tool, #f0c674);
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
  /* Each row is a flex wrapper holding TWO buttons (the cost-popup row and
     the Workbench commits jump) — nesting them would be invalid HTML. The
     row divider lives on the wrapper so it spans both. */
  .sessrow-wrap {
    display: flex;
    align-items: center;
    gap: 6px;
    border-bottom: 1px solid var(--border, #2a2a2a);
  }
  /* The main row area is a <button> (it opens the cost popup) — strip the UA
     button chrome so it keeps rendering as a plain grid row. */
  button.arow.sessrow {
    background: none;
    border: none;
    color: inherit;
    font: inherit;
    text-align: left;
    flex: 1 1 auto;
    min-width: 0;
    cursor: pointer;
  }
  button.arow.sessrow:hover {
    background: var(--surface-raised, rgba(255, 255, 255, 0.04));
  }
  .commits-btn {
    flex: 0 0 auto;
    font-variant-numeric: tabular-nums;
    white-space: nowrap;
  }

  /* Session-cost popup. Fixed-position so it centers on the window even
     though this tab's root is its own absolutely-positioned scroll box. */
  .cost-backdrop {
    position: fixed;
    inset: 0;
    background: rgba(0, 0, 0, 0.5);
    z-index: 100;
  }
  .cost-dialog {
    position: fixed;
    top: 50%;
    left: 50%;
    transform: translate(-50%, -50%);
    z-index: 101;
    min-width: 30rem;
    max-width: min(44rem, calc(100vw - 2rem));
    background: var(--surface-3, #1d1d1d);
    border: 1px solid var(--border, #333);
    border-radius: 8px;
    padding: 14px 16px;
    box-shadow: 0 8px 32px rgba(0, 0, 0, 0.45);
    font-size: 13px;
  }
  .cost-when {
    font-size: 12px;
    margin: -6px 0 10px;
    font-variant-numeric: tabular-nums;
  }
  .cost-title {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 8px;
    font-weight: 600;
    margin-bottom: 10px;
  }
  .cost-close {
    background: none;
    border: none;
    color: inherit;
    font-size: 16px;
    line-height: 1;
    cursor: pointer;
    padding: 2px 6px;
  }
  .cost-close:hover {
    color: var(--text, #fff);
  }
  .cost-provider {
    display: flex;
    align-items: center;
    gap: 8px;
    margin-bottom: 10px;
  }
  .cost-provider select {
    flex: 1;
  }
  .cost-custom {
    display: grid;
    grid-template-columns: 1fr 1fr;
    gap: 6px 12px;
    margin-bottom: 10px;
  }
  .cost-custom label {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 8px;
  }
  .cost-custom input {
    width: 6.5rem;
    text-align: right;
  }
  .cost-table {
    width: 100%;
    border-collapse: collapse;
    margin-bottom: 10px;
  }
  .cost-table th,
  .cost-table td {
    padding: 4px 8px;
    border-bottom: 1px solid var(--border, #2a2a2a);
  }
  .cost-table thead th {
    text-align: right;
    font-weight: 600;
    color: var(--text-subtle, #999);
  }
  .cost-table tbody th {
    text-align: left;
    font-weight: 500;
    color: var(--text-subtle, #999);
    white-space: nowrap;
  }
  .cost-table td {
    text-align: right;
    white-space: nowrap;
  }
  .cost-table tbody tr:last-child th,
  .cost-table tbody tr:last-child td {
    font-weight: 600;
    color: var(--text, #ddd);
  }
  .cost-total {
    display: flex;
    justify-content: space-between;
    align-items: baseline;
    font-size: 14px;
    margin-bottom: 6px;
  }
  .cost-total b {
    font-size: 16px;
  }
  .cost-hint {
    color: var(--text-subtle, #999);
  }
  /* Constant width + right alignment so the percentages line up too. */
  .arow.sessrow .aloc {
    min-width: 14ch;
    text-align: right;
  }
  .sess-date {
    margin-left: 8px;
    font-weight: 400;
    font-size: 11px;
    opacity: 0.6;
    font-variant-numeric: tabular-nums;
  }
  .sess-stats {
    /* Fixed tracks so the values line up as columns ACROSS rows (fmtTok is
       ≤ 6 chars and each column's label is constant); flex sized every row
       by its own content, so nothing aligned vertically. Track order MUST
       match the span order: in · cache-write · cache-read · out. */
    display: grid;
    grid-template-columns: 4.6rem 7.8rem 7.2rem 4.6rem;
    font-size: 11px;
    opacity: 0.85;
  }
  .sess-stats b {
    font-weight: 600;
  }
  .sess-stats > span {
    position: relative;
    text-align: right;
    padding-left: 13px;
    white-space: nowrap;
  }
  /* Subtle mid-height tick between values — a hint of a column boundary,
     deliberately not a full-height rule (this must not read as a table). */
  .sess-stats > span + span::before {
    content: '';
    position: absolute;
    left: 5px;
    top: 50%;
    transform: translateY(-50%);
    width: 1px;
    height: 0.8em;
    background: var(--border, #444);
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
