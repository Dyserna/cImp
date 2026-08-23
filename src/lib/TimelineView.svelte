<script lang="ts">
  // V13 Phase C — the Timeline section (`WorkbenchView`'s "Timeline" tab):
  // the checkpoint list with Diff-vs-now / Restore row actions. Only
  // rendered once `settings.workbench.checkpoints` is on (WorkbenchView's
  // banner explains the toggle otherwise) — this component assumes the
  // feature is enabled and just deals with fetching/rendering/acting on
  // whatever checkpoints already exist.
  //
  // V33 step 5 — the evidence surface. The list is no longer homogeneous: a
  // contamination event (the moment a tab's conversation stopped being clean)
  // is merged into it by time, so the user can see WHICH checkpoint was live
  // when it happened and act from there. Everything that decides what those
  // rows claim lives in `timeline.ts`, which has a test harness; this file
  // renders what that module returns and adds no rules of its own.
  //
  // Three things this view is careful about, all of them "do not overstate":
  //   • the nearest preceding checkpoint often belongs to a DIFFERENT tab (per
  //     tab throttling is not per tab guaranteeing), so every row says whose it
  //     is rather than offering it as this tab's restore point;
  //   • a contamination row can age out of the activity feed, so a tab flagged
  //     with no row is announced, not rendered as an empty list;
  //   • the flag and the latch are separate holds — see `latchAlsoHoldsMemory`.
  import {
    workbenchCheckpoints,
    workbenchCheckpointDiff,
    workbenchCheckpointNow,
    workbenchCheckpointsVersion,
    timelineCheckpointRequest,
    takeTimelineHighlightRoute,
    FULL_FILE_CONTEXT,
    type Checkpoint,
    type FileDiff,
  } from './workbench';
  import { onMount, untrack } from 'svelte';
  import { openRestoreCheckpointDialog, dialogState } from './dialog/store';
  import { errorMessage } from './errors';
  import CheckpointDiffView from './CheckpointDiffView.svelte';
  import { WORKBENCH_TAB_ID, type TabId } from './tabs/types';
  import { onAppViewShown, isAppViewVisible } from './appViewVisibility';
  import { loadViewString, saveViewString } from './viewSection';
  import { latchByTab, applyLatchOverride, type LatchRow } from './latch';
  import { findHarness, harnesses } from './harness';
  import {
    fetchContaminationEvents,
    buildTimelineRows,
    evidenceNotices,
    linkLine,
    restoreTarget,
    clearedLine,
    rowIcon,
    rowTitle,
    checkpointSource,
    latchAlsoHoldsMemory,
    type ContaminationEvent,
    type TimelineRow,
  } from './timeline';

  let checkpoints = $state<Checkpoint[]>([]);
  let loading = $state(false);
  let loadError = $state<string | null>(null);
  let creatingNow = $state(false);

  // Step 5a: the contamination lifecycle, from its own command. Its failure is
  // tracked SEPARATELY from `loadError` — a failed evidence read must not blank
  // the checkpoint list, and an intact checkpoint list must not imply the
  // evidence was read.
  let events = $state<ContaminationEvent[]>([]);
  let evidenceRoot = $state('');
  let eventsError = $state<string | null>(null);

  // The open "Diff vs now" persists (viewSection.ts) like the sibling
  // sections' expansions — a stale id matches no row and renders nothing;
  // refresh() fetches the diff for a restored id (toggleDiff only fetches
  // on click).
  let openDiffFor = $state<string | null>(loadViewString('timeline', 'open-diff'));
  $effect(() => saveViewString('timeline', 'open-diff', openDiffFor));
  let diffFiles = $state<Map<string, FileDiff[]>>(new Map());
  let diffErrors = $state<Map<string, string>>(new Map());
  let diffLoading = $state<Set<string>>(new Set());

  // Overlap guard for BOTH refresh flavours — `loading` alone can't carry it
  // because quiet (poll-driven) refreshes deliberately never set it, so the
  // toolbar's Refresh button doesn't flash disabled on every tick.
  let refreshing = false;

  async function refresh(quiet = false): Promise<void> {
    if (refreshing) return;
    refreshing = true;
    if (!quiet) loading = true;
    loadError = null;
    try {
      // Newest first — the shadow module returns oldest-first (matches
      // `git log`'s natural iteration order for the backing commits).
      checkpoints = (await workbenchCheckpoints()).slice().reverse();
      if (openDiffFor !== null && checkpoints.some((c) => c.id === openDiffFor)) {
        void loadDiffFor(openDiffFor);
      }
    } catch (e) {
      loadError = errorMessage(e);
    } finally {
      if (!quiet) loading = false;
    }
    await refreshEvidence();
    refreshing = false;
  }

  /// Step 5a. Keeps the last known events on failure — a read that failed is
  /// not evidence that nothing happened — and reports the failure as its own
  /// notice (`evidenceNotices`) so the view never goes quiet about it.
  async function refreshEvidence(): Promise<void> {
    try {
      const feed = await fetchContaminationEvents();
      evidenceRoot = feed.root;
      events = feed.events;
      eventsError = null;
    } catch (e) {
      eventsError = errorMessage(e);
    }
  }

  // Refetch after a restore (or a future "checkpoint now" from elsewhere)
  // bumps the shared version store — see `workbench.ts`'s doc comment. The
  // effect's initial run also covers the first load on mount (no separate
  // `onMount` fetch — that would fire a duplicate `workbench_checkpoints`
  // call every time the view mounts).
  $effect(() => {
    void $workbenchCheckpointsVersion;
    void refresh();
  });

  // Keep-alive (appViews.ts): auto-checkpoints that landed while the tab was
  // off-screen don't bump the version store — refetch when the tab returns
  // (the pre-keep-alive remount used to cover this) and, since auto-refresh,
  // every POLL_MS while it stays open. The poll gates on appViewVisibility
  // (the keep-alive cost rule: a detached view keeps polling forever
  // otherwise) and skips while a fetch or a latch action is in flight.
  const POLL_MS = 5000;
  onMount(() => {
    const unsub = onAppViewShown(WORKBENCH_TAB_ID, () => void refresh());
    const poll = setInterval(() => {
      if (isAppViewVisible(WORKBENCH_TAB_ID) && !actionBusy) void refresh(true);
    }, POLL_MS);
    return () => {
      unsub();
      clearInterval(poll);
      if (highlightTimer) clearTimeout(highlightTimer);
    };
  });

  // ── Events-tab jump target (#51) ──────────────────────────────────────────
  // A checkpoint-row click in the Events tab lands here: highlight that row
  // for HIGHLIGHT_MS and scroll it into view once the list contains it (the
  // request can arrive before this component has fetched — or even mounted).
  // One-shot per nonce via the module-scope latch in workbench.ts, so a
  // remount inside the window does not replay a finished jump.
  const HIGHLIGHT_MS = 20_000;
  let highlightId = $state<string | null>(null);
  let highlightTimer: ReturnType<typeof setTimeout> | null = null;
  let highlightScrolled = false;
  let rootEl: HTMLElement | null = null;

  $effect(() => {
    const req = $timelineCheckpointRequest;
    if (!req || !takeTimelineHighlightRoute(req.nonce)) return;
    if (highlightTimer) clearTimeout(highlightTimer);
    highlightId = req.id;
    highlightScrolled = false;
    highlightTimer = setTimeout(() => {
      highlightId = null;
      highlightTimer = null;
    }, HIGHLIGHT_MS);
  });

  // Runs after every DOM update while a highlight is pending: the moment the
  // row exists (initial fetch, or a later poll if the checkpoint was created
  // between the Events poll and the click), scroll to it — once. A checkpoint
  // GC'd in the meantime simply never matches and the highlight quietly
  // expires.
  $effect(() => {
    if (highlightId === null || highlightScrolled) return;
    if (!rows.some((r) => r.kind === 'checkpoint' && r.checkpoint.id === highlightId)) return;
    const el = rootEl?.querySelector(`[data-cp="${highlightId}"]`);
    if (el) {
      highlightScrolled = true;
      el.scrollIntoView({ block: 'center', behavior: 'smooth' });
    }
  });

  const latchRows = $derived(
    Object.values($latchByTab).filter((r): r is LatchRow => r !== undefined),
  );

  // Step 5a: contamination can land between the poll's ticks above, so the
  // evidence read also rides the latch poll that already runs app-wide: when
  // the SET of contaminated tabs changes, the evidence is refetched once,
  // immediately. Not a poll — a change signal — and skipped while the view is
  // detached, because `onAppViewShown` above already refreshes on return.
  let lastContaminationSig = '';
  $effect(() => {
    const sig = latchRows
      .filter((r) => r.contaminated)
      .map((r) => r.tab)
      .sort()
      .join(',');
    untrack(() => {
      if (sig === lastContaminationSig) return;
      lastContaminationSig = sig;
      if (isAppViewVisible(WORKBENCH_TAB_ID)) void refreshEvidence();
    });
  });

  const rows = $derived(buildTimelineRows(checkpoints, events, evidenceRoot));
  const notices = $derived(
    evidenceNotices({ events, root: evidenceRoot, latch: latchRows, error: eventsError }),
  );

  // ── Step 5c: the two actions on a contamination row ──────────────────────

  /// The tab's live latch row — the backend owns which moves are legal and
  /// publishes them (`can_clear`), exactly as `TaintMenu` reads them. Absent
  /// means the tab has had no gated call this run, so there is nothing to act on.
  function latchFor(tab: string | null): LatchRow | undefined {
    return tab === null ? undefined : $latchByTab[tab as TabId];
  }

  /// Two clicks, like `TaintMenu`'s: this one releases containment on the
  /// user's judgement alone.
  let confirmingClear = $state<string | null>(null);
  let actionBusy = $state(false);
  let actionError = $state<string | null>(null);

  async function run(row: LatchRow, action: 'clear_contamination' | 'await_session_clear') {
    if (actionBusy) return;
    actionBusy = true;
    actionError = null;
    try {
      await applyLatchOverride(row.tab, row.consumer, action);
      confirmingClear = null;
      await refreshEvidence();
    } catch (e) {
      // The backend's own message, verbatim — a control that appears to do
      // nothing when clicked is worse than one that explains why it declined.
      actionError = errorMessage(e);
    } finally {
      actionBusy = false;
    }
  }

  /// Step 5c's restore, in two halves.
  ///
  /// `RestoreCheckpointDialog` is opened through the global dialog store and
  /// has no completion callback — but it bumps `workbenchCheckpointsVersion`
  /// on success, and only on success, and this view already subscribes to that
  /// bus. So the `await_session_clear` arm rides the bump rather than a fourth
  /// dispatch mechanism. Arming BEFORE the restore was the alternative and is
  /// wrong: a cancelled dialog would leave the audit trail claiming a restore
  /// that never happened.
  let pendingArm = $state<LatchRow | null>(null);
  let armNote = $state<string | null>(null);
  let lastVersion = -1;

  function restoreFrom(id: string, latch: LatchRow | undefined): void {
    pendingArm = latch ?? null;
    armNote = null;
    openRestoreCheckpointDialog(id);
  }

  $effect(() => {
    const v = $workbenchCheckpointsVersion;
    untrack(() => {
      const bumped = lastVersion >= 0 && v !== lastVersion;
      lastVersion = v;
      const arm = pendingArm;
      if (!bumped || !arm) return;
      pendingArm = null;
      void armSessionClear(arm);
    });
  });

  // The dialog closing without a bump means the user cancelled — drop the arm
  // rather than letting it fire on some later, unrelated restore.
  $effect(() => {
    const open = $dialogState.kind === 'restore-checkpoint';
    untrack(() => {
      if (!open && pendingArm) pendingArm = null;
    });
  });

  /// How to start a fresh session in a tab of `harness`, as a clause.
  ///
  /// V40 Phase F (locked decision 27): the in-session command is the harness's
  /// declared `newSessionCommand`. Three strings across this window said "run
  /// /clear in that tab" as if every harness had that command; a harness that
  /// declares none now gets the honest half of the sentence instead.
  function newSessionAdvice(harness: string): string {
    const cmd = findHarness($harnesses, harness)?.affordances.newSessionCommand;
    return cmd ? `run ${cmd} in that tab, or restart it` : 'restart that tab';
  }

  async function armSessionClear(row: LatchRow): Promise<void> {
    try {
      await applyLatchOverride(row.tab, row.consumer, 'await_session_clear');
      armNote = `Restored. ${row.consumer}:${row.tab} stays flagged until it starts a new session — ${newSessionAdvice(row.consumer)}, and the flag lifts on its own. Restoring files cannot remove injected text from the conversation, which is why it was kept.`;
      await refreshEvidence();
    } catch (e) {
      armNote = `Restored, but the contamination flag could not be armed to clear: ${errorMessage(e)} — clear it from the tab's containment badge instead.`;
    }
  }

  async function checkpointNow(): Promise<void> {
    creatingNow = true;
    try {
      await workbenchCheckpointNow();
      await refresh();
    } catch (e) {
      loadError = errorMessage(e);
    } finally {
      creatingNow = false;
    }
  }

  async function toggleDiff(id: string): Promise<void> {
    if (openDiffFor === id) {
      openDiffFor = null;
      return;
    }
    openDiffFor = id;
    await loadDiffFor(id);
  }

  async function loadDiffFor(id: string): Promise<void> {
    if (diffFiles.has(id) || diffLoading.has(id)) return;
    diffLoading.add(id);
    diffLoading = new Set(diffLoading);
    try {
      const files = await workbenchCheckpointDiff(id);
      diffFiles.set(id, files);
      diffFiles = new Map(diffFiles);
      diffErrors.delete(id);
    } catch (e) {
      diffErrors.set(id, errorMessage(e));
      diffErrors = new Map(diffErrors);
    } finally {
      diffLoading.delete(id);
      diffLoading = new Set(diffLoading);
    }
  }

  function formatTime(iso: string): string {
    const d = new Date(iso);
    return Number.isNaN(d.getTime()) ? iso : d.toLocaleString();
  }

  function formatMs(ms: number): string {
    const d = new Date(ms);
    return Number.isNaN(d.getTime()) ? String(ms) : d.toLocaleString();
  }

  /// The one-line summary of a contamination row.
  function headline(row: Extract<TimelineRow, { kind: 'contamination' }>): string {
    if (!row.opened) return 'Contamination flag cleared';
    const where = row.opened.host ? ` · ${row.opened.host}` : '';
    return `External content entered this tab — ${row.opened.tool}${where}`;
  }

  // The standing per-row explanations, shown as hover text behind the note
  // icons below rather than as paragraphs — they repeat on every row, so as
  // prose they cost more height than the rows themselves. The sentences are
  // unchanged; only where they live moved.
  const NOTE_NOT_RETAINED =
    'The event that set the flag is no longer retained, so there is nothing to correlate this to.';
  const NOTE_AWAITING_CLEAR =
    'A restore was recorded for this tab: the flag lifts once cImp sees it start a new session.';
  const NOTE_NO_TAB =
    "cImp could not read which tab this event belongs to, so it cannot offer the flag actions here. They are on the tab's own containment badge.";
  function noLiveStateNote(scope: string): string {
    return `cImp holds no live containment state for ${scope} — the tab has made no gated call this run, so there is no flag here to act on. If it is open, its containment badge is the place to check.`;
  }
  function sessionNote(session: string): string {
    return `Started in session ${session}. The flag belongs to the tab, not to that conversation — it survives the start of a new one, so a later session on this tab is covered by this same row and writes no second one.`;
  }
