<script lang="ts">
  // The read-only, app-rendered Events tab (#51) — the same persistent
  // activity store the Tool Activity tab's "Activities" section shows, but
  // rendered as the ATTRIBUTION view: every row says which tab and which
  // harness session it belongs to, and the feed is filterable by kind,
  // source/screen and tab.
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
    activityDetail,
    activityList,
    attributionId,
    attributionState,
    filterEntries,
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
    type AttributionState,
    type FeedFilter,
  } from './activity';
  import StatusChip from './StatusChip.svelte';
  import { fmtTime } from './format';
  import { fmtTok } from './usageMath';
  import { EVENTS_TAB_ID } from './tabs/types';
  import { isAppViewVisible, onAppViewShown } from './appViewVisibility';

  // ── Feed poll ─────────────────────────────────────────────────────────
  // `$state.raw`: the feed holds up to ~1.4k rows (the per-lane caps in
  // `crate::activity`) and is only ever REPLACED, never mutated in place, so
  // plain `$state` would deep-proxy every row on every poll for nothing.
  let entries = $state.raw<ActivityEntry[]>([]);
  let poll: ReturnType<typeof setInterval> | null = null;

  async function refresh(): Promise<void> {
    try {
      const list = await activityList();
      // Reuse the rows already held (see `mergeEntries`) — a plain
      // `entries = list` re-renders the whole feed on every 2s poll.
      entries = mergeEntries(entries, list);
    } catch {
      /* backend unavailable mid-teardown — keep whatever we have */
    }
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

  // Options are derived from the feed rather than hardcoded: `kind` gains
  // variants over time and `source` is free text (it names the offload
  // backend, or the V32 screen that fired), so anything this build doesn't
  // know still shows up and stays selectable.
  const kindOptions = $derived(
    [...new Set(entries.map((e) => e.kind))].sort((a, b) => a.localeCompare(b)),
  );
  const sourceOptions = $derived(
    [...new Set(entries.map((e) => e.source))].sort((a, b) => a.localeCompare(b)),
  );
  // ONLY genuine `{ tab: x }` ids become tab options. An `{ unrecognized: x }`
  // row names no configured tab, so offering it here would put a phantom tab
  // in the picker of the view whose entire job is attribution; those rows are
  // reachable through the dedicated "unrecognized" option instead.
  const tabOptions = $derived(
    [
      ...new Set(
        entries
          .filter((e) => attributionState(e.tab) === 'tab')
          .map((e) => attributionId(e.tab) as string),
      ),
    ].sort((a, b) => a.localeCompare(b)),
  );

  function resetFilters(): void {
    filter = { ...NO_FILTER };
  }

  // ── Detail popup ──────────────────────────────────────────────────────
  let detailOpen = $state(false);
  let detail = $state<ActivityRecord | null>(null);
  let detailMissing = $state(false);
  // Fetch token: a click on row B while row A's (slower) fetch is in flight
  // must not let A's late response overwrite the popup.
  let detailSeq = 0;

  async function openDetail(id: number): Promise<void> {
    const seq = ++detailSeq;
    detailOpen = true;
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

  function closeDetail(): void {
    detailSeq += 1; // invalidate any in-flight fetch
    detailOpen = false;
    detail = null;
    detailMissing = false;
  }

  function onKeyDown(e: KeyboardEvent): void {
    if (e.key === 'Escape' && detailOpen) {
      e.preventDefault();
      closeDetail();
    }
  }

  // Keep-alive (appViews.ts): the poll idles while the tab is off-screen and
  // a fresh refresh runs the moment it comes back. A periodic job in a
  // reserved app view that does NOT gate on appViewVisibility keeps burning
  // IPC forever once the tab has been opened once — a known bug class here.
  const unsubShown = onAppViewShown(EVENTS_TAB_ID, () => void refresh());

  onMount(() => {
    void refresh();
    poll = setInterval(() => {
      if (isAppViewVisible(EVENTS_TAB_ID)) void refresh();
    }, 2000);
    window.addEventListener('keydown', onKeyDown);
  });

  onDestroy(() => {
    if (poll) clearInterval(poll);
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
    return e.kind === 'audit'
      ? `${dur} · ${e.chars} findings`
      : `${dur} · ${fmtTok(e.chars)} chars`;
  }

  // The attribution cell. Each of the four states gets its OWN label and its
  // own styling — see the `Attribution` doc in activity.ts for why collapsing
  // any two of them is a bug, and .attr-* below for how they stay visually
  // distinct.
  function attrLabel(e: ActivityEntry): string {
    const state = attributionState(e.tab);
    switch (state) {
      case 'tab':
        return attributionId(e.tab) as string;
      case 'unrecognized':
        // Never rendered as a bare id: the id is shown (it is evidence) but
        // always behind the word that says it names no tab that exists.
        return `unrecognized: ${attributionId(e.tab)}`;
      case 'headless':
        return 'no tab';
      default:
        return 'not recorded';
    }
  }

  function attrTitle(e: ActivityEntry): string {
    switch (attributionState(e.tab)) {
      case 'tab':
        return `Tab ${attributionId(e.tab)}`;
      case 'unrecognized':
        return `Unrecognized id "${attributionId(e.tab)}" — it names no configured tab, so this row is NOT attributable to a tab`;
      case 'headless':
        return 'No tab: a headless caller (claude -p, cron, a worker task, or cImp’s own internal work). A fact about the caller, not missing data.';
      default:
        return 'Not recorded: the writer did not know which tab this was, or the row predates the attribution column. Different from "no tab".';
    }
  }

  function attrState(e: ActivityEntry): AttributionState {
    return attributionState(e.tab);
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
    <span class="count">
      {shown.length === entries.length
        ? `${entries.length} events`
        : `${shown.length} of ${entries.length} events`}
    </span>
  </header>

  <p class="caveat">
    Every recorded tool call, offload run, audit scan and injection-containment
    event, newest first, with the tab and harness session it belongs to. Click a
    row for the captured request and response.
  </p>

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
  </div>

  <section class="card feed">
    <div class="erow head">
      <span>Time</span>
      <span>Kind</span>
      <span>Source</span>
      <span>Tool</span>
      <span>Target</span>
      <span>Status</span>
      <span>Tab</span>
      <span>Session</span>
    </div>
    {#if entries.length === 0}
      <div class="empty">
        No events recorded yet — query the graph from a Claude tab or run an
        offload_task and it shows up here.
      </div>
    {:else if shown.length === 0}
      <div class="empty">No events match the current filters.</div>
    {:else}
      <div class="rows">
        {#each shown as r (r.id)}
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
            <span class="etime">{fmtTime(r.ts_ms)}</span>
            <span class="ekind {r.kind}">{r.kind}</span>
            <span class="esrc{srcClass(r.source)}" title={r.source}>{r.source}</span>
            <span class="etool" title={r.tool}>{rowTool(r)}</span>
            <span class="etarget" title={r.target}>{r.target}</span>
            <StatusChip status={rowStatus(r)} />
            <span class="eattr attr-{attrState(r)}" title={attrTitle(r)}>{attrLabel(r)}</span>
            <span class="esession" class:none={!r.session} title={r.session ?? 'No session recorded'}
              >{shortSession(r.session)}</span
            >
          </div>
        {/each}
      </div>
    {/if}
  </section>
</div>

{#if detailOpen}
  <div class="backdrop" onclick={closeDetail} role="presentation"></div>
  <div class="detail-card" role="dialog" aria-label="Event detail">
    {#if detail}
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
        <span class="eattr attr-{attrState(detail)}" title={attrTitle(detail)}
          >{attrLabel(detail)}</span
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
    margin-bottom: 4px;
  }
  header h2 {
    margin: 0;
    font-size: 15px;
  }
  .count {
    font-size: 11px;
    opacity: 0.65;
  }
  .caveat {
    font-size: 11px;
    opacity: 0.65;
    margin: 2px 0 10px;
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
    /* Status column widened from 4.5rem: M-24 replaced five words with twelve
       and `unscreened` is the longest of them. The slack comes out of the
       flexible target column, not out of another fixed one. */
    grid-template-columns: 5.5rem 5rem 6rem 9rem 1fr 6.5rem 9rem 5.5rem;
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
</style>
