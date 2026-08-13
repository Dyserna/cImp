<script lang="ts">
  // The read-only, app-rendered Events tab (#51) — the same persistent
  // activity store the Tool Activity tab's "Activities" section shows, but
  // rendered as the ATTRIBUTION view: every row says which tab and which
  // harness session it belongs to, and the feed is filterable by kind,
  // source/screen and tab. Workbench CHECKPOINTS are merged in as synthetic
  // rows (kind `checkpoint`, straight from the shadow repo — see the note at
  // `cps` below); clicking one lands on the Workbench Timeline with that
  // checkpoint highlighted.
  //
  // ADDITIVE by decision: Tool Activity keeps its own feed untouched, and the
  // Workbench Timeline stays a separate tab. The overlap between the three is
  // accepted for now — consolidating them is a later, separate call.
  //
  // Same reserved/no-PTY pattern as ToolActivityView: mounted once into the
  // appViews.ts keep-alive registry for the `events` tab id, so the poll must
  // gate on appViewVisibility (a detached view keeps running otherwise).
  import { onMount, onDestroy } from 'svelte';
  import {
    activityClear,
    activityDelete,
    activityDetail,
    activityList,
    attributionId,
    attributionState,
    filterEntries,
    matchesTabFilter,
    mergeEntries,
    rowStatus,
    tabFilterValue,
    FILTER_ANY,
    NO_FILTER,
    TAB_FILTER_HEADLESS,
    TAB_FILTER_UNATTRIBUTED,
    TAB_FILTER_UNRECOGNIZED,
    type ActivityEntry,
    type ActivityRecord,
    type Attribution,
    type AttributionState,
    type FeedFilter,
  } from './activity';
  import StatusChip from './StatusChip.svelte';
  import { fmtDate, fmtTime } from './format';
  import { fmtTok } from './usageMath';
  import { EVENTS_TAB_ID, WORKBENCH_TAB_ID } from './tabs/types';
  import { revealTab } from './tabs/visibility';
  import { isAppViewVisible, onAppViewShown } from './appViewVisibility';
  import { loadViewSet, loadViewString, saveViewSet, saveViewString } from './viewSection';
  import { settings } from './settings/store';
  import { workbenchCheckpoints, openTimelineCheckpoint, type Checkpoint } from './workbench';

  // ── Feed poll ─────────────────────────────────────────────────────────
  // `$state.raw`: the feed holds up to ~1.4k rows (the per-lane caps in
  // `crate::activity`) and is only ever REPLACED, never mutated in place, so
  // plain `$state` would deep-proxy every row on every poll for nothing.
  let entries = $state.raw<ActivityEntry[]>([]);
  let poll: ReturnType<typeof setInterval> | null = null;
  // Bumped by every local mutation (delete/clear — moved here from the Tools
  // tab's retired Activities section). A poll response already in flight when
  // a mutation landed is stale — applying it would resurrect just-deleted
  // rows for a poll cycle — so refresh() drops it.
  let mutationSeq = 0;

  async function refresh(): Promise<void> {
    const seq = mutationSeq;
    try {
      const list = await activityList();
      // Reuse the rows already held (see `mergeEntries`) — a plain
      // `entries = list` re-renders the whole feed on every 2s poll.
      if (seq === mutationSeq) entries = mergeEntries(entries, list);
    } catch {
      /* backend unavailable mid-teardown — keep whatever we have */
    }
  }

  // ── Checkpoint rows (#51) ─────────────────────────────────────────────
  // Merged CLIENT-SIDE from the shadow repo (`workbench_checkpoints`), not
  // recorded into the activity store: checkpoints already persist in the
  // shadow repo under their own GC, so a second copy in tool-activity.jsonl
  // could only drift from it (a GC'd checkpoint whose "event" still claims
  // a restore point, or the reverse), and a new store kind would also owe a
  // retention-lane decision (see `kind_cap` in `crate::activity`). Events is
  // the INDEX; the Timeline and its shadow repo stay the source of truth —
  // the same one-way join the Timeline itself uses for contamination rows,
  // pointed the other way.
  let cps = $state.raw<Checkpoint[]>([]);

  async function refreshCheckpoints(): Promise<void> {
    if (!$settings.workbench.checkpoints) {
      cps = [];
      return;
    }
    try {
      // Newest first (the command returns oldest-first), reusing held rows
      // like the activity poll above.
      cps = mergeCheckpoints(cps, (await workbenchCheckpoints()).slice().reverse());
    } catch {
      /* keep the last list — a failed read is not an empty timeline */
    }
  }

  /// `mergeEntries`' reuse trick, resting on the same invariant: a checkpoint
  /// tag's metadata never changes after creation (a dedup hit returns the
  /// previous tag unrelabelled), so an id already held identifies identical
  /// content.
  function mergeCheckpoints(prev: Checkpoint[], next: Checkpoint[]): Checkpoint[] {
    const byId = new Map(prev.map((c) => [c.id, c]));
    let identical = prev.length === next.length;
    const merged = next.map((c, i) => {
      const kept = byId.get(c.id) ?? c;
      if (identical && kept !== prev[i]) identical = false;
      return kept;
    });
    return identical ? prev : merged;
  }

  // ── Filters ───────────────────────────────────────────────────────────
  // CLIENT-SIDE: the poll fetches the whole feed and `filterEntries` narrows
  // it here. `activity_list` does take a server-side filter, but it can only
  // express a real tab id — not the headless / unattributed / unrecognized
  // states, which are three of this view's four tab selections — and the
  // option lists below have to be derived from an unfiltered feed anyway. See
  // the "Feed filtering" note in activity.ts for what the switch would take.
  let filter = $state<FeedFilter>({ ...NO_FILTER });

  const shown = $derived(filterEntries(entries, filter));

  // The synthetic kind checkpoint rows wear in the Kind column and filter.
  // Not an `ActivityKind` — these rows never touch the store.
  const CHECKPOINT_KIND = 'checkpoint';

  /// A checkpoint's Source cell: the harness that prompted it, or
  /// `workbench` for the app's own triggers (burst / manual / pre-restore).
  function cpSource(cp: Checkpoint): string {
    return cp.agent ?? 'workbench';
  }

  /// A checkpoint's attribution, in the feed's own four-state vocabulary.
  /// `tab` is cImp-authored (`Origin` at snapshot time), so a present id
  /// renders as a tab. Absent splits on trigger: burst/manual/pre-restore
  /// positively have no conversation behind them (headless), while a prompt
  /// checkpoint without one predates the identity fields (unattributed).
  function cpAttribution(cp: Checkpoint): Attribution {
    if (cp.tab) return { tab: cp.tab };
    return cp.trigger === 'prompt' ? 'unattributed' : 'headless';
  }

  const shownCps = $derived(
    cps.filter(
      (cp) =>
        (filter.kind === FILTER_ANY || filter.kind === CHECKPOINT_KIND) &&
        (filter.source === FILTER_ANY || cpSource(cp) === filter.source) &&
        matchesTabFilter(cpAttribution(cp), filter.tab),
    ),
  );

  // One merged, newest-first list. Row WRAPPERS are reused so long as the
  // underlying object is the one already held (`mergeEntries` /
  // `mergeCheckpoints` make identity mean identical content) — with fresh
  // wrappers every poll, every row's expressions would re-evaluate each
  // tick, the exact full-table churn `mergeEntries` exists to prevent.
  type FeedRow =
    | { key: string; ts: number; e: ActivityEntry; cp: null }
    | { key: string; ts: number; e: null; cp: Checkpoint };
  let rowCache = new Map<string, FeedRow>();
  const shownRows = $derived.by(() => {
    const cache = new Map<string, FeedRow>();
    const out: FeedRow[] = [];
    for (const e of shown) {
      const key = `a${e.id}`;
      const prev = rowCache.get(key);
      const row: FeedRow = prev && prev.e === e ? prev : { key, ts: e.ts_ms, e, cp: null };
      cache.set(key, row);
      out.push(row);
    }
    for (const cp of shownCps) {
      const key = `c${cp.id}`;
      const prev = rowCache.get(key);
      const row: FeedRow =
        prev && prev.cp === cp ? prev : { key, ts: cp.ts_unix * 1000, e: null, cp };
      cache.set(key, row);
      out.push(row);
    }
    rowCache = cache;
    // Stable sort, so same-millisecond rows keep activity-before-checkpoint
    // order instead of flickering between polls.
    out.sort((a, b) => b.ts - a.ts);
    return out;
  });

  const totalCount = $derived(entries.length + cps.length);

  // Options are derived from the feed rather than hardcoded: `kind` gains
  // variants over time and `source` is free text (it names the offload
  // backend, or the V32 screen that fired), so anything this build doesn't
  // know still shows up and stays selectable.
  const kindOptions = $derived(
    [
      ...new Set<string>([
        ...entries.map((e) => e.kind),
        ...(cps.length > 0 ? [CHECKPOINT_KIND] : []),
      ]),
    ].sort((a, b) => a.localeCompare(b)),
  );
  const sourceOptions = $derived(
    [...new Set([...entries.map((e) => e.source), ...cps.map(cpSource)])].sort((a, b) =>
      a.localeCompare(b),
    ),
  );
  // ONLY genuine `{ tab: x }` ids become tab options. An `{ unrecognized: x }`
  // row names no configured tab, so offering it here would put a phantom tab
  // in the picker of the view whose entire job is attribution; those rows are
  // reachable through the dedicated "unrecognized" option instead.
  const tabOptions = $derived(
    [
      ...new Set([
        ...entries
          .filter((e) => attributionState(e.tab) === 'tab')
          .map((e) => attributionId(e.tab) as string),
        ...cps.filter((cp) => cp.tab !== null).map((cp) => cp.tab as string),
      ]),
    ].sort((a, b) => a.localeCompare(b)),
  );

  /// Land on the Workbench Timeline with that checkpoint highlighted — Events
  /// is the index, the Timeline is the specialist view (#51).
  ///
  /// This USED to be the checkpoint row's click. It isn't any more: a row that
  /// silently switched tabs was the one row in the feed whose click did
  /// something other than "show me this record", and there was no way to read a
  /// checkpoint's commit, seq or full label without leaving the feed. The jump
  /// is now a button in the detail popup, so every row in the feed opens its
  /// record and the tab switch is something you ask for.
  function openCheckpoint(cp: Checkpoint): void {
    openTimelineCheckpoint(cp.id);
    revealTab(WORKBENCH_TAB_ID);
    // Nothing behind this dialog is what the user is looking at any more.
    closeDetail();
  }

  function resetFilters(): void {
    filter = { ...NO_FILTER };
  }

  // ── Columns: visibility + drag-resize ─────────────────────────────────
  // Both are per-machine VIEW preferences (like the sub-tab selection), so
  // they persist through viewSection.ts / localStorage, not settings. Widths
  // are px; `target` is the one flexible column (minmax → 1fr) so the table
  // always fills the card, and its width acts as a minimum.
  type ColKey = 'time' | 'kind' | 'source' | 'tool' | 'target' | 'status' | 'tab' | 'session';
  interface Col {
    key: ColKey;
    label: string;
    min: number;
    def: number;
    flex?: boolean;
  }
  const COLUMNS: readonly Col[] = [
    { key: 'time', label: 'Time', min: 52, def: 88 },
    { key: 'kind', label: 'Kind', min: 48, def: 80 },
    { key: 'source', label: 'Source', min: 52, def: 96 },
    { key: 'tool', label: 'Tool', min: 60, def: 144 },
    { key: 'target', label: 'Target', min: 80, def: 160, flex: true },
    { key: 'status', label: 'Status', min: 52, def: 104 },
    { key: 'tab', label: 'Tab', min: 52, def: 144 },
    { key: 'session', label: 'Session', min: 52, def: 88 },
  ];

  function loadWidths(): Record<ColKey, number> {
    const out = Object.fromEntries(COLUMNS.map((c) => [c.key, c.def])) as Record<ColKey, number>;
    try {
      const raw = loadViewString('events', 'col-widths');
      const saved: unknown = raw ? JSON.parse(raw) : null;
      if (saved && typeof saved === 'object') {
        for (const c of COLUMNS) {
          const v = (saved as Record<string, unknown>)[c.key];
          // Clamp to the column minimum so a corrupt/ancient value can never
          // load a 0-width (invisible but "visible") column.
          if (typeof v === 'number' && Number.isFinite(v)) out[c.key] = Math.max(c.min, v);
        }
      }
    } catch {
      /* unparseable → defaults */
    }
    return out;
  }

  function loadVisible(): Record<ColKey, boolean> {
    // Stored as the HIDDEN set so newly added columns default to visible.
    const hidden = new Set(loadViewSet('events', 'cols-hidden'));
    return Object.fromEntries(COLUMNS.map((c) => [c.key, !hidden.has(c.key)])) as Record<
      ColKey,
      boolean
    >;
  }

  let widths = $state<Record<ColKey, number>>(loadWidths());
  let visible = $state<Record<ColKey, boolean>>(loadVisible());
  let colMenuOpen = $state(false);

  const shownCols = $derived(COLUMNS.filter((c) => visible[c.key]));
  // One custom property on the card drives every row's grid — the rows
  // themselves never re-render on resize.
  const gridTemplate = $derived(
    shownCols
      .map((c) => (c.flex ? `minmax(${widths[c.key]}px, 1fr)` : `${widths[c.key]}px`))
      .join(' '),
  );

  function toggleCol(key: ColKey): void {
    if (visible[key] && shownCols.length === 1) return; // never hide the last one
    visible[key] = !visible[key];
    saveViewSet(
      'events',
      'cols-hidden',
      COLUMNS.filter((c) => !visible[c.key]).map((c) => c.key),
    );
  }

  // Drag state is a plain variable, not $state: nothing renders from it, the
  // grip's pointer capture keeps move/up events flowing to the grip itself.
  let resizing: { key: ColKey; min: number; startX: number; startW: number } | null = null;

  function startResize(e: PointerEvent & { currentTarget: HTMLElement }, col: Col): void {
    e.preventDefault();
    e.currentTarget.setPointerCapture(e.pointerId);
    resizing = { key: col.key, min: col.min, startX: e.clientX, startW: widths[col.key] };
  }

  function moveResize(e: PointerEvent): void {
    if (!resizing) return;
    widths[resizing.key] = Math.max(resizing.min, resizing.startW + e.clientX - resizing.startX);
  }

  function endResize(): void {
    if (!resizing) return;
    resizing = null;
    saveViewString('events', 'col-widths', JSON.stringify(widths));
  }

  // ── Detail popup ──────────────────────────────────────────────────────
  let detailOpen = $state(false);
  let detail = $state<ActivityRecord | null>(null);
  let detailMissing = $state(false);
  // Fetch token: a click on row B while row A's (slower) fetch is in flight
  // must not let A's late response overwrite the popup.
  let detailSeq = 0;

  /// The checkpoint the popup is showing, by id — `null` when the popup is
  /// showing an activity row (or nothing). Which of the two the popup renders
  /// is decided by this id, NOT by `detail`, so a checkpoint click can never
  /// land in the "Loading…" leg of the activity branch.
  let detailCpId = $state<string | null>(null);

  /// Resolved against the LIVE checkpoint list each render rather than captured
  /// at click time: the shadow repo has its own GC, and turning checkpoints off
  /// empties `cps` outright. A stale captured copy would keep offering "Open in
  /// Timeline" for a checkpoint the Timeline no longer has.
  let detailCp = $derived(
    detailCpId === null ? null : (cps.find((c) => c.id === detailCpId) ?? null),
  );

  async function openDetail(id: number): Promise<void> {
    const seq = ++detailSeq;
    detailOpen = true;
    detailCpId = null;
    detail = null;
    detailMissing = false;
    try {
      const rec = await activityDetail(id);
      if (seq !== detailSeq) return; // superseded by a later click / close
      if (rec) detail = rec;
      else detailMissing = true;
    } catch {
      if (seq === detailSeq) detailMissing = true;
    }
  }

  /// A checkpoint row's click. No fetch: the row already holds the whole
  /// record (the feed merges it from `workbench_checkpoints` client-side), so
  /// the popup opens populated. The seq bump still matters — it invalidates an
  /// activity fetch started by a previous click that hasn't landed yet.
  function openCheckpointDetail(cp: Checkpoint): void {
    detailSeq += 1;
    detail = null;
    detailMissing = false;
    detailCpId = cp.id;
    detailOpen = true;
  }

  function closeDetail(): void {
    detailSeq += 1; // invalidate any in-flight fetch
    detailOpen = false;
    detail = null;
    detailMissing = false;
    detailCpId = null;
  }

  // ── Delete / clear ────────────────────────────────────────────────────
  // Moved here from the Tools tab when its Activities section was retired
  // (#51 consolidation) — this feed is now the one place the store is read,
  // so it is also the one place it can be pruned. ACTIVITY ROWS ONLY: the
  // checkpoint rows merged in above live in the shadow repo under its own
  // GC and are untouched by both actions.
  //
  // Both update optimistically, then unconditionally refresh AFTER the final
  // seq bump: the bump invalidates any poll that raced the backend mutation,
  // and the trailing refresh (which captures the post-bump seq) repaints
  // authoritatively — restoring the rows if the backend call failed.
  async function removeEntry(id: number): Promise<void> {
    mutationSeq += 1;
    entries = entries.filter((e) => e.id !== id);
    if (detail?.id === id) closeDetail();
    try {
      await activityDelete(id);
    } catch {
      /* the refresh below restores the row */
    }
    mutationSeq += 1;
    void refresh();
  }

  // Two-step confirm (same pattern as the retired section's): the first
  // click arms the button, a second within 4s clears for real.
  let confirmClear = $state(false);
  let clearTimer: ReturnType<typeof setTimeout> | null = null;

  async function clearHistory(): Promise<void> {
    if (!confirmClear) {
      confirmClear = true;
      clearTimer = setTimeout(() => (confirmClear = false), 4000);
      return;
    }
    if (clearTimer) clearTimeout(clearTimer);
    clearTimer = null;
    confirmClear = false;
    mutationSeq += 1;
    entries = [];
    closeDetail();
    try {
      await activityClear();
    } catch {
      /* the refresh below restores the feed */
    }
    mutationSeq += 1;
    void refresh();
  }

  function onKeyDown(e: KeyboardEvent): void {
    if (e.key !== 'Escape') return;
    if (detailOpen) {
      e.preventDefault();
      closeDetail();
    } else if (colMenuOpen) {
      e.preventDefault();
      colMenuOpen = false;
    }
  }

  // Keep-alive (appViews.ts): the poll idles while the tab is off-screen and
  // a fresh refresh runs the moment it comes back. A periodic job in a
  // reserved app view that does NOT gate on appViewVisibility keeps burning
  // IPC forever once the tab has been opened once — a known bug class here.
  const unsubShown = onAppViewShown(EVENTS_TAB_ID, () => {
    void refresh();
    void refreshCheckpoints();
  });

  // Checkpoints change far less often than the activity feed and cost a git
  // spawn per read, so they ride every CP_EVERY-th tick of the 2s poll (plus
  // the on-shown refresh above) rather than all of them.
  const CP_EVERY = 3;
  let pollTick = 0;

  onMount(() => {
    void refresh();
    void refreshCheckpoints();
    poll = setInterval(() => {
      if (!isAppViewVisible(EVENTS_TAB_ID)) return;
      void refresh();
      pollTick += 1;
      if (pollTick % CP_EVERY === 0) void refreshCheckpoints();
    }, 2000);
    window.addEventListener('keydown', onKeyDown);
  });

  onDestroy(() => {
    if (poll) clearInterval(poll);
    if (clearTimer) clearTimeout(clearTimer);
    window.removeEventListener('keydown', onKeyDown);
    unsubShown();
  });

  // ── Row rendering ─────────────────────────────────────────────────────
  // Agent sources with a dedicated accent class; anything else (offload
  // backend names are user-chosen, screen names grow) falls back to the
  // default colour rather than leaking arbitrary strings into a class.
  const KNOWN_SOURCES = new Set([
    'claude',
    'opencode',
    'offload',
    'read_advisor',
    'auto_check',
    'audit',
  ]);
  function srcClass(source: string): string {
    return KNOWN_SOURCES.has(source) ? ` ${source}` : '';
  }

  // `ok` alone does not mean "the call worked" in this store, so the status
  // column untangles the overlapping conventions instead of painting a bare
  // pass/fail. The rule itself now lives in `activity.ts::rowStatus` and is
  // rendered by `StatusChip`, shared with the Tool Activity feed.
  //
  // **What moved and why** (#48, M-24). The local version here was
  // `e.ok ? 'flagged' : 'denied'` for every `injection_flag` row, so five
  // distinct facts arrived as one word: a detector match, a result that was only
  // PARTLY screened (`unscreened` — nothing found, nothing stopped), a memory
  // write held for review, containment engaging, and a latch override the USER
  // applied to give capability BACK. The last two are not even containment
  // firing, and `unscreened` read as the opposite of what it means. It also read
  // `!ok` as "denied" for `updater` rows, whose `ok` is a bundle outcome — so a
  // rejected rules bundle reported as a blocked tool call.
  //
  // Kept in a `.ts` file because `.svelte` has no test harness here, and shared
  // with the other feed because the security vocabulary of this app must not
  // differ between two tabs rendering the same rows.

  function rowTool(e: ActivityEntry): string {
    // mcp tools are namespaced `<server>__<tool>` — render the first `__` as
    // a separator so the server reads as a prefix.
    return e.kind === 'graph'
      ? e.tool.replace('graph_', '')
      : e.kind === 'mcp'
        ? e.tool.replace('__', '/')
        : e.tool;
  }

  function rowMeta(e: ActivityEntry): string {
    const dur = e.ms >= 10_000 ? `${(e.ms / 1000).toFixed(1)}s` : `${e.ms}ms`;
    // For audit entries `chars` carries the finding count, not a payload size.
    if (e.kind === 'audit') return `${dur} · ${e.chars} findings`;
    // A server lifecycle row has no payload — `chars` is always 0, and printing
    // "0 chars" would dress a structural zero up as a measurement. What its
    // duration MEANS also varies by transition, so it says so rather than
    // leaving a bare number to be read as latency.
    if (e.kind === 'offload_server') {
      if (e.tool === 'ready') return `${dur} to healthy`;
      if (e.tool === 'stop' || (e.tool === 'fail' && e.target === 'exited unexpectedly')) {
        return `up ${dur}`;
      }
      return e.ms > 0 ? dur : '';
    }
    return `${dur} · ${fmtTok(e.chars)} chars`;
  }

  // The attribution cell. Each of the four states gets its OWN label and its
  // own styling — see the `Attribution` doc in activity.ts for why collapsing
  // any two of them is a bug, and .attr-* below for how they stay visually
  // distinct.
  // Over `Attribution` rather than `ActivityEntry`, so the synthetic
  // checkpoint rows wear the exact same four-state treatment.
  function attrLabel(a: Attribution): string {
    const state = attributionState(a);
    switch (state) {
      case 'tab':
        return attributionId(a) as string;
      case 'unrecognized':
        // Never rendered as a bare id: the id is shown (it is evidence) but
        // always behind the word that says it names no tab that exists.
        return `unrecognized: ${attributionId(a)}`;
      case 'headless':
        return 'no tab';
      default:
        return 'not recorded';
    }
  }

  function attrTitle(a: Attribution): string {
    switch (attributionState(a)) {
      case 'tab':
        return `Tab ${attributionId(a)}`;
      case 'unrecognized':
        return `Unrecognized id "${attributionId(a)}" — it names no configured tab, so this row is NOT attributable to a tab`;
      case 'headless':
        return 'No tab: a headless caller (claude -p, cron, a worker task, or cImp’s own internal work). A fact about the caller, not missing data.';
      default:
        return 'Not recorded: the writer did not know which tab this was, or the row predates the attribution column. Different from "no tab".';
    }
  }

  function attrState(a: Attribution): AttributionState {
    return attributionState(a);
  }

  // Sessions are long opaque ids; the head is enough to eyeball two rows as
  // the same conversation, and the full value is in the tooltip + the popup.
  function shortSession(s: string | null): string {
    if (!s) return '—';
    return s.length > 10 ? `${s.slice(0, 8)}…` : s;
  }