</script>

<div class="timeline" bind:this={rootEl}>
  <div class="toolbar">
    <button type="button" class="checkpoint-now" onclick={checkpointNow} disabled={creatingNow}>
      {creatingNow ? 'Checkpointing…' : 'Checkpoint now'}
    </button>
    <button type="button" class="refresh" onclick={() => void refresh()} disabled={loading}>Refresh</button>
  </div>

  <!-- Step 5a: what this view cannot show, said out loud. Rendered above the
       rows and independently of them — an empty list with a suppressed notice
       is the exact failure these exist to prevent. -->
  {#each notices as n (n.kind)}
    <!-- `rootless` wears the same unknown treatment as `not-retained` (#48
         F-16): both say "there is evidence this view cannot show you", which is
         not the same claim as the neutral `other-root` one. -->
    <p
      class="msg notice"
      class:err={n.kind === 'error'}
      class:unknown={n.kind === 'not-retained' || n.kind === 'rootless'}
    >
      {n.text}
    </p>
  {/each}
  {#if armNote}
    <p class="msg notice">{armNote}</p>
  {/if}
  {#if actionError}
    <p class="msg err">{actionError}</p>
  {/if}

  {#if loadError}
    <p class="msg err">Couldn't load checkpoints: {loadError}</p>
  {:else if loading && rows.length === 0}
    <p class="msg">Loading…</p>
  {:else if rows.length === 0}
    <p class="msg">
      No checkpoints yet. They're created automatically (per prompt, or after a
      burst of file activity) or on demand with "Checkpoint now" above.
    </p>
  {:else}
    <div class="rows">
      {#each rows as row (row.key)}
        {#if row.kind === 'checkpoint'}
          {@const cp = row.checkpoint}
          {@const src = checkpointSource(cp.source)}
          <div class="row" data-cp={cp.id} class:flash={highlightId === cp.id}>
            <div class="row-main">
              <span class="trigger" title={rowTitle(row)}>{rowIcon(row)}</span>
              <span class="time">{formatTime(cp.ts)}</span>
              <span class="label" title={cp.label}>{cp.label}</span>
              <span
                class="files"
                title="Files changed since the PREVIOUS checkpoint — what this one newly captured when it was taken. It is not the difference from your current tree (that is what &quot;Diff vs now&quot; shows), so the two counts rarely agree. A project's first checkpoint counts every file, since everything is new to it."
                >{cp.files_changed} file{cp.files_changed === 1 ? '' : 's'}</span
              >
              <!-- V33 (C8): the tool this checkpoint was taken in FRONT of.
                   Rendered only when present — a tool checkpoint is the rare
                   one, so an always-there "—" would add a column of dashes to
                   every row for the sake of a handful. It sits next to the
                   agent because together they read as "<harness> · <tool>": who,
                   then what they were about to run. -->
              {#if src}
                <span class="source" title={src.title}>{src.text}</span>
              {/if}
              <span class="agent">{cp.agent ?? '—'}</span>
              <span class="actions">
                <button type="button" onclick={() => void toggleDiff(cp.id)}>
                  {openDiffFor === cp.id ? 'Hide diff' : 'Diff vs now'}
                </button>
                <button type="button" class="restore" onclick={() => openRestoreCheckpointDialog(cp.id)}>
                  Restore
                </button>
              </span>
            </div>
            {#if openDiffFor === cp.id}
              <div class="row-diff">
                {#if diffLoading.has(cp.id)}
                  <p class="msg">Loading diff…</p>
                {:else if diffErrors.get(cp.id)}
                  <p class="msg err">{diffErrors.get(cp.id)}</p>
                {:else}
                  <CheckpointDiffView
                    files={diffFiles.get(cp.id) ?? []}
                    fetchFull={() => workbenchCheckpointDiff(cp.id, FULL_FILE_CONTEXT)}
                    stateKey={`timeline:${cp.id}`}
                  />
                {/if}
              </div>
            {/if}
          </div>
        {:else}
          {@const latch = latchFor(row.tab)}
          {@const memoryNote = latchAlsoHoldsMemory(latch?.latch)}
          {@const target = restoreTarget(row.link)}
          <div class="row contam" class:resolved={row.cleared !== null}>
            <div class="row-main">
              <span class="trigger" title={rowTitle(row)}>{rowIcon(row)}</span>
              <span class="time">{formatMs(row.tsMs)}</span>
              <span class="label">{headline(row)}</span>
              <span class="agent">{row.scope}</span>
            </div>
            <div class="row-detail">
              <!-- Every STANDING explanation on a contamination row is an
                   icon whose hover (and aria-label) carries the full
                   sentence — as prose they repeat on every row and cost more
                   height than the rows themselves. The icons differ per
                   note so rows can be told apart without hovering. Transient
                   text (the clear confirmation) and data (the cleared line)
                   stay written out. The sentences are the same ones as
                   before; step 5d's still comes verbatim from `timeline.ts`,
                   shared with the popover. -->
              <p class="line notes">
                <!-- The join's limits, on the row they apply to. `linkLine`
                     names whose checkpoint this is; it is never presented as
                     "this tab's restore point" unless it is one. -->
                <span class="note-icon" title={linkLine(row.link)} role="img" aria-label={linkLine(row.link)}
                  >🔗</span
                >
                {#if row.opened?.session}
                  {@const t = sessionNote(row.opened.session)}
                  <span class="note-icon" title={t} role="img" aria-label={t}>💬</span>
                {/if}
                {#if row.opened === null}
                  <span
                    class="note-icon dim"
                    title={NOTE_NOT_RETAINED}
                    role="img"
                    aria-label={NOTE_NOT_RETAINED}>❔</span
                  >
                {/if}
                {#if row.cleared === null}
                  {#if memoryNote}
                    <span class="note-icon warn" title={memoryNote} role="img" aria-label={memoryNote}
                      >⚠</span
                    >
                  {/if}
                  {#if latch?.can_clear && latch.awaiting_session_clear}
                    <span
                      class="note-icon"
                      title={NOTE_AWAITING_CLEAR}
                      role="img"
                      aria-label={NOTE_AWAITING_CLEAR}>⏳</span
                    >
                  {/if}
                  {#if !latch?.can_clear}
                    {@const t = row.tab === null ? NOTE_NO_TAB : noLiveStateNote(row.scope)}
                    <span class="note-icon dim" title={t} role="img" aria-label={t}>ℹ</span>
                  {/if}
                {/if}
              </p>
              {#if row.cleared}
                <p class="line ok">{clearedLine(row.cleared)} · {formatMs(row.cleared.ts_ms)}</p>
              {/if}

              {#if row.cleared === null}
                {#if latch?.can_clear}
                  <div class="row-actions">
                    {#if target}
                      <button
                        type="button"
                        class="restore"
                        disabled={actionBusy}
                        onclick={() => restoreFrom(target.id, latch)}
                      >
                        Restore to before this…
                      </button>
                    {/if}
                    {#if confirmingClear === row.key}
                      <button
                        type="button"
                        class="danger"
                        disabled={actionBusy}
                        onclick={() => void run(latch, 'clear_contamination')}
                      >
                        Yes, clear the flag
                      </button>
                      <button type="button" onclick={() => (confirmingClear = null)}>Cancel</button>
                    {:else}
                      <button type="button" onclick={() => (confirmingClear = row.key)}>
                        Mark false positive…
                      </button>
                    {/if}
                  </div>
                  {#if confirmingClear === row.key}
                    <!-- Deliberately still TEXT, not an icon: this appears
                         only while the destructive confirm is open, and a
                         warning gating a click must not require a hover. -->
                    <p class="line warn">
                      Clearing says the flagged content was harmless. If it was not, this tab's
                      memory writes stop being held for review while a model that read it is
                      still running. The conversation is not changed — nothing is restarted and
                      nothing is rolled back.
                    </p>
                  {/if}
                {/if}
              {/if}
            </div>
          </div>
        {/if}
      {/each}
    </div>
  {/if}
</div>

<style>
  .timeline {
    display: flex;
    flex-direction: column;
    gap: var(--space-2, 8px);
    font-size: var(--font-size-sm, 13px);
  }
  .toolbar {
    display: flex;
    gap: 8px;
  }
  .toolbar button {
    appearance: none;
    background: var(--surface-3, #2a2a2a);
    border: 1px solid var(--border-subtle, #444);
    color: var(--text-primary, #ddd);
    border-radius: var(--radius-sm, 4px);
    padding: 4px 10px;
    font-size: var(--font-size-xs, 11px);
    cursor: pointer;
  }
  .toolbar button:hover:not(:disabled) {
    background: var(--surface-4, #333);
  }
  .toolbar button:disabled {
    opacity: 0.6;
    cursor: not-allowed;
  }
  .toolbar .checkpoint-now {
    border-color: var(--accent, #3b6ea5);
    color: var(--accent, #3b6ea5);
  }
  .msg {
    opacity: 0.7;
    font-style: italic;
    padding: var(--space-2, 8px) 0;
  }
  .msg.err {
    color: var(--text-danger-soft, #ff8a80);
    font-style: normal;
  }
  .msg.notice {
    opacity: 1;
    font-style: normal;
    color: var(--text-secondary, #bbb);
    line-height: 1.45;
    margin: 0;
  }
  /* A claim we cannot verify wears the dashed treatment `latch.ts` established
     for its unknown states — this is "we cannot see it", not "it is fine". */
  .msg.unknown {
    color: var(--awaiting, #d0a24c);
    border-bottom: 1px dashed var(--border-default, #555);
  }
  .rows {
    display: flex;
    flex-direction: column;
    gap: 1px;
  }
  .row {
    border: 1px solid var(--border-subtle, #444);
    border-radius: var(--radius-md, 6px);
    overflow: hidden;
  }
  /* The Events-tab jump target (#51): held for HIGHLIGHT_MS, then the class
     drops and the transition fades the row back to normal. */
  .row.flash {
    border-color: var(--accent, #3b6ea5);
    background: color-mix(in srgb, var(--accent, #3b6ea5) 14%, transparent);
  }
  .row:not(.flash) {
    transition:
      background 0.6s ease,
      border-color 0.6s ease;
  }
  /* An event, not a restore point — it must not read as another checkpoint. */
  .row.contam {
    border-color: var(--border-danger, #a33);
  }
  .row.contam.resolved {
    border-color: var(--border-subtle, #444);
  }
  .row-main {
    display: flex;
    align-items: center;
    gap: 10px;
    padding: 6px 10px;
    background: var(--surface-2, #232323);
    flex-wrap: wrap;
  }
  .row.contam .row-main {
    background: var(--surface-danger-soft, #2b1f1f);
  }
  .row.contam.resolved .row-main {
    background: var(--surface-2, #232323);
  }
  .trigger {
    flex: 0 0 auto;
  }
  .time {
    flex: 0 0 auto;
    color: var(--text-tertiary, #999);
    font-size: var(--font-size-xs, 11px);
    white-space: nowrap;
  }
  .label {
    flex: 1 1 200px;
    min-width: 0;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .files {
    flex: 0 0 auto;
    color: var(--text-secondary, #bbb);
    font-size: var(--font-size-xs, 11px);
    font-variant-numeric: tabular-nums;
    /* The count's meaning lives in its title (it is NOT the diff-vs-now
       size) — same "hover me" affordance as .note-icon. */
    cursor: help;
  }
  /* The tool a checkpoint was taken in front of. Monospace and boxed so it
     reads as an identifier rather than as more prose, and `cursor: help`
     because the sentence that explains what it means lives in its title —
     same affordance as `.files`. No `min-width`: the element is absent on
     almost every row, so it must not reserve a column. */
  .source {
    flex: 0 0 auto;
    color: var(--text-secondary, #bbb);
    font-family: var(--font-mono, ui-monospace, monospace);
    font-size: var(--font-size-xs, 11px);
    border: 1px solid var(--border-subtle, #444);
    border-radius: var(--radius-sm, 4px);
    padding: 0 4px;
    max-width: 18ch;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
    cursor: help;
  }
  .agent {
    flex: 0 0 auto;
    color: var(--text-tertiary, #999);
    font-size: var(--font-size-xs, 11px);
    min-width: 5ch;
  }
  .actions {
    flex: 0 0 auto;
    display: inline-flex;
    gap: 4px;
  }
  .actions button,
  .row-actions button {
    appearance: none;
    background: transparent;
    border: 1px solid var(--border-subtle, #444);
    color: var(--text-secondary, #bbb);
    border-radius: var(--radius-sm, 4px);
    padding: 2px 8px;
    font-size: var(--font-size-xs, 11px);
    cursor: pointer;
  }
  .actions button:hover,
  .row-actions button:hover {
    background: var(--surface-3, #2a2a2a);
    color: var(--text-primary, #ddd);
  }
  .actions button.restore,
  .row-actions button.restore,
  .row-actions button.danger {
    border-color: var(--border-danger, #a33);
    color: var(--text-danger-soft, #ff8a80);
  }
  .row-detail {
    padding: 6px 10px 8px;
    background: var(--surface-sunken, #1a1a1a);
    border-top: 1px solid var(--border-faint, #333);
    display: flex;
    flex-direction: column;
    gap: 4px;
  }
  .row-detail .line {
    margin: 0;
    line-height: 1.45;
    color: var(--text-secondary, #bbb);
  }
  .row-detail .line.warn {
    color: var(--awaiting, #d0a24c);
  }
  .row-detail .line.notes {
    display: flex;
    align-items: center;
    gap: 10px;
  }
  .note-icon {
    /* The sentence lives in the title — `help` says "hover me" the way a
       bare glyph on its own cannot. */
    cursor: help;
    font-size: 1.15em;
    line-height: 1;
  }
  .note-icon.warn {
    color: var(--awaiting, #d0a24c);
  }
  .note-icon.dim {
    color: var(--text-tertiary, #999);
  }
  .row-detail .line.ok {
    color: var(--text-success, #7ec699);
  }
  .row-actions {
    display: flex;
    gap: 6px;
    margin-top: 2px;
  }
  .row-diff {
    padding: 8px 10px;
    background: var(--surface-sunken, #1a1a1a);
    border-top: 1px solid var(--border-faint, #333);
  }
</style>
