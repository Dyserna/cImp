<script lang="ts">
  /// Code Intelligence → Overview (#130, F4). The usage X-ray: the two
  /// dashboard donuts, the per-turn token chart, the effectiveness counters,
  /// the cost table, the advisor proposals and the sessions list — 660 lines of
  /// markup and ~1,200 of script that `CodeIntelligenceView.svelte` used to
  /// carry alongside five other sections.
  ///
  /// WHAT THIS COMPONENT OWNS. Everything the Overview alone reads: the
  /// `graph_usage` / `graph_usage_advice` snapshots and the whole fetch gate
  /// around them (in-flight, sequence, payload-key and cadence), the session
  /// drill-in and focus-follow, the price table and cost overrides, the chart
  /// zoom/scroll geometry, the lane and segment colour pickers, and the four
  /// `<details>` card-open flags.
  ///
  /// WHAT THE PARENT STILL OWNS: the schedule. The 2s keep-alive poll stays in
  /// exactly one place, and it drives this component through `tick()` below
  /// rather than owning a second timer here.
  ///
  /// WHY `active` INSTEAD OF AN `{#if}` IN THE PARENT. The section markup was
  /// inside `{#if section === 'overview'}`, but the STATE behind it lived in
  /// the parent's script, which a section switch does not touch — so leaving
  /// the Overview and coming back kept the selected session, the follow mode,
  /// the chart zoom and the scroll offset. Rendering this component only while
  /// its section is open would destroy all of that on every section switch.
  /// So the parent mounts it unconditionally and passes `active`; the markup
  /// below is what the `{#if}` used to gate, and it renders in the same place,
  /// in the same DOM order, under the same parent element.
  ///
  /// STYLES: no import here. `codeIntel.css` is keyed on the `.graph-monitor`
  /// class the parent puts on its root and this renders inside that element, so
  /// the shared chrome (`button`, `.card`, `.arow`, `.rows`, `.history-head`, …)
  /// reaches this markup through the DOM. Only the rules no other section can
  /// reach travelled here.
  import { onMount, onDestroy } from 'svelte';
  import { listen } from '@tauri-apps/api/event';
  import { writeText as clipboardWriteText } from '@tauri-apps/plugin-clipboard-manager';
  import { get } from 'svelte/store';
  import {
    advisorDismiss,
    advisorMarkApplied,
    advisorRules,
    graphSessionUsage,
    graphTabSession,
    graphUsage,
    graphUsageAdvice,
    harnessMarkVerified,
    type AdvisorProposal,
    type AdvisorSnapshot,
    type ModelUsage,
    type SessionUsageDetail,
    type SessionUsageRow,
    type UsageSnapshot,
  } from '../graph';
  import {
    FREE_RATES,
    agentBarClass,
    arcPath,
    barHeightPct,
    cacheHitRatio,
    costGrandTotal,
    costOverrideForIdx,
    costRowState,
    decideUsageApply,
    donutArcs,
    fmtPct,
    fmtTok,
    fmtUsd,
    isActiveSessionIn,
    isEmptyDetailRow,
    kindsTotal,
    laneLabel,
    laneLabelVisible,
    laneSegments,
    matchPricing,
    maxTurnTotal,
    originKindTotals,
    originShareLine,
    sessionCost,
    sessionRowState,
    turnCost,
    turnTotal,
    type CostOverride,
    type CostRowState,
    type PriceRates,
  } from '../usageMath';
  import { fmtDate, fmtTime } from '../format';
  import { listenManaged } from '../listenManaged';
  import { applySettings, settings } from '../settings/store';
  import { llmPricingGet } from '../settings/ipc';
  import type { LlmPricingModel } from '../settings/types';
  import { openSessionCommits, workbenchSessionCommitCounts } from '../workbench';
  import { revealTab } from '../tabs/visibility';
  import { activeTab } from '../tabs/state';
  import { tabMeta } from '../tabs/store';
  import { WORKBENCH_TAB_ID } from '../tabs/types';
  import { harnessLabel, harnesses } from '../harness';
  import { harnessUsage, type DeclaredOrigin } from '../ipc';
  import { loadCardOpen, loadViewString, saveCardOpen, saveViewString } from '../viewSection';

  let {
    active,
    onActiveSessions,
  }: {
    /// Whether the Overview is the section on screen. Gates the MARKUP only —
    /// every piece of state below survives a section switch, exactly as it did
    /// when all of it lived in the parent.
    active: boolean;
    /// Push `active_session_ids` up as each usage snapshot lands. The Memory
    /// section's "this session / last session" label is the other reader of
    /// that list, and this component owns the snapshot it arrives in.
    onActiveSessions: (ids: readonly string[]) => void;
  } = $props();

  // Usage (V14 Phase D/D2): the token X-ray + the budget-tuning advisor's
  // proposals card. Fetched only while the section is open (same posture as
  // Memory) — folded into `refresh()`'s poll below.
  // `$state.raw`, not `$state`: the snapshot is a deep tree (every session row,
  // and a turn series that runs to thousands of entries) and plain `$state`
  // deep-proxies all of it on every assignment, then routes every read in the
  // render path through a proxy trap. Nothing here ever mutates the snapshot in
  // place — it is only ever REPLACED wholesale by `refreshUsage` — so the deep
  // proxy buys no reactivity it doesn't already get from the reassignment.
  let usage = $state.raw<UsageSnapshot | null>(null);
  // Replaced wholesale like `usage` above (the three local proposal drops all
  // build a fresh object), so the same `$state.raw` reasoning applies.
  let advice = $state.raw<AdvisorSnapshot | null>(null);
  let advisorBusy = $state<string | null>(null); // rule_id currently applying/dismissing
  // Rendered in the Advisor card when an Apply can't be honored (e.g. a
  // proposal names a setting this build has no case for) — cleared on the
  // next successful apply.
  let advisorError = $state<string | null>(null);

  // A `graph_usage` pass over a LARGE store has been measured at 27–60s —
  // far longer than the 2s poll below. Without a gate the ticks piled up
  // unboundedly and landed out of order, so the Sessions list visibly
  // flapped (loads → clears → loads). `usageInFlight` is the gate: a tick
  // that fires while the previous fetch is still pending is dropped, not
  // queued (the interval is a backstop, so the next one is 2s away).
  let usageInFlight = false;
  // Belt-and-braces ordering guard. The in-flight gate alone nearly
  // guarantees it, but `refreshUsage` has other callers than the poll (the
  // section-switch onclick and `onAppViewShown` below) — a response must
  // never overwrite state written by a LATER-started request, so stamp each
  // request and refuse to apply a stamp that's already been superseded.
  let usageSeq = 0;
  let usageApplied = 0;
  // Was the LAST applied tick in the store-error state? Drives the
  // transition-only notice flash (see `decideUsageApply`).
  let usageErrored = false;
  // Serialized form of the snapshot currently in `usage` — the change gate.
  // Reassigning `$state` invalidates the WHOLE derived graph downstream
  // (`originKindTotals` over the full turn series, the per-model cost rows,
  // `usageCostMax`) and re-diffs the turn chart, which is up to
  // `TURN_RENDER_CAP` columns of ~7 nodes each. Doing that every poll tick for
  // a byte-identical payload is what made the Overview janky to scroll while a
  // second agent tab was running. Comparing the serialized payload (rather
  // than a hand-picked field list) is deliberate: a fingerprint that misses a
  // field renders permanently stale data, and the stringify is orders of
  // magnitude cheaper than the render it prevents.
  let usageKey: string | null = null;
  let adviceKey: string | null = null;

  /// Drop one proposal from the card without waiting for the next poll, after
  /// its action succeeded. Clearing `adviceKey` is load-bearing: the key gate
  /// above compares against the last SERVER payload, so a local edit must
  /// invalidate it — otherwise a best-effort action that did not actually
  /// stick server-side (`advisorMarkApplied` documents itself as one) would
  /// see an unchanged payload next tick, skip the apply, and leave the
  /// proposal hidden forever instead of re-proposing.
  ///
  /// Identified by (rule_id, signature), not by rule_id alone: since V35
  /// Phase E every harness-capability drift notice shares the one
  /// `drift.capability.v1` id and is told apart by its signature, so dropping
  /// by rule id would clear a sibling capability's card along with the one the
  /// user acted on.
  function dropProposal(p: AdvisorProposal): void {
    adviceKey = null;
    advice = advice && {
      ...advice,
      proposals: advice.proposals.filter(
        (x) => !(x.rule_id === p.rule_id && x.signature === p.signature),
      ),
    };
  }

  /// The identity of one proposal within the card — the `{#each}` key, and the
  /// value `advisorBusy` holds so a spinner marks the row that is working
  /// rather than every row sharing its rule id. Same (rule_id, signature) pair
  /// `dropProposal` and the dismiss memory use.
  function proposalKey(p: AdvisorProposal): string {
    return `${p.rule_id} ${p.signature}`;
  }
  // When the last usage fetch FINISHED. The gap is measured from completion,
  // not from start, so the effective cadence self-tunes: a fast store polls at
  // roughly the tick rate, a slow one (a `graph_usage` pass has been measured
  // in seconds) backs itself off instead of re-entering the moment it returns
  // and pinning the store's single connection against the transcript taps.
  let usageDoneAt = 0;
  const USAGE_MIN_GAP_MS = 2000;

  function usageDue(): boolean {
    return Date.now() - usageDoneAt >= USAGE_MIN_GAP_MS;
  }

  async function refreshUsage(): Promise<void> {
    if (usageInFlight) return;
    usageInFlight = true;
    const seq = ++usageSeq;
    // Independent fetches — run concurrently so the 2s Overview poll pays
    // one round-trip of wall time, not two. Each keeps its last good value
    // on failure, same as before.
    let u: UsageSnapshot | null;
    let a: AdvisorSnapshot | null;
    try {
      [u, a] = await Promise.all([
        graphUsage().catch((e) => {
          console.warn('graph_usage failed', e);
          return null;
        }),
        graphUsageAdvice().catch((e) => {
          console.warn('graph_usage_advice failed', e);
          return null;
        }),
      ]);
    } finally {
      usageInFlight = false;
      usageDoneAt = Date.now();
    }
    if (seq <= usageApplied) return; // superseded by a later-started request
    usageApplied = seq;
    // `store_error != null` ⇒ the store couldn't be read this tick, so the
    // payload (notably its empty `sessions`) is not authoritative: keep the
    // last-good snapshot on screen and say so once, on entry into the
    // condition only. A healthy snapshot applies even when empty.
    const d = decideUsageApply(u, usageErrored);
    usageErrored = d.errored;
    if (d.flash) flashSessionNotice('Usage store busy — showing last loaded data.');
    // Apply only what actually changed (see `usageKey`) — an unchanged payload
    // must not touch `$state`, or the whole Overview re-renders for nothing.
    if (u && d.apply) {
      const key = JSON.stringify(u);
      if (key !== usageKey) {
        usageKey = key;
        usage = u;
        // The Memory section (still the parent's) labels its working set from
        // the same list. Pushed HERE, synchronously with the assignment, not
        // from an `$effect`: an effect runs after the render that already read
        // the stale value, which would paint one frame of "last session" on a
        // session that had just gone live.
        onActiveSessions(u.active_session_ids ?? []);
      }
    }
    if (a) {
      const key = JSON.stringify(a);
      if (key !== adviceKey) {
        adviceKey = key;
        advice = a;
      }
    }
    // V16 Feature 8: the cost view's auto-match price table. Fetched once
    // per view lifetime here (not per poll — prices change rarely); Settings
    // edits land live via the `llm-pricing-changed` listener, and card opens
    // refetch too.
    if (costMode === 'cost' && pricingTable === null) void refreshPricingTable();
    // V34: re-attempt the focus follow now that rows may have landed. The
    // subscription only fires on a tab CHANGE, so without this a tab focused
    // before its session had any rows would never be picked up.
    if (lastAgentTab) void followActiveTab(lastAgentTab);
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

  // ── V24 Phase C: session drill-in ────────────────────────────────────
  // Clicking a Sessions row selects that session — the "This session" card
  // then renders its fetched detail (turns / top-tools) instead of
  // `usage.current`. Null = live mode (current session). `selectedId` guards
  // against a slow response landing after the user clicked another row.
  let selectedSession = $state<SessionUsageDetail | null>(null);
  // The CLICKED row — always fully populated (agent/date/id), so the selected
  // card title is robust even if the fetched detail's `row` is the empty
  // sentinel. Set/cleared alongside `selectedSession`.
  let selectedRow = $state<SessionUsageRow | null>(null);
  let selectedId = $state<string | null>(null);
  let copiedId = $state(false);
  // Transient inline notice under the Sessions list (e.g. a vanished session).
  let sessionNotice = $state<string | null>(null);
  let sessionNoticeTimer: ReturnType<typeof setTimeout> | null = null;
  function flashSessionNotice(msg: string): void {
    sessionNotice = msg;
    if (sessionNoticeTimer) clearTimeout(sessionNoticeTimer);
    sessionNoticeTimer = setTimeout(() => (sessionNotice = null), 4000);
  }

  // ── V34: follow the focused agent tab ────────────────────────────────
  // `usage.current` is "the session with the most recent activity across all
  // agents" — with two tabs of one harness open that is whichever tab last WROTE, not
  // the one being looked at, so the card appeared stuck on one tab's session.
  // The fix is to select the focused tab's session automatically, using the
  // same drill-in path a click uses.
  //
  // `following` is the mode bit: true until the user picks a row by hand, and
  // restored by "back to live". While following, a tab switch re-selects; while
  // not, the user's choice stands (their click is a stronger signal than focus).
  let following = $state(true);
  // The session id the follow last applied, so a re-resolve to the same session
  // doesn't refetch on every poll.
  let followedId: string | null = null;

  /// Point the card at whatever the focused tab is working in. A `null` answer
  /// means the app cannot PROVE which session that tab owns (an unpinned tab
  /// sharing a project, a tab that hasn't started) — in that case we leave the
  /// card exactly as it was rather than guessing, which is the same fail-open
  /// posture the rest of the identity path takes.
  async function followActiveTab(tab: string): Promise<void> {
    if (!following) return;
    let sid: string | null = null;
    try {
      sid = await graphTabSession(tab);
    } catch (e) {
      console.warn('graph_tab_session failed', e);
      return;
    }
    if (!sid || !following || sid === followedId) return;
    // The focused tab IS the live session ⇒ show the live card, not a drill-in.
    if (usage?.current?.session_id === sid) {
      followedId = sid;
      clearSelection({ keepFollowing: true });
      return;
    }
    const row = usage?.sessions.find((s) => s.session_id === sid);
    // A known tab whose rows haven't landed yet (a fresh session, or the very
    // first snapshot). `followedId` stays PUT so the retry below re-attempts
    // once they do — recording it here would make this the one tab the follow
    // never catches up with.
    if (!row) return;
    followedId = sid;
    await selectSession(row, { auto: true });
  }

  // The last AGENT tab to hold focus — the follow target.
  //
  // NOT `activeTab` itself: looking at this dashboard makes IT the active tab,
  // and a dashboard has no session, so keying on `activeTab` resolved nothing
  // the moment the user actually looked at the card and left the selection
  // stuck on whatever was focused first. Only `ai-tool` tabs move the target;
  // focusing a dashboard, a shell or a preview leaves it where it was, which is
  // what "the session of the tab I was last working in" means.
  let lastAgentTab: string | null = null;

  // Subscribing to the store (rather than an `$effect` over `$activeTab`) keeps
  // this working while the view is detached: the app-view registry keeps this
  // component mounted, so a tab switch that happens while Code Intelligence is
  // off-screen must still be reflected when it comes back.
  const unsubActiveTab = activeTab.subscribe((t) => {
    if (tabMeta(t)?.kind !== 'ai-tool') return;
    lastAgentTab = t;
    void followActiveTab(t);
  });

  async function selectSession(
    s: SessionUsageRow,
    opts: { auto?: boolean } = {},
  ): Promise<void> {
    // A hand-picked row pins the card: stop following focus until the user
    // explicitly goes back to live.
    if (!opts.auto) following = false;
    selectedId = s.session_id;
    // Reveal the card so the freshly selected session is actually visible.
    sessionCardOpen = true;
    // Cost mode needs the price table in the selected view too.
    if (costMode === 'cost' && pricingTable === null) void refreshPricingTable();
    try {
      const detail = await graphSessionUsage(undefined, s.session_id);
      if (selectedId !== s.session_id) return; // superseded by a later click
      // The session vanished or the graph is off → the backend returns an
      // empty-sentinel detail. Don't enter selected mode (its title would read
      // "Session ·  · 1970-01-01…"); stay live and surface a transient notice.
      if (isEmptyDetailRow(detail.row)) {
        // An AUTO selection failing is not a user-facing event: nobody asked
        // for this session, so stay live quietly and keep following. Notably we
        // do NOT reset `followedId` — that would re-attempt (and re-flash) on
        // every poll for as long as the focused tab has no stored detail.
        clearSelection({ keepFollowing: opts.auto });
        if (!opts.auto) flashSessionNotice('Session data no longer available.');
        return;
      }
      selectedRow = s;
      selectedSession = detail;
      // Seed the auto-refresh key with the CLICKED row's last_ms so the
      // effect below doesn't immediately refetch the snapshot it just got.
      selectedFetchKey = `${s.session_id}:${s.last_ms}`;
      seedCostCustom(detail.per_model); // Cost card rows for this session.
    } catch (e) {
      console.warn('graph_session_usage failed', e);
      if (selectedId === s.session_id) clearSelection({ keepFollowing: opts.auto });
    }
  }

  /// Back to live. `keepFollowing` is set by the follow path itself (which is
  /// already in follow mode and must not be read as a user action); a user
  /// clicking "back to live" RE-ENABLES following, since going live is exactly
  /// a request to track whatever is current rather than a pinned session.
  function clearSelection(opts: { keepFollowing?: boolean } = {}): void {
    selectedSession = null;
    selectedRow = null;
    selectedId = null;
    selectedFetchKey = '';
    if (!opts.keepFollowing) {
      following = true;
      followedId = null;
    }
  }

  // Keep a drilled-in session LIVE: the snapshot fetched at click time goes
  // stale while its session keeps running (selecting the current session
  // froze the chart until the row was clicked again). The 2s Overview poll
  // refreshes `usage.sessions`; whenever the selected session's row advances
  // (`last_ms` moves on every recorded turn) refetch its detail. Idle
  // sessions never advance, so this costs nothing for historical drill-ins.
  // Non-reactive key guard = one fetch per advance, and no effect loop when
  // the fetch itself replaces `selectedSession`.
  let selectedFetchKey = '';
  // Same pile-up hazard as `refreshUsage`: on a slow store the detail fetch
  // can outlive several `last_ms` advances, and the key guard alone would
  // start a new one per advance. Skip BEFORE writing the key, so the stale
  // key survives and the next applied `usage` snapshot re-triggers this
  // effect and retries — a dropped tick is never a permanently stale card.
  let selectedFetchInFlight = false;
  $effect(() => {
    const sid = selectedId;
    if (!sid || !selectedSession) return;
    const row = usage?.sessions.find((s) => s.session_id === sid);
    if (!row) return;
    const key = `${sid}:${row.last_ms}`;
    if (selectedFetchKey === key) return;
    if (selectedFetchInFlight) return;
    selectedFetchKey = key;
    void refetchSelected(sid);
  });

  async function refetchSelected(sid: string): Promise<void> {
    selectedFetchInFlight = true;
    try {
      const detail = await graphSessionUsage(undefined, sid);
      if (selectedId !== sid) return; // superseded / cleared mid-flight
      if (isEmptyDetailRow(detail.row)) return; // vanished — keep the last snapshot
      selectedSession = detail;
      seedCostCustom(detail.per_model);
    } catch (e) {
      console.warn('graph_session_usage (selected refresh) failed', e);
    } finally {
      selectedFetchInFlight = false;
    }
  }

  // Return to the live current-session view. Lives on a button inside the
  // card <summary>, so it must not also toggle the <details>.
  function goLive(e: Event): void {
    e.preventDefault();
    e.stopPropagation();
    clearSelection();
  }

  // Copy the FULL selected session id (the <summary> only shows an 8-char
  // prefix). WebView2 denies `navigator.clipboard`, so use the Tauri
  // clipboard plugin (project_webview_clipboard_wheel).
  async function copySessionId(e: Event): Promise<void> {
    e.preventDefault();
    e.stopPropagation();
    if (!selectedRow) return;
    try {
      await clipboardWriteText(selectedRow.session_id);
      copiedId = true;
      setTimeout(() => (copiedId = false), 1200);
    } catch (err) {
      console.warn('copy session id failed', err);
    }
  }

  // ── V24 Phase D: Cost card — per-model what-if pricing ───────────────
  // The collapsible Cost card (replacing the old single-rate cost popup)
  // prices each model in the session separately, each row picking its own
  // rates. It shares the ONE `pricingTable` declared here (also feeding the
  // tokens|cost toggle and the Sessions-row costs), refreshed on both cost-mode
  // entry and Cost-card open, so a Settings → LLM pricing edit propagates to
  // every consumer consistently.
  //   Live mode  = the current session's `per_model` (lazy-fetched on open).
  //   Selected   = the already-fetched `selectedSession.per_model`.
  let costCardOpen = $state(loadCardOpen('code-intelligence', 'usage-cost'));
  $effect(() => saveCardOpen('code-intelligence', 'usage-cost', costCardOpen));
  // V28: the Dashboard card (donuts) — open by default; it's the Overview's
  // at-a-glance header. It shares the Cost card's per-model + pricing needs,
  // so the two effects below gate on EITHER card being open.
  let dashCardOpen = $state(loadCardOpen('code-intelligence', 'usage-dashboard', true));
  $effect(() => saveCardOpen('code-intelligence', 'usage-dashboard', dashCardOpen));
  // The ONE LLM price table for every Usage-section consumer. `null` = not
  // fetched yet (fetch on first need); `[]` = fetched, empty.
  let pricingTable = $state<LlmPricingModel[] | null>(null);
  const pricingRows = $derived(pricingTable ?? []);
  async function refreshPricingTable(): Promise<void> {
    try {
      pricingTable = await llmPricingGet();
    } catch (e) {
      console.warn('llm_pricing_get failed', e);
      pricingTable = [];
    }
  }
  // The current session's full detail, lazy-fetched for live-mode pricing, plus
  // the snapshot it was fetched at (session id + turn count) so live mode
  // refetches when the session ADVANCES rather than freezing for its lifetime.
  let liveDetail = $state<SessionUsageDetail | null>(null);
  let liveDetailAt = $state<{ sid: string; turns: number } | null>(null);
  // Non-reactive in-flight guard: the `sid:turns` snapshot being fetched, so
  // the live effect fires at most once per poll tick.
  let liveFetchKey = '';
  // Per-model pricing OVERRIDES (user picks only), keyed by model id — no entry
  // means "follow the live auto-match against the current table" (see
  // `costRowByModel`). Values are stable across table edits (row key / custom /
  // free), never a positional index. Custom rate objects are seeded eagerly so
  // the number-input binds below always have an lvalue.
  let costOverrideByModel = $state<Record<string, CostOverride>>({});
  let costCustomByModel = $state<Record<string, PriceRates>>({});

  // The per_model list driving the card: selected session's, else the live
  // current session's (empty until fetched).
  const costPerModel = $derived(
    selectedSession ? selectedSession.per_model : (liveDetail?.per_model ?? []),
  );

  async function fetchLiveDetail(sid: string, atTurns: number): Promise<void> {
    try {
      const d = await graphSessionUsage(undefined, sid);
      // Guard against the current session having rotated mid-flight.
      if (usage?.current?.session_id === sid) {
        liveDetail = d;
        liveDetailAt = { sid, turns: atTurns };
        seedCostCustom(d.per_model);
      }
    } catch (e) {
      console.warn('graph_session_usage (cost card) failed', e);
    }
  }

  // Ensure every model has a custom-rates object so `bind:value` on the custom
  // inputs always targets a real field (called synchronously at each data-set
  // point, before the card renders those rows).
  function seedCostCustom(models: ModelUsage[]): void {
    for (const m of models) {
      if (!costCustomByModel[m.model]) {
        costCustomByModel[m.model] = { input: 0, cache_write: 0, cache_read: 0, output: 0 };
      }
    }
  }

  // Refresh the shared price table each time the Cost or Dashboard card opens
  // — belt-and-braces alongside the `llm-pricing-changed` push listener (which
  // covers Settings edits while a card is already open).
  $effect(() => {
    if (costCardOpen || dashCardOpen) void refreshPricingTable();
  });

  // Live mode: lazy-fetch the current session's detail while a per-model
  // consumer (Cost or Dashboard card) is open, refetch when its turn count
  // advances, and drop it when BOTH close so a reopen refetches. No fetch
  // while closed or in selected mode; the turn-count + in-flight guards keep
  // it to at most one fetch per poll tick.
  $effect(() => {
    const needDetail = costCardOpen || dashCardOpen;
    if (!needDetail || selectedSession) {
      // Both cards closed → clear so a reopen refetches. (Selected mode just
      // parks the live detail; it isn't shown, so leave it.)
      if (!needDetail && (liveDetail || liveDetailAt)) {
        liveDetail = null;
        liveDetailAt = null;
        liveFetchKey = '';
      }
      return;
    }
    const cur = usage?.current;
    const sid = cur?.session_id;
    if (!sid) return;
    const turns = cur.turns.length;
    // Already hold this exact snapshot → nothing to do.
    if (liveDetailAt && liveDetailAt.sid === sid && liveDetailAt.turns === turns) return;
    const key = `${sid}:${turns}`;
    if (liveFetchKey === key) return; // this snapshot is already being fetched
    liveFetchKey = key;
    void fetchLiveDetail(sid, turns);
  });

  // Keep every row's custom-rates object present (lvalue for the Custom inputs).
  $effect(() => {
    seedCostCustom(costPerModel);
  });

  // Per-model resolved select state (selIdx / rates / matchedRow), recomputed
  // against the CURRENT table on every render-relevant change — the single
  // source of truth for the card's selects and cost figures. An override whose
  // named row no longer exists falls back to auto-match, never a wrong row and
  // never a silent Free.
  const costRowByModel = $derived.by(() => {
    const rows = pricingRows;
    const out: Record<string, CostRowState<LlmPricingModel>> = {};
    for (const m of costPerModel) {
      out[m.model] = costRowState(
        m.model,
        costOverrideByModel[m.model],
        rows,
        costCustomByModel[m.model] ?? FREE_RATES,
      );
    }
    return out;
  });
  const costGrand = $derived(
    costGrandTotal(costPerModel, (i) => costRowByModel[costPerModel[i].model].rates),
  );


  // Applies a proposal by writing the ONE named `graph.*` field it targets
  // through the normal settings round-trip (`applySettings` — visible in
  // Settings, undoable, migration-safe). There is no bespoke "apply" IPC —
  // the advisor never mutates settings itself (milestone Feature 1b: never
  // silent self-modification).
  async function applyProposal(p: AdvisorProposal): Promise<void> {
    advisorBusy = proposalKey(p);
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
      dropProposal(p);
    } finally {
      advisorBusy = null;
    }
  }

  async function dismissProposal(p: AdvisorProposal): Promise<void> {
    advisorBusy = proposalKey(p);
    try {
      await advisorDismiss(p.rule_id, p.signature);
      dropProposal(p);
    } catch (e) {
      console.error('advisor_dismiss failed', e);
    } finally {
      advisorBusy = null;
    }
  }

  // Stacked-bar chart derived state (This Session). Pure math lives in
  // `./usageMath` (unit-tested); this just wires it to the current turns.
  // V24 Phase C: source the card from the selected session when one is
  // picked, else the live current session — so `shownTurns`, `currentModel`,
  // the cost toggle, and the top-tools list all follow the selection with no
  // further branching.
  let usageTurns = $derived(selectedSession ? selectedSession.turns : (usage?.current?.turns ?? []));
  let cardTopTools = $derived(
    selectedSession ? selectedSession.top_tools : (usage?.current?.top_tools ?? []),
  );
  // The chart used to DROP the oldest turns once bars hit their minimum
  // width; it now scrolls horizontally instead (wheel = zoom, shift+wheel =
  // pan), so every turn stays reachable. Only a hard DOM cap remains — each
  // column is ~7 nodes, so an absurdly long session can't bloat the card.
  const TURN_RENDER_CAP = 1000;
  let shownTurns = $derived(
    usageTurns.length > TURN_RENDER_CAP ? usageTurns.slice(-TURN_RENDER_CAP) : usageTurns,
  );
  let usageMax = $derived(maxTurnTotal(shownTurns));
  // V24 Phase C: merged same-lane runs for the lane strip under the bars.
  let laneSegs = $derived(laneSegments(shownTurns));

  // ── Chart zoom + horizontal scroll ───────────────────────────────────
  // `zoomCol` is the wheel-chosen column width (px); null = auto, i.e. fill
  // the viewport down to `BAR_MIN_COL` per bar, then overflow into a scroll.
  let ubarsWidth = $state(0); // the scroll viewport's clientWidth
  const BAR_GAP = 3; // must match the .ubars / .salane CSS gap
  const BAR_MIN_COL = 2; // px — a bar column's minimum width before scrolling
  const BAR_MAX_COL = 48; // zoom-in ceiling
  let zoomCol = $state<number | null>(null);
  let ubarsScroll = $state<HTMLDivElement | null>(null);
  // Non-reactive: stick to the right edge (newest turns) as data lands,
  // unless the user scrolled back into history.
  let pinnedRight = true;
  // Zoom is per-session: when the card switches to a DIFFERENT session
  // (drill-in, back-to-live, or the live session rolling over) reset to
  // auto-fit — a column width chosen for one session's turn count means
  // nothing for another's. Non-reactive prev guard = no reset on refetches
  // of the same session.
  let zoomSessionId: string | null | undefined;
  $effect(() => {
    const sid = selectedId ?? usage?.current?.session_id ?? null;
    if (sid === zoomSessionId) return;
    zoomSessionId = sid;
    zoomCol = null;
    pinnedRight = true;
  });
  // Column width at which all shown turns exactly fill the viewport (may be
  // sub-minimum when the session is long — that's the scroll trigger).
  let fitCol = $derived(
    shownTurns.length > 0 && ubarsWidth > 0
      ? (ubarsWidth - BAR_GAP * (shownTurns.length - 1)) / shownTurns.length
      : 0,
  );
  // The fill-the-card floor applies to a REMEMBERED zoom too: a zoomCol
  // picked on a long session can sit below the fit width of a shorter one
  // selected later (or below the new fit after a window resize) — without
  // this clamp that stale zoom leaves the chart narrower than the card.
  let colPx = $derived(Math.max(zoomCol ?? 0, fitCol, BAR_MIN_COL));
  // Fixed-width columns (scroll mode) whenever the user zoomed or the fit
  // width dropped below the minimum; otherwise columns flex to fill. fitCol
  // goes NEGATIVE once the inter-bar gaps alone exceed the viewport (very
  // long sessions), so "measured" is length+width — not fitCol > 0, which
  // dropped the explicit chart/lane widths at full zoom-out and let the S/A
  // lane collapse to the viewport while the bars still overflowed.
  let fixedCols = $derived(
    zoomCol !== null || (shownTurns.length > 0 && ubarsWidth > 0 && fitCol < BAR_MIN_COL),
  );
  let chartWidthPx = $derived(
    shownTurns.length > 0 ? colPx * shownTurns.length + BAR_GAP * (shownTurns.length - 1) : 0,
  );
  let laneWidthPx = $derived(fixedCols ? chartWidthPx : ubarsWidth);
  let chartScrollable = $derived(fixedCols && chartWidthPx > ubarsWidth + 1);

  function onChartScroll(): void {
    const el = ubarsScroll;
    if (!el) return;
    pinnedRight = el.scrollLeft + el.clientWidth >= el.scrollWidth - 4;
  }

  function chartWheel(e: WheelEvent): void {
    // Shift+wheel keeps the browser's native horizontal pan; plain wheel
    // zooms the bar width around the cursor.
    if (e.shiftKey || shownTurns.length === 0) return;
    const el = ubarsScroll;
    if (!el || ubarsWidth <= 0) return;
    e.preventDefault();
    const minCol = Math.max(fitCol, BAR_MIN_COL); // zoom-out floor: fill-the-card
    const maxCol = Math.max(BAR_MAX_COL, minCol);
    const factor = e.deltaY < 0 ? 1.25 : 0.8;
    const next = Math.min(maxCol, Math.max(minCol, colPx * factor));
    if (Math.abs(next - colPx) < 0.01) return;
    // Anchor the turn under the cursor while the chart width changes.
    const mouseX = e.clientX - el.getBoundingClientRect().left;
    const frac = chartWidthPx > 0 ? (el.scrollLeft + mouseX) / chartWidthPx : 1;
    pinnedRight = false; // don't fight the anchor below
    zoomCol = next <= minCol + 0.01 ? null : next;
    const newWidth = shownTurns.length * next + BAR_GAP * (shownTurns.length - 1);
    requestAnimationFrame(() => {
      el.scrollLeft = frac * newWidth - mouseX;
      onChartScroll(); // re-derive pinnedRight from where we actually landed
    });
  }

  // Svelte 5 attaches `onwheel` passively; zooming needs preventDefault, so
  // the listener is attached by hand, non-passive.
  function wheelZoom(node: HTMLElement): { destroy(): void } {
    const h = (e: WheelEvent): void => chartWheel(e);
    node.addEventListener('wheel', h, { passive: false });
    return { destroy: () => node.removeEventListener('wheel', h) };
  }

  // Keep the newest turns in view as new data lands (only while pinned to
  // the right edge — a user reading history isn't yanked forward).
  $effect(() => {
    void shownTurns.length;
    void colPx;
    const el = ubarsScroll;
    if (el && pinnedRight && el.scrollWidth > el.clientWidth) el.scrollLeft = el.scrollWidth;
  });

  // V24 Phase C: the current session's totals row (agent + start time for the
  // live card title) — matched out of the Sessions list by `current`'s id.
  let currentSessionRow = $derived.by(() => {
    const cur = usage?.current;
    if (!cur) return null;
    return usage?.sessions.find((s) => s.session_id === cur.session_id) ?? null;
  });

  // Whether a session is actually live right now (open tab ∪ recency, per
  // `active_session_ids`) — see `isActiveSessionIn`, which the parent's Memory
  // section asks the same question through. Read straight off `usage` here so
  // the answer is never a frame behind the snapshot the row came from.
  function isActiveSession(sid?: string | null): boolean {
    return isActiveSessionIn(usage?.active_session_ids, sid);
  }

  // Chart segment colors: each legend dot is a native color input. The
  // committed value lives in settings (`graph.usage_color_*`, persisted by
  // the backend); `chartPreview` holds the live value while a picker is open
  // (`oninput` fires per drag tick) so the chart recolors immediately without
  // a settings round-trip per tick.
  // `kind` is the PRICING CATEGORY id each segment reads out of a turn's
  // `tokens` map (V40 Phase G) — the same four ids `PriceRates` names its
  // fields after, which is cImp's own vocabulary, not a harness's. `tool` has
  // none: tool-result chars are an estimate cImp derives, never a billed
  // category anybody reported.
  const CHART_SEGS = [
    { key: 'in', kind: 'input', label: 'input', field: 'usage_color_in' },
    { key: 'cache', kind: 'cache_read', label: 'cache-read', field: 'usage_color_cache' },
    { key: 'write', kind: 'cache_write', label: 'cache-write', field: 'usage_color_write' },
    { key: 'out', kind: 'output', label: 'output', field: 'usage_color_out' },
    { key: 'tool', kind: null, label: 'est. tool-result', field: 'usage_color_tool' },
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

  // ── lane colors: the DECLARED order picks the swatch (V40 Phase I) ────────
  //
  // Was `SA_SEGS`, a fixed pair of settings fields (`usage_color_session` /
  // `usage_color_agent`) named after one harness's two lanes — so a harness
  // declaring a THIRD lane got the second lane's legend swatch (`laneSeg`
  // clamped to the last entry) and no `.dseg`/`.saseg` fill rule at all, which
  // painted it SVG-default black in the donut and transparent in the strip.
  // The lane IDS have been the harness's declaration since Phase D; their
  // colors are read off the same declaration now — palette slot N for the Nth
  // lane the harness declares, with `graph.usage_lane_colors[id]` overriding
  // when the user has picked one.
  //
  // Slots 0 and 1 are the exact colors the two retired settings defaulted to,
  // so every lane the shipped harnesses declare keeps the exact colour it
  // had before this change.
  const LANE_PALETTE = ['#30363d', '#3b6ea5', '#7d6b3f', '#4f6f5a', '#6b4f6f', '#3f5f6f'];
  const LANE_OVERFLOW = '#8b8b86';
  let lanePreview = $state<Record<string, string>>({});
  /// Readable text on a user-picked fill. The two retired settings hard-coded
  /// this per lane (`--text-quiet` on the dark session swatch, `#fff` on the
  /// blue agent one), which cannot generalise to a color someone picks for a
  /// lane core has never heard of.
  function laneTextColor(hex: string): string {
    const m = /^#([0-9a-fA-F]{6})$/.exec(hex);
    if (!m) return '#fff';
    const n = parseInt(m[1], 16);
    // Rec. 601 luma, the usual cheap approximation.
    const luma = (0.299 * ((n >> 16) & 255) + 0.587 * ((n >> 8) & 255) + 0.114 * (n & 255)) / 255;
    return luma > 0.6 ? '#111' : '#fff';
  }
  async function commitLaneColor(id: string, value: string): Promise<void> {
    const next = structuredClone($settings);
    next.graph.usage_lane_colors = { ...(next.graph.usage_lane_colors ?? {}), [id]: value };
    // applySettings updates the store optimistically (and rolls back on
    // failure), so the preview can be dropped afterwards either way.
    await applySettings(next);
    delete lanePreview[id];
  }

  // V40 Phase F (locked decision 23): the rule reference is PUBLISHED by the
  // backend now. It used to be a hard-coded string here — a second copy of
  // every threshold `advisor.rs` owns, which a tuning change left lying, with
  // one harness's mechanisms named in it (a product's version, a named hook)
  // for rules that fire per registered harness.
  //
  // What each card says about the FIX still comes from the card: the pointer is
  // `Capability::drift_hint()`, supplied by the harness that raised it, so it
  // can name that harness's mechanism where this reference cannot.
  //
  // Empty until the fetch lands (one paint): the tooltip is a reference, and an
  // absent one is better than a stale one.
  let advisorRulesTooltip = $state('');

  async function loadAdvisorRules(): Promise<void> {
    try {
      const doc = await advisorRules();
      advisorRulesTooltip = [
        ...doc.rules.map((r) => `${r.id}: ${r.thresholds}`),
        doc.footer,
      ].join('\n');
    } catch (e) {
      console.error('advisor_rules failed:', e);
    }
  }

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
  // `pricingTable` / `refreshPricingTable` are the ONE shared price table,
  // declared with the Cost card above (unified so a Settings edit propagates to
  // the toggle, the Sessions rows, and the Cost card together).
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

  // ── V28: Dashboard card — donut data ─────────────────────────────────
  // Token donut: outer ring session|agent, inner ring the four exact token
  // kinds per origin (aligned under their outer arc — same cumulative
  // angles). Sourced from the full turn series of the shown session, so it
  // follows drill-in/live exactly like the other cards.
  const DASH_KIND_SEGS = CHART_SEGS.filter((s) => s.kind !== null);
  // The Cost card's four fixed table columns (Input / Cache write / Cache read
  // / Output) and the Sessions row's four stats read these pricing category
  // ids out of a `TokenKinds`. Fixed at four because cImp's PRICE TABLE has
  // exactly four rates — the columns are its vocabulary, not a harness's — and
  // a category the session's harness did not declare renders 0 here rather
  // than shifting the table out from under the $/MTok row beside it.
  const COST_TABLE_KINDS = ['input', 'cache_write', 'cache_read', 'output'] as const;
  const SESSION_STAT_KINDS = [
    { id: 'input', short: 'in', long: 'input' },
    { id: 'cache_write', short: 'cache-write', long: 'cache-write' },
    { id: 'cache_read', short: 'cache-read', long: 'cache-read' },
    { id: 'output', short: 'out', long: 'output' },
  ] as const;
  // ≈2px surface gap at each ring's mid-radius (viewBox units = px at the
  // rendered size below).
  const DASH_GAP_OUTER = 2 / 54;
  const DASH_GAP_INNER = 2 / 35;
  // The lanes the donut, its legend and the per-model share line iterate:
  // the harness's DECLARED origins once `loadDeclaredOrigins` has answered,
  // else whatever lanes the DATA actually carries. Never a hard-coded pair —
  // that closed shape was this file's half of the ledger row.
  const dashLaneIds = $derived.by(() => {
    if (declaredOrigins.length > 0) return declaredOrigins.map((o) => o.id);
    return [...new Set(usageTurns.map((t) => t.origin))].sort();
  });
  const dashKinds = $derived(originKindTotals(usageTurns, dashLaneIds));
  // Every lane the donut can draw = declared ∪ present-in-data, so a stored
  // row is never dropped because its harness has not answered yet.
  const dashLaneKeys = $derived(Object.keys(dashKinds));
  const dashLaneTok = $derived(
    Object.fromEntries(dashLaneKeys.map((id) => [id, kindsTotal(dashKinds[id])])),
  );
  const dashTokenTotal = $derived(
    dashLaneKeys.reduce((sum, id) => sum + dashLaneTok[id], 0),
  );
  const dashOuterArcs = $derived(
    donutArcs(
      dashLaneKeys.map((id) => ({ key: id, value: dashLaneTok[id] })),
      DASH_GAP_OUTER,
    ),
  );
  // One declared category's tokens within one lane. `?? 0` is right here and
  // only here: a segment the harness never declared draws nothing, which is
  // the same pixel as a zero — the ABSENCE is preserved in the legend, which
  // iterates the declared list, not this lookup.
  function dashKindValue(kinds: Record<string, number>, kind: string | null): number {
    return kind === null ? 0 : (kinds[kind] ?? 0);
  }
  const dashInnerArcs = $derived(
    donutArcs(
      dashLaneKeys.flatMap((o) =>
        DASH_KIND_SEGS.map((s) => ({
          key: `${o}:${s.key}`,
          value: dashKindValue(dashKinds[o] ?? {}, s.kind),
        })),
      ),
      DASH_GAP_INNER,
    ),
  );
  /// The lanes the donut, its legend and the per-model share line print — as
  /// the harnesses DECLARE them, in declared order.
  ///
  /// **V40 Phase F (locked decision 19)** made the lane NAMES declared: the
  /// `session | agent` split is one harness's sidechain model, and a harness
  /// that attributes turns some other way had no way to say so. **V40 Phase G
  /// removed the two-lane SHAPE with it** — the payload carried
  /// `OriginSplit { session_tok, agent_tok }`, a closed pair, so a harness with
  /// one lane rendered a fabricated second at 0 and one with three had nowhere
  /// to put the third. `ModelUsage.origins` is a per-lane map now and this list
  /// is what iterates it. A lane no harness declares still renders under its own
  /// id, the same posture every other unknown gets.
  /// Each harness's declared turn lanes, **keyed by harness** (V40 review
  /// finding F-4).
  ///
  /// This was one flat list, unioned across the roster, first-id-wins, rendered
  /// for every session. Two consequences, both latent only because the two
  /// shipped harnesses declare byte-identical origins — and V40 exists to make
  /// a third one easy:
  ///
  /// * a lane only harness B declares became a permanent `0 tok` row on every
  ///   harness-A session's Cost card and an empty ring in the donut legend — a
  ///   lane that collects nothing, asserted as a real zero, which is exactly
  ///   the absent-vs-zero distinction Phase G's payload change preserves;
  /// * two harnesses declaring the same lane id with different `subagent`
  ///   flags meant the first one's flag decided for both, so `subagentOrigins`
  ///   painted the outline and the `A` badge on the other harness's bars.
  let declaredOriginsByHarness = $state<Record<string, DeclaredOrigin[]>>({});
  /// The harness whose session the usage surface is currently showing — the
  /// pinned row's, else the live session's row in the sessions list. `''` when
  /// nothing is selected and the live session has no row yet, which degrades
  /// the lanes to whatever the DATA carries (bare ids), never to another
  /// harness's wording.
  const dashHarnessId = $derived.by(() => {
    if (selectedRow) return selectedRow.agent;
    const sid = usage?.current?.session_id ?? '';
    if (!sid) return '';
    return usage?.sessions.find((r) => r.session_id === sid)?.agent ?? '';
  });
  const declaredOrigins = $derived<DeclaredOrigin[]>(
    declaredOriginsByHarness[dashHarnessId] ?? [],
  );
  const DASH_ORIGIN_LABEL = $derived(
    Object.fromEntries(declaredOrigins.map((o) => [o.id, o.label])) as Record<string, string>,
  );
  /// The lanes `originShareLine` prints, with their declared labels. Falls
  /// back to the lanes present in the data so a share line still renders before
  /// the declaration lands (bare ids, never another harness's wording).
  const dashShareLanes = $derived(
    declaredOrigins.length > 0
      ? declaredOrigins.map((o) => ({ id: o.id, label: o.label }))
      : dashLaneIds.map((id) => ({ id })),
  );
  /// The declared lanes that carry FAN-OUT spend — what marks a chart bar as a
  /// sub-agent turn. The harness's own statement (`origins[].subagent`), never
  /// the word "agent".
  const subagentOrigins = $derived(declaredOrigins.filter((o) => o.subagent).map((o) => o.id));

  /// Ask each harness what its recorded turns look like, once.
  ///
  /// V40 Phase G: asked of EVERY harness in the roster, not just those with a
  /// `session_usage`-shaped quota answer, and read off `harness_usage`'s
  /// top-level `origins` rather than out of `source` — a harness can record
  /// turns without reporting quota (one shipped harness does exactly that), and
  /// nesting the declaration under `source` is why its sessions' lanes had no
  /// labels at all. Best-effort in both directions: a harness that declares no turn shape
  /// contributes nothing, and a failed call leaves the list as it is — the
  /// labels degrade to bare ids, never to another harness's wording.
  // Driven by the roster rather than by mount: `harness_list` and this
  // component's mount race, and an empty roster would leave every lane labelled
  // with its bare id. Re-runs once, when the list arrives.
  let originsAsked = false;
  $effect(() => {
    if (originsAsked || $harnesses.length === 0) return;
    originsAsked = true;
    void loadDeclaredOrigins();
  });

  async function loadDeclaredOrigins(): Promise<void> {
    const next: Record<string, DeclaredOrigin[]> = {};
    for (const h of get(harnesses)) {
      try {
        const answer = await harnessUsage(h.id);
        // Kept under the harness that DECLARED them (V40 review F-4). A
        // harness that declares nothing gets no key, and its sessions fall back
        // to the lanes present in the data — which is the honest answer, not a
        // neighbour's lane list.
        if (answer.origins.length > 0) next[h.id] = answer.origins;
      } catch (e) {
        console.warn('harness_usage origins failed:', e);
      }
    }
    if (Object.keys(next).length > 0) declaredOriginsByHarness = next;
  }
  const DASH_KIND_LABEL = Object.fromEntries(DASH_KIND_SEGS.map((s) => [s.key, s.label])) as Record<
    string,
    string
  >;
  /// A lane's palette slot: its position in the harness's DECLARED order.
  ///
  /// A lane that appears only in stored data — its harness has not answered
  /// `harness_usage` yet, or the row predates the declaration — sorts after
  /// every declared one rather than stealing slot 0, so a late IPC answer
  /// cannot recolor a declared lane out from under the reader.
  const laneSlot = $derived((id: string): number => {
    const at = declaredOrigins.findIndex((o) => o.id === id);
    return at >= 0 ? at : declaredOrigins.length + Math.max(0, dashLaneKeys.indexOf(id));
  });
  /// The color a lane paints with: the user's pick if there is one, else the
  /// palette slot for its declared position.
  const laneColor = $derived(
    (id: string): string =>
      lanePreview[id] ??
      $settings.graph.usage_lane_colors?.[id] ??
      LANE_PALETTE[laneSlot(id)] ??
      LANE_OVERFLOW,
  );
  /// The sub-agent bars' outline color. Was `usage_color_agent` — a settings
  /// field named after one harness's fan-out lane; it is the first lane the
  /// harness
  /// DECLARES `subagent: true` for now, and falls back to the accent when the
  /// harness declares none (a harness with no fan-out has no sub-agent bars to
  /// outline, so the value is inert rather than wrong).
  const subagentLaneColor = $derived(
    subagentOrigins.length > 0 ? laneColor(subagentOrigins[0]) : 'var(--accent, #3b6ea5)',
  );
  // An inner arc's "session:cache"-style key split back into its parts for
  // the tooltip / fill class.
  function dashInnerParts(key: string): { origin: string; kind: string } {
    const i = key.indexOf(':');
    return { origin: key.slice(0, i), kind: key.slice(i + 1) };
  }
  // The token donut's legend: one row per origin, each with its kind split.
  // The row's color picker writes `graph.usage_lane_colors[origin]` — the same
  // per-lane store the This-session legend writes, so a pick in either card
  // recolors both, for any number of lanes.
  const dashLegendRows = $derived(
    dashLaneKeys.map((id) => ({
      origin: id,
      tok: dashLaneTok[id],
      kinds: dashKinds[id] ?? {},
    })),
  );

  // Cost donut: per-model share of the session's estimated cost, at the same
  // resolved rates as the Cost card (overrides included) — the two can never
  // disagree. A model with no price match prices at $0 (no arc); the legend
  // marks it "no price" instead of hiding it.
  const dashCostRows = $derived(
    costPerModel.map((m) => {
      const st = costRowByModel[m.model];
      return {
        model: m.model,
        cost: sessionCost(m.totals, st.rates).total,
        tokens: kindsTotal(m.totals),
        unpriced: st.matchedRow === null && !costOverrideByModel[m.model],
      };
    }),
  );
  const dashCostArcs = $derived(
    donutArcs(
      dashCostRows.map((r) => ({ key: r.model, value: r.cost })),
      DASH_GAP_OUTER,
    ),
  );

  // Model → categorical slot, assigned first-seen and never re-ranked, so a
  // model keeps its hue as shares shift between polls (color follows the
  // entity). Non-reactive on purpose: assignment happens during render and
  // must not invalidate it. Slots validated for the dark card surface
  // (dataviz six-checks); models past the 6th share the overflow gray —
  // realistic sessions carry 2–4 models.
  const DASH_MODEL_COLORS = ['#3987e5', '#d95926', '#199e70', '#c98500', '#d55181', '#9085e9'];
  const DASH_MODEL_OVERFLOW = '#8b8b86';
  const dashModelSlot = new Map<string, number>();
  // User overrides on top of the slot palette, keyed by model id. Unlike the
  // token segment colors (fixed settings fields), model ids are dynamic, so
  // these live in the localStorage view prefs — a per-machine view choice,
  // same posture as the other viewSection state. Invalid entries are dropped
  // at load so a corrupt pref can never paint a non-color.
  const dashModelColors = $state<Record<string, string>>(loadDashModelColors());
  function loadDashModelColors(): Record<string, string> {
    try {
      const parsed: unknown = JSON.parse(
        loadViewString('code-intelligence', 'dash-model-colors') ?? '{}',
      );
      const out: Record<string, string> = {};
      if (parsed && typeof parsed === 'object') {
        for (const [k, v] of Object.entries(parsed)) {
          if (typeof v === 'string' && /^#[0-9a-fA-F]{6}$/.test(v)) out[k] = v;
        }
      }
      return out;
    } catch {
      return {};
    }
  }
  $effect(() =>
    saveViewString('code-intelligence', 'dash-model-colors', JSON.stringify(dashModelColors)),
  );
  function dashModelColor(model: string): string {
    const over = dashModelColors[model];
    if (over) return over;
    let i = dashModelSlot.get(model);
    if (i === undefined) {
      i = dashModelSlot.size;
      dashModelSlot.set(model, i);
    }
    return DASH_MODEL_COLORS[i] ?? DASH_MODEL_OVERFLOW;
  }

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
    advisorBusy = proposalKey(p);
    try {
      // A `mark_verified` card always carries the harness it is about; a card
      // without one has nothing to stamp, and stamping the default harness
      // instead is exactly the misattribution Phase C's field removed.
      if (!p.harness) return;
      await harnessMarkVerified(p.harness);
      dropProposal(p);
    } catch (e) {
      console.error('harness_mark_verified failed', e);
    } finally {
      advisorBusy = null;
    }
  }

  // Open/collapsed state of the Overview usage cards — native <details>
  // elements, so a remount would otherwise snap them back to collapsed.
  // Effectiveness was an always-open <section> until V28 — open stays the
  // default so the counters remain the at-a-glance readout they were.
  let effCardOpen = $state(loadCardOpen('code-intelligence', 'usage-effectiveness', true));
  $effect(() => saveCardOpen('code-intelligence', 'usage-effectiveness', effCardOpen));
  let sessionCardOpen = $state(loadCardOpen('code-intelligence', 'usage-this-session'));
  let advisorCardOpen = $state(loadCardOpen('code-intelligence', 'usage-advisor'));
  let sessionsCardOpen = $state(loadCardOpen('code-intelligence', 'usage-sessions'));
  $effect(() => saveCardOpen('code-intelligence', 'usage-this-session', sessionCardOpen));
  $effect(() => saveCardOpen('code-intelligence', 'usage-advisor', advisorCardOpen));
  $effect(() => saveCardOpen('code-intelligence', 'usage-sessions', sessionsCardOpen));

  /// The parent's keep-alive poll drives the Overview through here — there is
  /// no second timer in this component. `force` is the user-initiated path (a
  /// section switch), which bypasses the cadence gate; the timer passes false.
  export async function tick(force: boolean): Promise<void> {
    if (force || usageDue()) await refreshUsage();
  }

  // Pricing edits are saved from the Settings window straight to the global
  // file (bypassing the settings-changed broadcast), so `llm_pricing_set`
  // emits its own event: refetch the shared table so already-open cost
  // surfaces reprice without a card reopen or an app restart.
  listenManaged(() => listen('llm-pricing-changed', () => void refreshPricingTable()));

  onMount(() => {
    // Static backend data, once — see `loadAdvisorRules`.
    void loadAdvisorRules();
  });

  onDestroy(() => {
    unsubActiveTab();
  });
</script>

{#if active}
  <h3 class="group-head">Usage</h3>
  <div class="usage-sec">
    <!-- V28: Dashboard — the Overview's at-a-glance donuts. Left: session vs
         sub-agent token spend (outer ring) over its per-origin kind split
         (inner ring, aligned under its origin's arc). Right: est. cost share
         per model, priced at the Cost card's resolved rates so the two can
         never disagree. Segment colors flow from the same settings-backed
         CSS vars as the stacked-bar chart — the legend pickers here and in
         the This-session card write the same settings, so recoloring in
         either card recolors both. Model colors are per-machine view prefs
         (dynamic keys don't fit fixed settings fields). -->
    <details
      class="card"
      bind:open={dashCardOpen}
      style="--ubar-in: {chartColors.in}; --ubar-cache: {chartColors.cache}; --ubar-write: {chartColors.write}; --ubar-out: {chartColors.out}; --sa-agent: {subagentLaneColor}"
    >
      <summary class="history-head">
        Dashboard
        {#if selectedRow}
          <span class="muted">· {selectedRow.agent} · <code>{selectedRow.session_id.slice(0, 8)}</code>…</span>
        {:else}
          <span class="muted">· this session</span>
        {/if}
      </summary>
      <div class="donuts">
        <div class="donut-block">
          <div class="donut-title">Tokens · session vs sub-agents</div>
          {#if dashTokenTotal === 0}
            <p class="placeholder">
              {selectedSession ? 'No usage recorded for this session.' : 'No usage recorded yet this session.'}
            </p>
          {:else}
            <div class="donut-row">
              <svg
                class="donut"
                viewBox="0 0 132 132"
                role="img"
                aria-label="Session vs sub-agent token usage"
              >
                {#each dashOuterArcs as a (a.key)}
                  <path
                    class="dseg"
                    style="fill: {laneColor(a.key)}"
                    d={arcPath(66, 66, 62, 46, a.a0, a.a1)}
                  >
                    <title
                      >{DASH_ORIGIN_LABEL[a.key] ?? a.key}: {a.value.toLocaleString()} tok · {fmtPct(
                        a.share,
                      )}</title
                    >
                  </path>
                {/each}
                {#each dashInnerArcs as a (a.key)}
                  {@const p = dashInnerParts(a.key)}
                  <path class="dseg {p.kind}" d={arcPath(66, 66, 42, 28, a.a0, a.a1)}>
                    <title
                      >{DASH_ORIGIN_LABEL[p.origin] ?? p.origin} {DASH_KIND_LABEL[p.kind]}: {a.value.toLocaleString()} tok
                      · {fmtPct(a.share)}</title
                    >
                  </path>
                {/each}
                <text class="donut-num" x="66" y="63">{fmtTok(dashTokenTotal)}</text>
                <text class="donut-sub" x="66" y="77">tokens</text>
              </svg>
              <div class="donut-legend">
                {#each dashLegendRows as r (r.origin)}
                  <div class="dl-head">
                    <input
                      type="color"
                      class="dot"
                      value={laneColor(r.origin)}
                      title="{DASH_ORIGIN_LABEL[r.origin] ?? r.origin} — click to pick a color (shared with the This-session chart)"
                      oninput={(e) => (lanePreview[r.origin] = e.currentTarget.value)}
                      onchange={(e) => commitLaneColor(r.origin, e.currentTarget.value)}
                    />
                    <span class="dl-name">{DASH_ORIGIN_LABEL[r.origin] ?? r.origin}</span>
                    <span class="tnum" title="{r.tok.toLocaleString()} tokens">{fmtTok(r.tok)}</span>
                    <span class="muted">{fmtPct(dashTokenTotal > 0 ? r.tok / dashTokenTotal : 0)}</span>
                  </div>
                  <div class="dl-kinds">
                    {#each DASH_KIND_SEGS as s (s.key)}
                      {@const v = dashKindValue(r.kinds, s.kind)}
                      <span class="dl-kind" title="{s.label}: {v.toLocaleString()} tokens">
                        <input
                          type="color"
                          class="dot sm"
                          value={chartColors[s.key]}
                          title="{s.label} — click to pick a color (shared with the This-session chart)"
                          oninput={(e) => (chartPreview[s.key] = e.currentTarget.value)}
                          onchange={(e) => commitChartColor(s, e.currentTarget.value)}
                        />{fmtTok(v)}
                      </span>
                    {/each}
                  </div>
                {/each}
              </div>
            </div>
          {/if}
        </div>

        <div class="donut-block">
          <div class="donut-title">Est. cost · by model</div>
          {#if dashCostRows.length === 0}
            <p class="placeholder">
              {selectedSession
                ? 'No per-model usage recorded for this session.'
                : 'No per-model usage recorded yet this session.'}
            </p>
          {:else if pricingTable === null}
            <p class="placeholder">loading prices…</p>
          {:else if dashCostRows.every((r) => r.unpriced)}
            <!-- rc.9 live-verify (#100 item 23, a session on a local model): when
                 NOTHING in the session is priced, a donut with no arcs, a
                 legend of $0.0000 · 0% rows and a badge per model says the same
                 thing four ways and the empty-donut prose overflowed the card.
                 One sentence, naming the models, is the whole message. -->
            <p class="placeholder">
              No price data for model{dashCostRows.length === 1 ? '' : 's'}:
              {#each dashCostRows as r, i (r.model)}{#if i > 0}, {/if}<code>{r.model}</code>{/each}
              (Settings → LLM pricing).
            </p>
          {:else}
            <div class="donut-row">
              {#if costGrand > 0}
                <svg
                  class="donut"
                  viewBox="0 0 132 132"
                  role="img"
                  aria-label="Estimated cost share by model"
                >
                  {#each dashCostArcs as a (a.key)}
                    <path fill={dashModelColor(a.key)} d={arcPath(66, 66, 62, 40, a.a0, a.a1)}>
                      <title>{a.key}: {fmtUsd(a.value)} · {fmtPct(a.share)}</title>
                    </path>
                  {/each}
                  <text class="donut-num" x="66" y="63">{fmtUsd(costGrand)}</text>
                  <text class="donut-sub" x="66" y="77">est. total</text>
                </svg>
              {:else}
                <p class="placeholder donut-empty">
                  No price rows match these models — add rates in Settings → LLM pricing, or pick
                  them per model in the Cost card below.
                </p>
              {/if}
              <div class="donut-legend">
                {#each dashCostRows as r (r.model)}
                  <div class="dl-head">
                    <input
                      type="color"
                      class="dot"
                      value={dashModelColor(r.model)}
                      title="{r.model} — click to pick a color"
                      oninput={(e) => (dashModelColors[r.model] = e.currentTarget.value)}
                    />
                    <code class="dl-model" title="{r.model} · {r.tokens.toLocaleString()} tokens">{r.model}</code>
                    <span class="tnum">{fmtUsd(r.cost)}</span>
                    <span class="muted">{fmtPct(costGrand > 0 ? r.cost / costGrand : 0)}</span>
                    {#if r.unpriced}<span class="est-badge">no price match</span>{/if}
                  </div>
                {/each}
              </div>
            </div>
          {/if}
        </div>
      </div>
    </details>

    <!-- Effectiveness: measured counters, never fabricated savings. -->
    <details class="card" bind:open={effCardOpen}>
      <summary class="history-head">Effectiveness</summary>
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
            <span class="lbl">tasks served locally — see <em>Tools → Offload server</em></span>
          </div>
          <div>
            <span
              class="num"
              title="Serialized size of the graph tool descriptors advertised to the cloud session and the offload worker — cache-written once per session. Toggle Settings → Code Intelligence → Code graph → Lean tool surface to trim the cold-tail tools."
            >{usage.surface.mcp_tools.toLocaleString()}</span>
            <span class="lbl">tool surface: {usage.surface.mcp_chars.toLocaleString()} chars, cache-written once per session
              <span class="est-badge">est. ~{Math.round(usage.surface.mcp_chars / 4).toLocaleString()} tok</span>
            </span>
          </div>
        </div>
      {/if}
    </details>

    <!-- This session: per-turn stacked bars + top consumers. The segment
         colors flow from settings (via the legend's color pickers) into CSS
         vars scoped to this card. -->
    <details
      class="card"
      bind:open={sessionCardOpen}
      style="--ubar-in: {chartColors.in}; --ubar-cache: {chartColors.cache}; --ubar-write: {chartColors.write}; --ubar-out: {chartColors.out}; --ubar-tool: {chartColors.tool}; --sa-agent: {subagentLaneColor}"
    >
      <summary class="history-head">
        {#if selectedRow}
          <!-- Selected mode: identify the session — agent, start time, an
               8-char id prefix, a copy-full-id button, and a Live pill back
               to the current session. Title fields
               come from the CLICKED row (always populated), not the fetched
               detail, so they're robust regardless of the detail's `row`. -->
          <span class="card-title"
            >{following ? 'Focused tab' : 'Session'} · {selectedRow.agent} · {fmtDate(
              selectedRow.started_ms,
            )}
            {fmtTime(selectedRow.started_ms)} ·
            <code>{selectedRow.session_id.slice(0, 8)}</code>…</span
          >
          <!-- V34: say WHY this session is on screen. Following = it tracks
               whichever agent tab has focus; pinned = the user picked this row
               and it stays put until they go Live. Without this the two modes
               look identical and a card that changed under a tab switch reads
               as a glitch. -->
          {#if following}
            <span class="muted" title="Tracking whichever agent tab has focus">follows focus</span>
          {/if}
          <button
            type="button"
            class="mini secondary"
            title="Copy the full session id"
            onclick={copySessionId}>{copiedId ? 'copied' : 'copy id'}</button
          >
          <button
            type="button"
            class="mini live-pill"
            title="Return to the live current session"
            onclick={goLive}>Live</button
          >
        {:else}
          <span class="card-title"
            >{usage?.current && !isActiveSession(usage.current.session_id)
              ? 'Last session'
              : 'This session'}{#if currentSessionRow} · {currentSessionRow.agent} · {fmtDate(
                currentSessionRow.started_ms,
              )} {fmtTime(currentSessionRow.started_ms)}{/if}</span
          >
        {/if}
        {#if shownTurns.length < usageTurns.length}
          <span class="muted">(last {shownTurns.length} of {usageTurns.length} turns)</span>
        {/if}
      </summary>
      {#if usageTurns.length === 0 && cardTopTools.length === 0}
        <p class="placeholder">
          {selectedSession ? 'No usage recorded for this session.' : 'No usage recorded yet this session.'}
        </p>
      {:else}
        {#if selectedRow}
          <!-- Full session id, select-all for easy copying (the title shows
               only an 8-char prefix). -->
          <div class="session-id-hint muted">
            session id:
            <code>{selectedRow.session_id}</code>
          </div>
        {/if}
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
          <!-- V24 Phase C: the lane key — color pickers too, persisted like the
               segment colors (the sub-agent lane's color also tints the
               sub-agent bars' outline). V40 Phase I: ONE ROW PER DECLARED LANE
               rather than a hard-coded S/A pair, so a harness with a third lane
               gets a third swatch instead of sharing the second's. -->
          {#each dashLaneKeys as id (id)}
            <span>
              <input
                type="color"
                class="dot"
                value={laneColor(id)}
                title="{DASH_ORIGIN_LABEL[id] ?? id} — click to pick a color"
                oninput={(e) => (lanePreview[id] = e.currentTarget.value)}
                onchange={(e) => commitLaneColor(id, e.currentTarget.value)}
              />
              {laneLabel(id)}
              {DASH_ORIGIN_LABEL[id] ?? id}
            </span>
          {/each}
          {#if shownTurns.length > 1}
            <span class="zoom-hint">wheel: zoom · shift+wheel: pan</span>
          {/if}
        </div>
        <!-- Wheel = zoom (bar width), shift+wheel = pan; the wrapper scrolls
             horizontally once bars hit their minimum width, so long sessions
             keep every turn reachable instead of dropping the oldest. -->
        <div
          class="ubars-scroll"
          class:scrollable={chartScrollable}
          bind:this={ubarsScroll}
          bind:clientWidth={ubarsWidth}
          onscroll={onChartScroll}
          use:wheelZoom
        >
          <div class="ubars" style={fixedCols ? `width: ${chartWidthPx}px` : undefined}>
            {#each shownTurns as t, i (i)}
              {@const est_tool = Math.round(t.tool_chars / 4)}
              {@const cost = costActive && currentRates ? turnCost(t, currentRates) : null}
              {@const total = cost ? cost.total : turnTotal(t)}
              {@const max = cost ? usageCostMax : usageMax}
              {@const turnNo = usageTurns.length - shownTurns.length + i + 1}
              <div class="ubar-col" style={fixedCols ? `flex: 0 0 ${colPx}px` : undefined}>
                <div
                  class="ubar {agentBarClass(t.origin, subagentOrigins)}"
                  style="height: {barHeightPct(total, max)}%"
                  title={cost
                    ? `turn ${turnNo} (est.): ${fmtUsd(cost.input)} in / ${fmtUsd(cost.cache_read)} cache-read / ${fmtUsd(cost.cache_write)} cache-write / ${fmtUsd(cost.output)} out / ${fmtUsd(cost.tool)} est. tool — ${fmtUsd(cost.total)}`
                    : `turn ${turnNo}: ${DASH_KIND_SEGS.map((s) => `${t.tokens[s.kind!] ?? 0} ${s.label}`).join(' / ')} / ~${est_tool} est. tool`}
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
                      : [...DASH_KIND_SEGS.map((s) => t.tokens[s.kind!] ?? 0), est_tool]}
                    {#each CHART_SEGS as s, si (s.key)}
                      <span class="useg {s.key}" style="flex-grow: {segs[si]}"></span>
                    {/each}
                  {/if}
                </div>
              </div>
            {/each}
          </div>

          <!-- V24 Phase C: S/A lane — one segment per contiguous same-origin
               run, width proportional to the turns it spans (same per-bar width
               logic as the chart). The letter shows only when the segment is
               ≥~2 chars wide; otherwise the tooltip carries it. Lives inside
               the scroll wrapper so it pans/zooms with the bars. -->
          {#if shownTurns.length > 0}
            <div class="salane" style={fixedCols ? `width: ${chartWidthPx}px` : undefined}>
              {#each laneSegs as seg, i (i)}
                <span
                  class="saseg"
                  style="flex-grow: {seg.count}; background: {laneColor(
                    seg.origin,
                  )}; color: {laneTextColor(laneColor(seg.origin))}"
                  title="{DASH_ORIGIN_LABEL[seg.origin] ?? seg.origin} · {seg.count} turn{seg.count ===
                  1
                    ? ''
                    : 's'}"
                  >{#if laneLabelVisible(seg.count, shownTurns.length, laneWidthPx)}{seg.label}{/if}</span
                >
              {/each}
            </div>
          {/if}
        </div>

        <div class="history-head">Top consumers</div>
        {#if cardTopTools.length === 0}
          <p class="placeholder">No tool-result usage recorded yet.</p>
        {:else}
          <div class="rows scroll5">
            {#each cardTopTools as t (t.tool)}
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

    <!-- V24 Phase D: Cost card — per-model what-if pricing. One row per model
         (tokens-desc as delivered), each priced by its own select: an
         auto-matched table row, hand-typed Custom rates, or Free ($0). The
         popup this replaces priced the whole session at ONE rate set. -->
    <details class="card" bind:open={costCardOpen}>
      <summary class="history-head">
        Cost
        {#if selectedRow}
          <span class="muted">· {selectedRow.agent} · <code>{selectedRow.session_id.slice(0, 8)}</code>…</span>
        {:else}
          <span class="muted">· this session</span>
        {/if}
      </summary>
      {#if costPerModel.length === 0}
        <p class="placeholder">
          {selectedSession
            ? 'No per-model usage recorded for this session.'
            : 'No per-model usage recorded yet this session.'}
        </p>
      {:else}
        <div class="costrows">
          {#each costPerModel as m (m.model)}
            {@const st = costRowByModel[m.model]}
            {@const rates = st.rates}
            {@const c = sessionCost(m.totals, rates)}
            <div class="costrow">
              <div class="costrow-head">
                <span class="cm-model">
                  <code>{m.model || '(no model id)'}</code>
                  {#if st.matchedRow}<span class="cm-provider">{st.matchedRow.provider}</span>{/if}
                </span>
                <select
                  class="cm-pick"
                  aria-label="Pricing for {m.model || 'this model'}"
                  value={st.selIdx}
                  onchange={(e) =>
                    (costOverrideByModel[m.model] = costOverrideForIdx(+e.currentTarget.value, pricingRows))}
                >
                  {#each pricingRows as p, i (i)}
                    <option value={i}>{p.provider} — {p.model}</option>
                  {/each}
                  <option value={pricingRows.length}>Custom…</option>
                  <option value={pricingRows.length + 1}>Free ($0)</option>
                </select>
              </div>
              <!-- Secondary line: this model's token share per declared lane. -->
              <div class="cm-share muted">{originShareLine(m.origins, dashShareLanes)}</div>
              {#if st.selIdx === pricingRows.length && costCustomByModel[m.model]}
                <div class="cost-custom">
                  <label><span>Input $/MTok</span><input type="number" min="0" step="0.01" bind:value={costCustomByModel[m.model].input} /></label>
                  <label><span>Cache write $/MTok</span><input type="number" min="0" step="0.01" bind:value={costCustomByModel[m.model].cache_write} /></label>
                  <label><span>Cache read $/MTok</span><input type="number" min="0" step="0.01" bind:value={costCustomByModel[m.model].cache_read} /></label>
                  <label><span>Output $/MTok</span><input type="number" min="0" step="0.01" bind:value={costCustomByModel[m.model].output} /></label>
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
                    <th>Tokens</th>
                    {#each COST_TABLE_KINDS as k (k)}
                      {@const v = m.totals[k] ?? 0}
                      <td title={v.toLocaleString()}>{fmtTok(v)}</td>
                    {/each}
                  </tr>
                  <tr>
                    <th>$ / MTok</th>
                    <td>{rates.input}</td>
                    <td>{rates.cache_write}</td>
                    <td>{rates.cache_read}</td>
                    <td>{rates.output}</td>
                  </tr>
                  <tr>
                    <th>Cost</th>
                    <td>{fmtUsd(c.input)}</td>
                    <td>{fmtUsd(c.cache_write)}</td>
                    <td>{fmtUsd(c.cache_read)}</td>
                    <td>{fmtUsd(c.output)}</td>
                  </tr>
                </tbody>
              </table>
              <div class="cm-subtotal">Subtotal <b>{fmtUsd(c.total)}</b></div>
            </div>
          {/each}
        </div>
        <div class="cost-foot">
          <span>Total across {costPerModel.length} model{costPerModel.length === 1 ? '' : 's'}</span>
          <b>{fmtUsd(costGrand)}</b>
        </div>
        <small class="cost-hint">Prices are editable in Settings → LLM pricing.</small>
      {/if}
    </details>

    <!-- V14 Phase D2: Advisor card. -->
    <details class="card advisor" bind:open={advisorCardOpen}>
      <summary class="history-head">
        Budget-tuning advisor
        <span class="muted" title={advisorRulesTooltip}>ⓘ rules</span>
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
          {#each advice.proposals as p (proposalKey(p))}
            <div class="proposal">
              <div class="prop-head">
                <span class="aname">{p.setting || p.capability || p.rule_id}</span>
                <span class="prop-vals"><code>{p.current}</code> → <code>{p.proposed}</code></span>
              </div>
              <p class="prop-rationale">{p.rationale}</p>
              <div class="prop-actions">
                {#if p.action === 'mark_verified'}
                  <button
                    class="mini"
                    disabled={advisorBusy !== null}
                    title="Stamp the currently-seen {harnessLabel(p.harness)} version as verified — do this AFTER re-running the MAINTENANCE.md contract checks"
                    onclick={() => markVerified(p)}
                  >{advisorBusy === proposalKey(p) ? 'Marking…' : 'Mark verified'}</button>
                {:else if !p.warn_only}
                  <button
                    class="mini"
                    disabled={advisorBusy !== null}
                    onclick={() => applyProposal(p)}
                  >{advisorBusy === proposalKey(p) ? 'Applying…' : 'Apply'}</button>
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
      {#if sessionNotice}
        <!-- Transient notice, e.g. a clicked session whose data has since
             vanished (empty-detail guard) — clears itself after a few seconds. -->
        <p class="placeholder notice">{sessionNotice}</p>
      {/if}
      {#if !usage || usage.sessions.length === 0}
        <p class="placeholder">No sessions recorded yet.</p>
      {:else}
        <div class="rows scroll10">
          {#each usage.sessions as s (s.session_id)}
            {@const nCommits = commitCounts[s.session_id] ?? 0}
            {@const hit = cacheHitRatio(s.totals)}
            {@const rowState = sessionRowState(s.session_id, selectedId, usage.active_session_ids)}
            <div class="sessrow-wrap">
              <button
                type="button"
                class="arow sessrow {rowState.selected ? 'selected' : ''} {rowState.active ? 'active' : ''}"
                title={`${SESSION_STAT_KINDS.map((k) => `${(s.totals[k.id] ?? 0).toLocaleString()} ${k.long}`).join(' · ')} tokens — click to view this session's usage`}
                onclick={() => void selectSession(s)}
              >
                <span class="aname">{#if rowState.active}<span class="active-dot" title="active now" aria-label="active now"></span>{/if}{s.agent}{#if s.est_only}<span class="est-badge" title="No real token data for this session — chars-only estimate">est</span>{/if}<span class="sess-date">{fmtDate(s.started_ms)}</span></span>
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
                            ? `Mixed models — this whole-session estimate is priced at ${s.models[0]}'s rates (its top consumer by tokens); the other models' tokens are mispriced. Click the row, then open the Cost card for exact per-model pricing.`
                            : 'Estimated from the auto-matched price row'}
                        >{s.models.length > 1 ? 'est · mixed' : 'est'}</span></span>
                    </span>
                  {:else}
                    <span class="sess-stats tnum muted" title="No price row auto-matches this session's model — add a model_prefix in Settings → LLM pricing, or click the row and open the Cost card to price it by hand">
                      no price match{#if s.models[0]}&nbsp;(<code>{s.models[0]}</code>){/if}
                    </span>
                  {/if}
                {:else}
                  <span class="sess-stats tnum">
                    {#each SESSION_STAT_KINDS as k (k.id)}
                      <span><b>{fmtTok(s.totals[k.id] ?? 0)}</b> {k.short}</span>
                    {/each}
                  </span>
                {/if}
                <span class="aloc"
                  >cache-hit {hit === null ? '—' : `${Math.round(hit * 100)}%`}</span
                >
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
{/if}

<style>
  /* Transient inline notice (e.g. a vanished session) — a touch more present
     than a plain placeholder, accent-tinted so it reads as a status message. */
  .placeholder.notice {
    opacity: 0.85;
    color: var(--accent, #e0a060);
  }
  /* The Overview section's group divider (Usage). */
  .group-head {
    margin: 18px 0 8px;
    font-size: 11px;
    font-weight: 600;
    text-transform: uppercase;
    letter-spacing: 0.06em;
    opacity: 0.7;
    border-bottom: 1px solid var(--border-subtle, #333);
    padding-bottom: 4px;
  }
  .group-head:first-of-type {
    margin-top: 0;
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

  /* ── V14 Phase D/D2: Usage section + Advisor card ─────────────────────── */

  .est-badge {
    display: inline-block;
    margin-left: 4px;
    padding: 0 5px;
    border-radius: 8px;
    background: rgba(255, 255, 255, 0.1);
    color: var(--text-primary, #ddd);
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
    border-bottom: 1px solid var(--border-faint, #2a2a2a);
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
     click to recolor that segment (persisted via settings; V28 the donut
     legends share the same treatment). */
  .ubars-legend input.dot,
  .donut-legend input.dot {
    width: 11px;
    height: 11px;
    padding: 0;
    border: 1px solid var(--border-default, #444);
    border-radius: 2px;
    background: none;
    cursor: pointer;
    -webkit-appearance: none;
    appearance: none;
    flex: 0 0 auto;
  }
  .ubars-legend input.dot::-webkit-color-swatch-wrapper,
  .donut-legend input.dot::-webkit-color-swatch-wrapper {
    padding: 0;
  }
  .ubars-legend input.dot::-webkit-color-swatch,
  .donut-legend input.dot::-webkit-color-swatch {
    border: none;
    border-radius: 1px;
  }
  /* The donut kind rows are compact — a slightly smaller picker dot. */
  .donut-legend input.dot.sm {
    width: 9px;
    height: 9px;
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
  /* V28: Dashboard donuts. Ring fills reuse the settings-backed segment
     vars; identity never rides on color alone — every ring has its legend
     rows (values + %) and per-segment tooltips. */
  .donuts {
    display: flex;
    flex-wrap: wrap;
    gap: 12px 28px;
    align-items: flex-start;
  }
  .donut-block {
    flex: 1 1 300px;
    min-width: 260px;
  }
  .donut-title {
    font-size: 11px;
    font-weight: 600;
    opacity: 0.7;
    margin-bottom: 6px;
  }
  .donut-row {
    display: flex;
    align-items: center;
    gap: 14px;
  }
  svg.donut {
    width: 132px;
    height: 132px;
    flex: 0 0 auto;
  }
  /* V40 Phase I: no `.dseg.<lane-id>` rules. A lane's fill is inline, from
     `laneColor()` — the harness's declared position in the palette, or the
     user's pick. The per-id rules could only ever color the lanes core knew
     the names of, so a third declared lane painted SVG-default black. The
     KIND rules below stay: `in`/`cache`/`write`/`out` are cImp's own pricing
     vocabulary, not a harness's declaration. */
  .dseg.in {
    fill: var(--ubar-in, #58a6ff);
  }
  .dseg.cache {
    fill: var(--ubar-cache, #d2a8ff);
  }
  .dseg.write {
    fill: var(--ubar-write, #e3738d);
  }
  .dseg.out {
    fill: var(--ubar-out, #3fb950);
  }
  /* Center figures inherit the app text color (theme-safe). */
  .donut-num {
    fill: currentColor;
    font-size: 15px;
    font-weight: 600;
    text-anchor: middle;
  }
  .donut-sub {
    fill: currentColor;
    opacity: 0.55;
    font-size: 9px;
    text-anchor: middle;
  }
  .donut-legend {
    display: flex;
    flex-direction: column;
    gap: 3px;
    font-size: 11px;
    min-width: 0;
  }
  .dl-head {
    display: flex;
    align-items: center;
    gap: 6px;
  }
  .dl-name {
    font-weight: 600;
  }
  .dl-model {
    max-width: 18ch;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .dl-kinds {
    display: flex;
    flex-wrap: wrap;
    gap: 8px;
    margin: 1px 0 4px 15px;
    opacity: 0.85;
  }
  .dl-kind {
    display: inline-flex;
    align-items: center;
    gap: 4px;
  }
  .donut-empty {
    max-width: 34ch;
  }
  /* Horizontal scroll + zoom viewport around the bars and the S/A lane —
     scrolls once bars hit their minimum width (or the user wheel-zooms in),
     so long sessions keep every turn reachable. */
  .ubars-scroll {
    overflow-x: auto;
    overflow-y: hidden;
    margin-bottom: 12px;
  }
  .ubars-scroll.scrollable {
    padding-bottom: 4px; /* keep the scrollbar off the S/A lane */
  }
  .ubars {
    display: flex;
    align-items: flex-end;
    gap: 3px;
    height: 130px;
    margin-bottom: 4px;
    border-bottom: 1px solid var(--border-subtle, #333);
    padding-bottom: 2px;
  }
  .ubars-legend .zoom-hint {
    margin-left: auto;
    opacity: 0.7;
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
  /* V24 Phase C: agent turns get a subtle accent outline + desaturated
     segment colors so sub-agent spend reads even when the lane is cramped.
     The stacked-segment structure is unchanged. */
  .ubar.agent {
    outline: 1px solid var(--sa-agent, var(--accent, #3b6ea5));
    outline-offset: -1px;
    filter: saturate(0.55);
  }

  /* V24 Phase C: S/A grouping lane. Same flex rhythm as `.ubars` (gap 3px,
     proportional widths) so segments line up under the bar groups. */
  .salane {
    display: flex;
    gap: 3px;
    height: 13px;
    margin: 0;
  }
  .saseg {
    flex-basis: 0;
    min-width: 0;
    display: flex;
    align-items: center;
    justify-content: center;
    overflow: hidden;
    border-radius: 2px;
    font-size: 9px;
    font-weight: 600;
    line-height: 1;
    letter-spacing: 0.5px;
  }
  /* Colors flow from settings via the legend pickers (like the segments). */
  /* V40 Phase I: `background` and `color` are inline, from `laneColor()` and
     `laneTextColor()`. Deliberately theme-independent, for the reason the two
     retired per-id rules gave: the fill is a chart color (declared slot or the
     user's pick), so the text contrasts with THAT, not with the theme — and
     picking it by luma is the only form of that rule which survives a lane
     core has never heard of. */

  /* V24 Phase C: card title + selected-session controls. */
  .history-head .card-title {
    font-weight: inherit;
  }
  .history-head .card-title code {
    font-size: 0.9em;
    opacity: 0.85;
  }
  .live-pill {
    border-color: var(--accent, #3b6ea5) !important;
    color: var(--accent, #3b6ea5) !important;
  }
  .session-id-hint {
    font-size: 11px;
    margin: -2px 0 8px;
    font-variant-numeric: tabular-nums;
  }
  .session-id-hint code {
    user-select: all;
  }

  .arow.tool {
    grid-template-columns: 1fr 1fr auto;
  }
  /* agent | the four billing stats | cache-hit % */
  .arow.sessrow {
    grid-template-columns: minmax(6rem, 1fr) auto auto;
  }
  /* Each row is a flex wrapper holding TWO buttons (the session-select row and
     the Workbench commits jump) — nesting them would be invalid HTML. The
     row divider lives on the wrapper so it spans both. */
  .sessrow-wrap {
    display: flex;
    align-items: center;
    gap: 6px;
    border-bottom: 1px solid var(--border-faint, #2a2a2a);
  }
  /* The main row area is a <button> (it selects the session — drilling the
     "This session" + Cost cards into it) — strip the UA button chrome so it
     keeps rendering as a plain grid row. */
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
  /* V24 Phase C: the drilled-in session (distinct from Phase E's active
     marker) — a filled row background. */
  button.arow.sessrow.selected {
    background: var(--surface-raised, rgba(255, 255, 255, 0.09));
    box-shadow: inset 2px 0 0 var(--accent, #3b6ea5);
  }
  /* V24 Phase E: a live session (open tab ∪ recency). The accent left edge
     matches `.selected`'s bar, but the two states stay distinguishable when
     they coexist: `.active` carries the pulsing `.active-dot` (below) and no
     fill, `.selected` carries the fill and no dot, and a row that is BOTH shows
     fill + edge + dot. ALL active rows get this — many can be live at once. */
  button.arow.sessrow.active {
    box-shadow: inset 2px 0 0 var(--accent, #3b6ea5);
  }
  /* Pulsing dot before the agent label — the primary "active now" signal. */
  .active-dot {
    display: inline-block;
    width: 6px;
    height: 6px;
    margin-right: 5px;
    vertical-align: middle;
    border-radius: 50%;
    background: var(--accent, #3b6ea5);
    animation: sess-active-pulse 1.4s ease-in-out infinite;
  }
  @keyframes sess-active-pulse {
    0%,
    100% {
      opacity: 0.35;
    }
    50% {
      opacity: 1;
    }
  }
  /* Honor reduced-motion (theme.css sets the precedent): drop the pulse but
     keep the dot at a steady, legible opacity so the marker still reads. */
  @media (prefers-reduced-motion: reduce) {
    .active-dot {
      animation: none;
      opacity: 0.85;
    }
  }
  .commits-btn {
    flex: 0 0 auto;
    font-variant-numeric: tabular-nums;
    white-space: nowrap;
  }

  /* V24 Phase D: Cost card — per-model rows. Each row stacks a head (model id
     + provider chip + pricing select), the S/A share line, an optional Custom
     rate grid, the tokens/$-per-MTok/cost table, and a subtotal. The
     `.cost-custom` + `.cost-table` rules below are shared with (were the) the
     old popup's markup. */
  .costrow {
    padding: 8px 0;
    border-bottom: 1px solid var(--border-faint, #2a2a2a);
  }
  .costrow:last-child {
    border-bottom: none;
  }
  .costrow-head {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 8px;
    margin-bottom: 4px;
  }
  .cm-model {
    display: flex;
    align-items: center;
    gap: 6px;
    min-width: 0;
    font-size: 12px;
  }
  .cm-model code {
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .cm-provider {
    flex: 0 0 auto;
    font-size: 10px;
    padding: 1px 6px;
    border-radius: 999px;
    background: var(--surface-raised, rgba(255, 255, 255, 0.08));
    color: var(--text-quiet, #999);
  }
  .cm-pick {
    flex: 0 0 auto;
    max-width: 16rem;
  }
  .cm-share {
    font-size: 11px;
    margin-bottom: 6px;
    font-variant-numeric: tabular-nums;
  }
  .cm-subtotal {
    text-align: right;
    font-size: 12px;
  }
  .cm-subtotal b {
    margin-left: 6px;
  }
  .cost-foot {
    display: flex;
    justify-content: space-between;
    align-items: baseline;
    font-size: 14px;
    margin: 10px 0 4px;
  }
  .cost-foot b {
    font-size: 16px;
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
    border-bottom: 1px solid var(--border-faint, #2a2a2a);
  }
  .cost-table thead th {
    text-align: right;
    font-weight: 600;
    color: var(--text-quiet, #999);
  }
  .cost-table tbody th {
    text-align: left;
    font-weight: 500;
    color: var(--text-quiet, #999);
    white-space: nowrap;
  }
  .cost-table td {
    text-align: right;
    white-space: nowrap;
  }
  .cost-table tbody tr:last-child th,
  .cost-table tbody tr:last-child td {
    font-weight: 600;
    color: var(--text-primary, #ddd);
  }
  .cost-hint {
    color: var(--text-quiet, #999);
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
    background: var(--border-default, #444);
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
</style>