</script>

<div class="events">
  <header>
    <h2>Events</h2>
    <div class="head-right">
      <span class="count">
        {shownRows.length === totalCount
          ? `${totalCount} events`
          : `${shownRows.length} of ${totalCount} events`}
      </span>
      {#if entries.length > 0}
        <button
          type="button"
          class="clear-btn"
          class:arm={confirmClear}
          title="Clears the recorded activity history. Checkpoint rows stay — they live in the shadow repo under its own retention."
          onclick={clearHistory}
        >{confirmClear ? 'Confirm clear' : 'Clear history'}</button>
      {/if}
    </div>
  </header>

  <div class="filters">
    <label>
      <span>Kind</span>
      <select bind:value={filter.kind}>
        <option value={FILTER_ANY}>All kinds</option>
        {#each kindOptions as k (k)}
          <option value={k}>{k}</option>
        {/each}
      </select>
    </label>

    <label>
      <span>Source</span>
      <select bind:value={filter.source}>
        <option value={FILTER_ANY}>All sources</option>
        {#each sourceOptions as s (s)}
          <option value={s}>{s}</option>
        {/each}
      </select>
    </label>

    <label>
      <span>Tab</span>
      <select bind:value={filter.tab}>
        <option value={FILTER_ANY}>All tabs</option>
        {#each tabOptions as t (t)}
          <option value={tabFilterValue(t)}>{t}</option>
        {/each}
        <!-- The three non-tab states are first-class selections, not an
             afterthought: "which rows had no tab at all" and "which rows we
             failed to attribute" are the two questions this view exists to
             answer, and neither is a tab id. -->
        <option value={TAB_FILTER_HEADLESS}>— no tab (headless)</option>
        <option value={TAB_FILTER_UNATTRIBUTED}>— not recorded</option>
        <option value={TAB_FILTER_UNRECOGNIZED}>— unrecognized id</option>
      </select>
    </label>

    <button
      type="button"
      class="reset"
      disabled={filter.kind === FILTER_ANY &&
        filter.source === FILTER_ANY &&
        filter.tab === FILTER_ANY}
      onclick={resetFilters}>Reset</button
    >

    <div class="colmenu-wrap">
      <button type="button" class="reset" onclick={() => (colMenuOpen = !colMenuOpen)}
        >Columns ▾</button
      >
      {#if colMenuOpen}
        <!-- Transparent backdrop = outside-click-to-close, same pattern as the
             detail dialog's backdrop but without the dimming. -->
        <div class="colmenu-backdrop" onclick={() => (colMenuOpen = false)} role="presentation">
        </div>
        <div class="colmenu">
          {#each COLUMNS as c (c.key)}
            <label class="colopt">
              <input
                type="checkbox"
                checked={visible[c.key]}
                disabled={visible[c.key] && shownCols.length === 1}
                onchange={() => toggleCol(c.key)}
              />
              <span>{c.label}</span>
            </label>
          {/each}
        </div>
      {/if}
    </div>
  </div>

  <section class="card feed" style="--ecols: {gridTemplate}">
    <div class="erow head">
      {#each shownCols as c (c.key)}
        <span>
          {c.label}
          <span
            class="grip"
            role="separator"
            aria-orientation="vertical"
            aria-label="Resize {c.label} column"
            onpointerdown={(e) => startResize(e, c)}
            onpointermove={moveResize}
            onpointerup={endResize}
            onpointercancel={endResize}
          ></span>
        </span>
      {/each}
    </div>
    {#if totalCount === 0}
      <div class="empty">
        No events recorded yet — query the graph from a Claude tab, run an
        offload_task, or start a local offload server, and it shows up here.
      </div>
    {:else if shownRows.length === 0}
      <div class="empty">No events match the current filters.</div>
    {:else}
      <div class="rows">
        {#each shownRows as row (row.key)}
          {#if row.e}
            {@const r = row.e}
            <div
              class="erow"
              role="button"
              tabindex="0"
              onclick={() => void openDetail(r.id)}
              onkeydown={(e) => {
                if (e.key === 'Enter' || e.key === ' ') {
                  e.preventDefault();
                  void openDetail(r.id);
                }
              }}
            >
              {#if visible.time}<span class="etime">{fmtTime(r.ts_ms)}</span>{/if}
              {#if visible.kind}<span class="ekind {r.kind}">{r.kind}</span>{/if}
              {#if visible.source}<span class="esrc{srcClass(r.source)}" title={r.source}
                  >{r.source}</span
                >{/if}
              {#if visible.tool}<span class="etool" title={r.tool}>{rowTool(r)}</span>{/if}
              {#if visible.target}<span class="etarget" title={r.target}>{r.target}</span>{/if}
              {#if visible.status}<StatusChip status={rowStatus(r)} />{/if}
              {#if visible.tab}<span class="eattr attr-{attrState(r.tab)}" title={attrTitle(r.tab)}
                  >{attrLabel(r.tab)}</span
                >{/if}
              {#if visible.session}<span
                  class="esession"
                  class:none={!r.session}
                  title={r.session ?? 'No session recorded'}>{shortSession(r.session)}</span
                >{/if}
            </div>
          {:else}
            {@const cp = row.cp}
            <!-- A checkpoint row opens the same detail popup every other row
                 opens; the jump to the Workbench Timeline is a button inside
                 it (see `openCheckpoint`). -->
            <div
              class="erow"
              role="button"
              tabindex="0"
              title="Show this checkpoint's details"
              onclick={() => openCheckpointDetail(cp)}
              onkeydown={(e) => {
                if (e.key === 'Enter' || e.key === ' ') {
                  e.preventDefault();
                  openCheckpointDetail(cp);
                }
              }}
            >
              {#if visible.time}<span class="etime">{fmtTime(row.ts)}</span>{/if}
              {#if visible.kind}<span class="ekind checkpoint">checkpoint</span>{/if}
              {#if visible.source}<span
                  class="esrc{srcClass(cpSource(cp))}"
                  title={cpSource(cp)}>{cpSource(cp)}</span
                >{/if}
              {#if visible.tool}<span class="etool" title={cp.trigger}>{cp.trigger}</span>{/if}
              {#if visible.target}<span class="etarget" title={cp.label}>{cp.label}</span>{/if}
              {#if visible.status}<span
                  class="cpfiles"
                  title="Files changed since the previous checkpoint — what this checkpoint captured when taken, not its diff against the current tree"
                  >{cp.files_changed} files</span
                >{/if}
              {#if visible.tab}
                {@const a = cpAttribution(cp)}
                <span class="eattr attr-{attrState(a)}" title={attrTitle(a)}>{attrLabel(a)}</span>
              {/if}
              {#if visible.session}<span
                  class="esession"
                  class:none={!cp.session}
                  title={cp.session ?? 'No session recorded'}>{shortSession(cp.session)}</span
                >{/if}
            </div>
          {/if}
        {/each}
      </div>
    {/if}
  </section>
</div>

{#if detailOpen}
  <div class="backdrop" onclick={closeDetail} role="presentation"></div>
  <div
    class="detail-card"
    role="dialog"
    aria-label={detailCpId ? 'Checkpoint detail' : 'Event detail'}
  >
    {#if detailCpId}
      <!-- The checkpoint leg. Branching on the ID (not on `detailCp`) keeps a
           GC'd or filtered-out checkpoint in THIS leg, where it can say so,
           instead of falling through to the activity branch's "Loading…". -->
      {#if detailCp}
        {@const cp = detailCp}
        {@const cpTs = cp.ts_unix * 1000}
        {@const a = cpAttribution(cp)}
        <header class="detail-head">
          <div class="detail-title">
            <span class="ekind checkpoint">checkpoint</span>
            <span class="detail-tool" title={cp.label}>{cp.label}</span>
            <span
              class="cpfiles"
              title="Files changed since the previous checkpoint — what this checkpoint captured when taken, not its diff against the current tree"
              >{cp.files_changed} files</span
            >
          </div>
          <button type="button" class="detail-close icon" onclick={closeDetail} aria-label="Close"
            >×</button
          >
        </header>
        <div class="detail-meta">
          {fmtDate(cpTs)} · {fmtTime(cpTs)} · {cpSource(cp)} · {cp.trigger}
        </div>
        <div class="detail-attr">
          <span class="eattr attr-{attrState(a)}" title={attrTitle(a)}>{attrLabel(a)}</span>
          <span class="detail-session">
            session: {cp.session ?? 'not recorded'}
          </span>
        </div>
        <div class="detail-body">
          <!-- The fields the feed's columns cannot carry. The commit and the
               tag id are here because they are what you take to git or to a
               bug report; the row only ever showed the label. -->
          <div class="cpgrid">
            <span class="cplabel">Label</span>
            <span>{cp.label}</span>
            <span class="cplabel">Trigger</span>
            <span>{cp.trigger}</span>
            <span class="cplabel">Taken</span>
            <span>{fmtDate(cpTs)} · {fmtTime(cpTs)}</span>
            <span class="cplabel">Files changed</span>
            <span>{cp.files_changed} since the previous checkpoint</span>
            <span class="cplabel">Tab</span>
            <span class="cpmono">{cp.tab ?? 'not recorded'}</span>
            <span class="cplabel">Session</span>
            <span class="cpmono">{cp.session ?? 'not recorded'}</span>
            <span class="cplabel">Commit</span>
            <span class="cpmono">{cp.commit}</span>
            <span class="cplabel">Checkpoint</span>
            <span class="cpmono">{cp.id} · seq {cp.seq}</span>
          </div>
          <p class="cpnote">
            Checkpoints live in the shadow repo under its own retention, not in
            the activity history — clearing this feed leaves them untouched, and
            restoring one is done from the Timeline.
          </p>
        </div>
        <footer class="detail-actions">
          <button type="button" class="detail-goto" onclick={() => openCheckpoint(cp)}
            >Open in Timeline</button
          >
          <button type="button" class="detail-dismiss" onclick={closeDetail}>Close</button>
        </footer>
      {:else}
        <header class="detail-head">
          <div class="detail-title">Checkpoint not available</div>
          <button type="button" class="detail-close icon" onclick={closeDetail} aria-label="Close"
            >×</button
          >
        </header>
        <div class="detail-meta">
          It has aged out of the shadow repo's retention, or checkpoints were
          turned off since this row was drawn.
        </div>
      {/if}
    {:else if detail}
      <header class="detail-head">
        <div class="detail-title">
          <span class="ekind {detail.kind}">{detail.kind}</span>
          <span class="detail-tool">{detail.tool}</span>
          <StatusChip status={rowStatus(detail)} />
        </div>
        <button type="button" class="detail-close icon" onclick={closeDetail} aria-label="Close"
          >×</button
        >
      </header>
      <div class="detail-meta">
        {fmtTime(detail.ts_ms)} · {detail.source} · {rowMeta(detail)}{#if detail.target}&nbsp;·
          <span title={detail.target}>{detail.target}</span>{/if}
      </div>
      <div class="detail-attr">
        <span class="eattr attr-{attrState(detail.tab)}" title={attrTitle(detail.tab)}
          >{attrLabel(detail.tab)}</span
        >
        <span class="detail-session">
          session: {detail.session ?? 'not recorded'}
        </span>
      </div>
      <div class="detail-body">
        <div class="payload">
          <div class="payload-head">Request</div>
          <pre>{detail.request || '(not captured)'}</pre>
        </div>
        <div class="payload">
          <div class="payload-head">Response</div>
          <pre>{detail.response || '(not captured)'}</pre>
        </div>
      </div>
      <footer class="detail-actions">
        <button
          type="button"
          class="detail-delete"
          onclick={() => {
            if (detail) void removeEntry(detail.id);
          }}
        >Delete entry</button>
        <button type="button" class="detail-dismiss" onclick={closeDetail}>Close</button>
      </footer>
    {:else if detailMissing}
      <header class="detail-head">
        <div class="detail-title">Event not found</div>
        <button type="button" class="detail-close icon" onclick={closeDetail} aria-label="Close"
          >×</button
        >
      </header>
      <div class="detail-meta">This event was deleted or has aged out of the history.</div>
    {:else}
      <div class="detail-meta">Loading…</div>
    {/if}
  </div>
{/if}

<style>
  .events {
    /* Sit ABOVE the pane's absolutely-positioned (empty) terminal slot, the
       same convention as ToolActivityView — otherwise that transparent slot
       paints on top and swallows every click. */
    position: absolute;
    inset: 0;
    overflow-y: auto;
    padding: 16px;
    font-size: 13px;
    color: var(--text-primary, #ddd);
    box-sizing: border-box;
    display: flex;
    flex-direction: column;
  }
  header {
    display: flex;
    align-items: baseline;
    justify-content: space-between;
    gap: 12px;
    margin-bottom: 10px;
  }
  header h2 {
    margin: 0;
    font-size: 15px;
  }
  .head-right {
    display: flex;
    align-items: center;
    gap: 10px;
  }
  .count {
    font-size: 11px;
    opacity: 0.65;
  }
  .clear-btn {
    border: 1px solid var(--border-subtle, #3a3a3a);
    border-radius: 6px;
    background: transparent;
    color: var(--text-primary, #ddd);
    font-size: 11px;
    padding: 2px 10px;
    cursor: pointer;
    opacity: 0.75;
  }
  .clear-btn:hover {
    opacity: 1;
    background: rgba(255, 255, 255, 0.06);
  }
  /* Armed state: the second click clears for real. */
  .clear-btn.arm {
    color: var(--text-danger-soft, #ffb4ab);
    border-color: var(--text-danger-soft, #ffb4ab);
    opacity: 1;
  }
  .filters {
    display: flex;
    align-items: flex-end;
    gap: 10px;
    flex-wrap: wrap;
    margin-bottom: 10px;
  }
  .filters label {
    display: flex;
    flex-direction: column;
    gap: 3px;
    font-size: 11px;
    opacity: 0.85;
  }
  .filters select {
    background: var(--surface-3, #1e1e1e);
    color: var(--text-primary, #ddd);
    border: 1px solid var(--border-subtle, #3a3a3a);
    border-radius: 6px;
    padding: 3px 6px;
    font-size: 12px;
    min-width: 10rem;
  }
  .reset {
    border: 1px solid var(--border-subtle, #3a3a3a);
    border-radius: 6px;
    background: transparent;
    color: var(--text-primary, #ddd);
    font-size: 12px;
    padding: 4px 12px;
    cursor: pointer;
    opacity: 0.8;
  }
  .reset:hover:not(:disabled) {
    opacity: 1;
    background: rgba(255, 255, 255, 0.06);
  }
  .reset:disabled {
    opacity: 0.35;
    cursor: default;
  }
  .card {
    border: 1px solid var(--border-subtle, #3a3a3a);
    border-radius: 8px;
    padding: 8px 12px 12px;
    background: var(--surface-card, #1e1e1e);
  }
  .feed {
    --erow-h: 1.55rem;
    /* Fill the rest of the pane (the container is a flex column); the row
       list below scrolls internally, so new rows never jump the layout. */
    flex: 1;
    min-height: calc(8 * var(--erow-h) + 6rem);
    display: flex;
    flex-direction: column;
  }
  .rows {
    display: flex;
    flex-direction: column;
    flex: 1;
    min-height: 0;
    overflow-y: auto;
  }
  .empty {
    opacity: 0.6;
    font-style: italic;
    padding: 8px 4px;
  }
  .erow {
    display: grid;
    /* Set per-instance on the .feed card: the visible-column set and the
       user's drag-resized widths compose `gridTemplate` in the script. */
    grid-template-columns: var(--ecols);
    align-items: center;
    gap: 8px;
    height: var(--erow-h);
    box-sizing: border-box;
    padding: 0 4px;
    border-bottom: 1px solid var(--border-faint, #2a2a2a);
    font-size: 0.86em;
    white-space: nowrap;
    cursor: pointer;
  }
  .erow > span {
    overflow: hidden;
    text-overflow: ellipsis;
  }
  .erow.head {
    cursor: default;
    font-weight: 600;
    opacity: 0.55;
    text-transform: uppercase;
    font-size: 0.78em;
    border-bottom-color: var(--border-subtle, #3a3a3a);
  }
  .erow.head > span {
    /* The grip hangs into the 8px column gap, so header cells must not clip. */
    position: relative;
    overflow: visible;
  }
  .grip {
    position: absolute;
    top: -2px;
    bottom: -2px;
    right: -8px;
    width: 9px;
    cursor: col-resize;
    /* The grip itself is the pointer-capture target during a drag. */
    touch-action: none;
  }
  .grip::after {
    content: '';
    position: absolute;
    top: 2px;
    bottom: 2px;
    left: 4px;
    width: 1px;
    background: var(--border-subtle, #3a3a3a);
  }
  .grip:hover::after,
  .grip:active::after {
    background: var(--text-info, #58a6ff);
  }
  .colmenu-wrap {
    position: relative;
  }
  .colmenu-backdrop {
    position: fixed;
    inset: 0;
    z-index: 60;
  }
  .colmenu {
    position: absolute;
    top: calc(100% + 4px);
    right: 0;
    z-index: 61;
    display: flex;
    flex-direction: column;
    gap: 2px;
    min-width: 9rem;
    padding: 6px 8px;
    background: var(--surface-3, #1e1e1e);
    border: 1px solid var(--border-subtle, #3a3a3a);
    border-radius: 6px;
    box-shadow: var(--shadow-lg, 0 8px 32px rgba(0, 0, 0, 0.5));
  }
  .colopt {
    display: flex;
    align-items: center;
    gap: 6px;
    font-size: 12px;
    padding: 2px 2px;
    cursor: pointer;
    white-space: nowrap;
  }
  .colopt:has(input:disabled) {
    opacity: 0.5;
    cursor: default;
  }
  .colopt input {
    margin: 0;
  }
  .erow:not(.head):hover,
  .erow:not(.head):focus-visible {
    background: rgba(255, 255, 255, 0.05);
    outline: none;
  }
  .ekind,
  .esrc {
    text-transform: uppercase;
    font-size: 0.82em;
    font-weight: 600;
    opacity: 0.85;
  }
  /* Feed-kind accents, matching the Tool Activity feed so the same row reads
     the same in both views. */
  .ekind.graph {
    color: var(--text-info, #58a6ff);
  }
  .ekind.offload {
    color: var(--text-success, #3fb950);
  }
  /* Server lifecycle rows sit beside the offload task rows they explain, so
     they take the same green at lower saturation: same family (this is the
     offload backend), plainly not the same feed (this is the process, not a
     task it ran). */
  .ekind.offload_server {
    color: color-mix(in srgb, var(--text-success, #3fb950) 55%, var(--text-tertiary, #9aa0aa));
  }
  .ekind.audit {
    color: color-mix(in srgb, var(--warning, #f0a020) 60%, var(--danger, #f06080));
  }
  .ekind.mcp {
    color: var(--accent-purple, #d2a8ff);
  }
  .ekind.injection_flag {
    color: var(--danger, #f06080);
    background: color-mix(in srgb, var(--danger, #f06080) 14%, transparent);
    border-radius: 3px;
    padding: 0 3px;
  }
  /* Synthetic checkpoint rows (#51): teal, so they read as neither the graph
     blue nor the offload green next to them. */
  .ekind.checkpoint {
    color: color-mix(in srgb, var(--text-info, #58a6ff) 45%, var(--text-success, #3fb950));
  }
  /* The Status cell of a checkpoint row — a fact (what it captured), not a
     call outcome, so deliberately NOT a StatusChip word. */
  .cpfiles {
    font-size: 0.82em;
    opacity: 0.75;
    cursor: help;
  }
  .esrc.claude {
    color: var(--text-info, #58a6ff);
  }
  .esrc.opencode {
    color: var(--accent-purple, #d2a8ff);
  }
  .esrc.offload {
    color: var(--text-success, #3fb950);
  }
  .esrc.read_advisor {
    color: var(--text-warning, #e3b341);
  }
  .esrc.auto_check,
  .esrc.audit {
    color: color-mix(in srgb, var(--warning, #f0a020) 60%, var(--danger, #f06080));
  }
  .etool {
    font-family: var(--font-mono, monospace);
  }
  .etarget {
    opacity: 0.85;
  }
  /* The status cell is `StatusChip` now (#48, M-24) — twelve states with twelve
     words instead of this file's five, and one copy of the treatments, shared
     with the Tool Activity feed so the two tabs cannot describe the same row
     differently. Its own scoped styles own the colours. */

  /* ── The four attribution states ──────────────────────────────────────
     Four visually SEPARATE treatments, on purpose. A real tab is the only
     one that reads as a solid identity; `unrecognized` is loud because it is
     an id that names nothing; and the two "no tab id" states are kept apart
     because "nobody was on a tab" (headless — a fact) and "we weren't
     recording it" (unattributed — an absence) are different answers, and only
     one of them is evidence. Dashed vs solid is the carrier of that
     difference, so both keep a border. */
  .eattr {
    font-size: 0.82em;
    border-radius: 3px;
    padding: 0 4px;
    border: 1px solid transparent;
    overflow: hidden;
    text-overflow: ellipsis;
  }
  .eattr.attr-tab {
    color: var(--text-info, #58a6ff);
    border-color: color-mix(in srgb, var(--text-info, #58a6ff) 45%, transparent);
    font-weight: 600;
  }
  .eattr.attr-unrecognized {
    color: var(--danger, #f06080);
    border-color: color-mix(in srgb, var(--danger, #f06080) 55%, transparent);
    background: color-mix(in srgb, var(--danger, #f06080) 12%, transparent);
    font-weight: 600;
  }
  .eattr.attr-headless {
    /* Solid border: this is a positive statement about the caller. */
    color: var(--text-primary, #ddd);
    border-color: var(--border-subtle, #3a3a3a);
    opacity: 0.75;
  }
  .eattr.attr-unattributed {
    /* Dashed + italic: an ABSENCE of information, never mistakable for the
       headless chip above. */
    color: var(--text-primary, #ddd);
    border-style: dashed;
    border-color: var(--border-faint, #2a2a2a);
    font-style: italic;
    opacity: 0.5;
  }
  .esession {
    font-family: var(--font-mono, monospace);
    font-size: 0.82em;
    opacity: 0.75;
  }
  .esession.none {
    opacity: 0.35;
  }

  /* ── Detail popup — dialog conventions per ToolActivityView. ────────── */
  .backdrop {
    position: fixed;
    inset: 0;
    background: rgba(0, 0, 0, 0.5);
    z-index: 100;
  }
  .detail-card {
    position: fixed;
    top: 50%;
    left: 50%;
    transform: translate(-50%, -50%);
    display: flex;
    flex-direction: column;
    width: min(820px, calc(100vw - 40px));
    max-height: min(80vh, 900px);
    background: var(--surface-3, #1e1e1e);
    border: 1px solid var(--border-subtle, #3a3a3a);
    border-radius: var(--radius-lg, 10px);
    padding: 14px 16px;
    color: var(--text-primary, #ddd);
    z-index: 101;
    box-shadow: var(--shadow-lg, 0 8px 32px rgba(0, 0, 0, 0.5));
    box-sizing: border-box;
    font-size: 13px;
  }
  .detail-head {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 12px;
    margin: 0 0 4px;
  }
  .detail-title {
    display: flex;
    align-items: center;
    gap: 8px;
    font-weight: 600;
    min-width: 0;
  }
  .detail-tool {
    font-family: var(--font-mono, monospace);
  }
  .detail-close {
    border: none;
    background: transparent;
    color: var(--text-primary, #ddd);
    font-size: 16px;
    line-height: 1;
    cursor: pointer;
    opacity: 0.7;
    padding: 2px 6px;
  }
  .detail-close:hover {
    opacity: 1;
  }
  .detail-meta {
    font-size: 11px;
    opacity: 0.7;
    margin-bottom: 6px;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .detail-attr {
    display: flex;
    align-items: center;
    gap: 10px;
    margin-bottom: 10px;
    font-size: 11px;
  }
  .detail-session {
    opacity: 0.7;
    font-family: var(--font-mono, monospace);
    overflow: hidden;
    text-overflow: ellipsis;
  }
  .detail-body {
    flex: 1;
    min-height: 0;
    overflow-y: auto;
    display: flex;
    flex-direction: column;
    gap: 10px;
  }
  .payload-head {
    font-size: 11px;
    font-weight: 600;
    text-transform: uppercase;
    opacity: 0.7;
    margin-bottom: 4px;
  }
  .payload pre {
    margin: 0;
    padding: 8px 10px;
    background: var(--surface-sunken, rgba(0, 0, 0, 0.3));
    border: 1px solid var(--border-faint, #2a2a2a);
    border-radius: 6px;
    font-size: 12px;
    white-space: pre-wrap;
    word-break: break-word;
    max-height: 40vh;
    overflow-y: auto;
  }
  .detail-actions {
    display: flex;
    justify-content: flex-end;
    gap: 8px;
    margin-top: 12px;
  }
  .detail-actions button {
    padding: 4px 12px;
    border-radius: 6px;
    border: 1px solid var(--border-subtle, #3a3a3a);
    background: transparent;
    color: var(--text-primary, #ddd);
    font-size: 12px;
    cursor: pointer;
  }
  .detail-actions button:hover {
    background: rgba(255, 255, 255, 0.06);
  }
  .detail-delete {
    color: var(--text-danger-soft, #ffb4ab);
    border-color: var(--border-danger-soft, rgba(255, 180, 171, 0.5));
  }
  .detail-delete:hover {
    border-color: var(--text-danger-soft, #ffb4ab);
  }
  /* The checkpoint popup's jump to the Timeline — the action the row's click
     used to be. Teal, matching the kind chip, so it reads as "go to the
     checkpoint" rather than as a second dismiss. */
  .detail-goto {
    /* The same mix `.ekind.checkpoint` wears, so the button reads as "go to
       the checkpoint" rather than as a second dismiss. */
    color: color-mix(in srgb, var(--text-info, #58a6ff) 45%, var(--text-success, #3fb950));
    border-color: color-mix(in srgb, currentColor 50%, transparent);
    /* Pushed to the left edge: it is the action, Close is the way out. */
    margin-right: auto;
  }
  .detail-goto:hover {
    border-color: currentColor;
  }
  /* The checkpoint field grid — the record behind the row's columns. */
  .cpgrid {
    display: grid;
    grid-template-columns: auto 1fr;
    gap: 3px 12px;
    font-size: 12px;
  }
  .cpgrid .cplabel {
    color: var(--text-tertiary, #9aa0aa);
    text-transform: uppercase;
    font-size: 10px;
    letter-spacing: 0.03em;
    align-self: baseline;
  }
  .cpgrid .cpmono {
    font-family: var(--font-mono, monospace);
    word-break: break-all;
  }
  .cpnote {
    margin: 12px 0 0;
    font-size: 11px;
    line-height: 1.4;
    opacity: 0.7;
  }
</style>
