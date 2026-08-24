<script lang="ts">
  // V9-01 Phase I: the read-only Code Intelligence tab — an app-rendered
  // dashboard (no PTY) of the per-project graph's memory, context, analyses,
  // and usage surfaces. Mirrors the other reserved dashboards' feature-gated
  // nature but is fed by the in-process GraphService rather than a child
  // process's output: it seeds from the `graph_status` IPC, then tracks live
  // transitions via the `graph-status` event. The graph indexer dashboard
  // (index cards + rebuild/pause actions) moved to Tool Activity → Graph
  // index (GraphIndexView.svelte).
  // #130 (F4): the rules more than one section of this view needs live in a
  // plain sheet keyed on the `.graph-monitor` class this component puts on its
  // root, so a section child can be extracted without losing them to Svelte's
  // per-component style scoping. This import stays FIRST — the sheet must be
  // emitted ahead of every child's CSS so a child wins a specificity tie.
  import './codeIntel/codeIntel.css';
  import { onMount, onDestroy } from 'svelte';
  import {
    graphStatus,
    graphMemory,
    graphMemoryClear,
    graphNoteReview,
    graphNoteSetPinned,
    graphFacts,
    graphFactUpdate,
    graphFactAdd,
    graphContextPreview,
    quarantineReason,
    onGraphStatus,
    onGraphAnalyses,
    type GraphStatus,
    type MemorySnapshot,
    type ProjectFact,
    type RetrieveResult,
    type SessionInfo,
  } from './graph';
  import { isActiveSessionIn } from './usageMath';
  import { fmtDate, fmtTime } from './format';
  import { listenManaged } from './listenManaged';
  import { settings } from './settings/store';
  import { openSettingsWindowToSection } from './settings/ipc';
  import type { ChecksSuggestion } from './settings/types';
  import { checksSuggestion, checksDismissSuggestion } from './checks';
  import { computeChip } from './settings/checksEditor';
  import { GRAPH_MONITOR_TAB_ID } from './tabs/types';
  import { isAppViewVisible, onAppViewShown } from './appViewVisibility';
  import SectionNav from './SectionNav.svelte';
  import UsageOverview from './codeIntel/UsageOverview.svelte';
  import AnalysesSection from './codeIntel/AnalysesSection.svelte';
  import ArchitectureSection from './codeIntel/ArchitectureSection.svelte';
  import TracePathSection from './codeIntel/TracePathSection.svelte';
  import { loadViewSection } from './viewSection';
  import { harnesses } from './harness';

  // The graph_* tool reference list, the recent-calls activity feed, and the
  // graph index dashboard all moved to the Tool Activity tab
  // (ToolActivityView.svelte / GraphIndexView.svelte).

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

  /// A scan in `AnalysesSection` finished and its result is on screen, so the
  /// badge baseline may advance. Prefer the count the `graph-analyses` event
  /// last reported over the one this pass measured: the event is what the
  /// badge compares against, and acking to a stale measurement would leave a
  /// badge standing over results the user has already read. `measured` is the
  /// fallback for the case where no event has landed this session.
  function ackAnalyses(kind: 'dead' | 'cycles', measured: number): void {
    if (kind === 'dead') analysesAckDead = analysesLive?.dead ?? measured;
    else analysesAckCycles = analysesLive?.cycles ?? measured;
  }

  // Memory (Phase C): per-project session/action memory. Fetched while the
  // Memory section is open (via refresh()'s poll) and on demand.
  let memory = $state<MemorySnapshot | null>(null);
  /// When the snapshot was last fetched, epoch ms; `0` ⇒ never in this
  /// instance's lifetime. See `refresh()`, which primes it off-section so the
  /// V32 quarantine badge is honest before anyone opens Memory.
  ///
  /// **A timestamp rather than the boolean it replaces (#48, M-3.)** The boolean
  /// was set once and never reset, and since `appViews.ts` keeps ONE instance of
  /// this component alive for the app's lifetime, "once per instance" was "once
  /// per app run" — the badge was a snapshot of the moment the view first
  /// rendered. A user who opened Code Intelligence on Overview primed it at 0
  /// and stayed off the Memory section; a contaminated tab then wrote notes,
  /// each quarantined correctly, and the badge stayed absent forever. It was
  /// honest only about notes quarantined *before* first render — the opposite of
  /// its purpose, and it failed the same way in the clearing direction.
  ///
  /// Throttled rather than unguarded, because the concern that motivated the
  /// original flag is real: `graph_memory` opens the warm index, and the poll
  /// below runs every 2 s.
  let memoryPrimedAt = 0;

  /// How stale the off-section snapshot may get. Well under the time it takes to
  /// write a note and wonder where it went, and 1/10th the poll's off-section
  /// cost at 2 s.
  const MEMORY_OFF_SECTION_MS = 20_000;

  async function refreshMemory(): Promise<void> {
    try {
      memory = await graphMemory();
      memoryPrimedAt = Date.now();
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

  // V32 Phase C2: the quarantine review queue. A note written while its session
  // was externally tainted is stored but withheld from every read path — recall,
  // listings, the compaction carry-over, the fact distiller and therefore the
  // launch-time auto-injection — until it is promoted or discarded here. This
  // view is the ONLY reader of tainted notes, which is why the count also rides
  // the section tab as a badge: a quarantined note nobody notices is a research
  // conclusion silently lost, and the whole point of quarantining rather than
  // refusing the write was to not lose it.
  let quarantined = $derived(memory?.quarantined ?? []);
  let reviewBusy = $state<string | null>(null);

  /// Which quarantined note has an armed confirmation, and for which action.
  ///
  /// **#48, M-23 — the polarity was inverted.** Promote — which un-quarantines
  /// attacker-authored text into project memory, where recall returns it and the
  /// launch-time injection carries it into every future session — was ONE
  /// unconfirmed click, while Discard, which can only lose a note, sat behind a
  /// `confirm()` dialog. The safety-destructive action cost less than the safe
  /// one, which is backwards.
  ///
  /// Both are confirmed now and the WEIGHT is the right way round, copying the
  /// shape `TaintMenu.svelte` already gets right for "Restore full access": a
  /// trailing `…` on the trigger, a sentence that spells out the consequence
  /// before the second click, and the danger treatment on the action that
  /// releases containment — not on the one that merely deletes.
  ///
  /// One armed row at a time, keyed by note id: opening a second confirmation
  /// replaces the first, so a stray Enter can never resolve a note the user is
  /// no longer looking at.
  let reviewConfirm = $state<{ note: string; action: 'promote' | 'discard' } | null>(null);

  /// Arm (or re-arm) the confirmation for one note + action. Mutually
  /// exclusive with the bulk confirmation below — two armed confirmations at
  /// once would make a stray Enter ambiguous.
  function armReview(noteId: string, action: 'promote' | 'discard'): void {
    bulkConfirm = null;
    reviewConfirm = { note: noteId, action };
  }

  async function reviewNote(noteId: string, action: 'promote' | 'discard'): Promise<void> {
    reviewConfirm = null;
    reviewBusy = noteId;
    try {
      await graphNoteReview(noteId, action);
      await refreshMemory();
    } catch (e) {
      console.error('graph_note_review failed', e);
    } finally {
      reviewBusy = null;
    }
  }

  /// The quarantined note whose detail dialog is open, by id — `null` for none.
  ///
  /// This replaced an inline expander that grew the row in place. The queue is
  /// a review list: its job is to let you see at a glance how many decisions
  /// are waiting and what each one is about, and a wrapped reason sentence plus
  /// a wrapped note body plus an expansion panel cost four-plus lines per note,
  /// so three held notes filled the section. Every row is one line now and the
  /// full text + context live in a dialog, the same row → detail-popup shape
  /// the Events feed uses.
  ///
  /// The dialog is where the decision is made: it carries the whole reason
  /// sentence, the un-truncated text, the context and BOTH buttons, so nothing
  /// that was needed to decide moved further away than one click.
  let detailQ = $state<string | null>(null);

  /// The open note, resolved against the LIVE queue rather than captured at
  /// click time. Promote/discard removes it from `quarantined`, and so does a
  /// background refresh of a note reviewed elsewhere — resolving each render
  /// means the dialog closes itself instead of sitting open over a note that no
  /// longer exists (and, worse, offering to promote it).
  let detailNote = $derived(
    detailQ === null ? null : (quarantined.find((n) => n.note_id === detailQ) ?? null),
  );

  function openQuarantined(noteId: string): void {
    // A confirmation armed on the previous note must not survive into this
    // one's dialog — same reason `armReview` clears the bulk one.
    reviewConfirm = null;
    bulkConfirm = null;
    detailQ = noteId;
  }

  function closeQuarantined(): void {
    detailQ = null;
    reviewConfirm = null;
  }

  /// Escape closes the dialog, per the app's dialog convention (EventsView).
  /// Guarded on the dialog being open so the Memory section never swallows an
  /// Escape meant for something else.
  function onKeyDown(e: KeyboardEvent): void {
    if (e.key === 'Escape' && detailQ !== null) {
      e.preventDefault();
      closeQuarantined();
    }
  }

  /// The writing session's summary row, when memory still has it. An EXACT
  /// join on session_id — unlike the hold cause, which is never reconstructed
  /// (see `quarantineReason`); a session that has aged out simply yields no
  /// extra line, never a guess.
  function sessionFor(sessionId: string): SessionInfo | undefined {
    return sessionId ? memory?.sessions.find((s) => s.session_id === sessionId) : undefined;
  }

  // Bulk review: the same two actions over the whole queue, with the same
  // confirmation polarity — Promote all releases containment on every held
  // note at once, so it wears the danger treatment and the heavier sentence;
  // Discard all only deletes. Sequential on purpose (`graph_note_review` is
  // per-note); a note that fails is logged and skipped rather than aborting
  // the sweep, and the trailing refresh repaints whatever actually happened.
  let bulkConfirm = $state<'promote' | 'discard' | null>(null);
  let bulkBusy = $state(false);

  function armBulk(action: 'promote' | 'discard'): void {
    reviewConfirm = null;
    bulkConfirm = action;
  }

  async function reviewAll(action: 'promote' | 'discard'): Promise<void> {
    bulkConfirm = null;
    bulkBusy = true;
    try {
      for (const n of quarantined) {
        try {
          await graphNoteReview(n.note_id, action);
        } catch (e) {
          console.error('graph_note_review failed', n.note_id, e);
        }
      }
      await refreshMemory();
    } finally {
      bulkBusy = false;
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

  /// How each harness receives an injected prompt, named by the harness (V40
  /// Phase F, locked decision 27). The sentence used to enumerate the two
  /// shipped harnesses and their mechanisms in markup here.
  const injectMechanisms = $derived(
    $harnesses
      .filter((h) => h.affordances.injectMechanism)
      .map((h) => `for ${h.label} via ${h.affordances.injectMechanism}`)
      .join(', '),
  );

  // ── the Overview seam (#130) ─────────────────────────────────────────────
  //
  // The Overview section is `codeIntel/UsageOverview.svelte`. It is mounted
  // unconditionally (its `active` prop gates the markup) because the STATE
  // behind that section — selected session, follow mode, chart zoom, scroll
  // offset — always survived a section switch, and a component destroyed by an
  // `{#if}` would lose it. Two things cross the seam:
  //
  //   * DOWN, the schedule: `refresh()` below drives `overview.tick(force)`.
  //     The 2s keep-alive poll stays in exactly one place, this file.
  //   * UP, `active_session_ids`: the Overview owns the usage snapshot that
  //     carries the list, and the Memory section's "this session / last
  //     session" label is its other reader.
  let overview: UsageOverview | undefined = $state();
  /// Live session ids as of the last usage snapshot the Overview applied.
  /// `$state.raw` for the same reason the snapshot it comes out of is raw: it
  /// is replaced wholesale, never mutated.
  let activeSessionIds = $state.raw<readonly string[]>([]);
  /// Whether a session is actually live right now. A fresh empty session has
  /// recorded nothing yet, so `memory.current_session` still points at the
  /// PREVIOUS session — the Working-set label says "last session" then instead
  /// of claiming it's this one. `isActiveSessionIn` is the one spelling of this
  /// predicate; the Overview asks it the same question of its own snapshot.
  function isActiveSession(sid?: string | null): boolean {
    return isActiveSessionIn(activeSessionIds, sid);
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

  // Per-root graph status — the index cards themselves moved to Tool
  // Activity → Graph index; this view still tracks the statuses because the
  // checks-suggestion chip fires once a root's index is complete.
  let roots = $state<GraphStatus[]>([]);

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

  let poll: ReturnType<typeof setInterval> | null = null;

  function upsert(s: GraphStatus): void {
    const i = roots.findIndex((r) => r.root === s.root);
    if (i >= 0) roots[i] = s;
    else roots = [...roots, s];
  }

  /// `force` = a user-initiated refresh (section switch): bypass the usage
  /// cadence gate below. The timer and the re-attach path pass `false`.
  async function refresh(force = false): Promise<void> {
    try {
      roots = await graphStatus();
    } catch (e) {
      console.warn('graph_status failed', e);
    }
    // V22: fetch the checks suggestion once an index is complete (guarded so it
    // runs at most once — the chip then recomputes reactively from settings).
    await maybeFetchChecksSuggestion();
    // Memory is only fetched while its section is visible (opens the warm
    // index) — with ONE exception: V32 Phase C2's quarantine badge sits on the
    // section nav and has to be honest from whichever section the view opens
    // on, or a note held for review is only ever found by someone who already
    // went looking. So prime the snapshot off-section — immediately on the
    // first tick, then at most every `MEMORY_OFF_SECTION_MS` (#48, M-3: priming
    // exactly ONCE made the badge a snapshot of app-start, blind to every note
    // quarantined afterwards).
    if (section === 'memory') {
      await refreshMemory();
      await refreshFacts();
    } else if (Date.now() - memoryPrimedAt >= MEMORY_OFF_SECTION_MS) {
      await refreshMemory();
    }
    // Usage (V14 Phase D/D2): same "only while visible" posture — the Usage
    // cards render inside the Overview section, which has owned the snapshot
    // and its fetch since #130. This poll stays the ONLY schedule: `tick`
    // applies the Overview's own cadence gate (that fetch is NOT on the 2s
    // tick — it is held to a minimum gap measured from the previous pass's
    // completion), and `force` — the user-initiated section switch — is what
    // bypasses that gate. The in-flight gate over there still keeps the calls
    // from stacking.
    if (section === 'overview') await overview?.tick(force);
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
  // refresh runs the moment it comes back.
  const unsubShown = onAppViewShown(GRAPH_MONITOR_TAB_ID, () => {
    void refresh();
  });

  onMount(async () => {
    await refresh();
    // A light poll backstops the event for the section-gated fetches (memory,
    // usage) and the checks-suggestion readiness edge.
    poll = setInterval(() => {
      if (isAppViewVisible(GRAPH_MONITOR_TAB_ID)) void refresh();
    }, 2000);
    window.addEventListener('keydown', onKeyDown);
  });

  onDestroy(() => {
    if (poll) clearInterval(poll);
    unsubShown();
    window.removeEventListener('keydown', onKeyDown);
  });
</script>

<div class="graph-monitor">
  <header>
    <h2>Code Intelligence</h2>
  </header>

  <!-- The badges keep their markup (and therefore their `.badge` rule) in
       this component: a snippet is compiled where it is DECLARED, so this
       one carries this file's style scope even though `SectionNav` renders
       it inside a button it owns. -->
  {#snippet sectionBadge(id: Section)}{#if id === 'analyses' && analysesBadgeTotal > 0}<span class="badge" title="New since last pass">+{analysesBadgeTotal}</span>{/if}{#if id === 'memory' && quarantined.length > 0}<span class="badge" title="Quarantined notes awaiting review">⚠{quarantined.length}</span>{/if}{/snippet}
  <SectionNav
    view="code-intelligence"
    sections={SECTIONS}
    bind:section
    onselect={(id) => {
      if (id === 'memory') {
        refreshMemory();
        refreshFacts();
      }
      if (id === 'overview') {
        // `true` = the user-initiated path: refetch now, do not wait out the
        // Overview's cadence gate. Same call the old inline `refreshUsage()`
        // made when this section's fetch lived in this file.
        void overview?.tick(true);
      }
    }}
    trailing={sectionBadge}
  />

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

  <!-- The four extracted sections (#130). Each is mounted UNCONDITIONALLY and
       gates its own markup on `active`, rather than sitting in the
       `{#if section === …}` chain below: the STATE behind these sections
       always outlived a section switch, because it lived in this file's
       script and the chain only ever gated markup. A component destroyed by
       an `{#if}` would lose a running usage view's selection and zoom, a
       trace someone typed, and every scan result they paid for.

       They render nothing while inactive, in the same place in the same
       parent, so the DOM for any given section is what it always was. The
       chain below still holds Memory and Context. -->
  <UsageOverview
    active={section === 'overview'}
    bind:this={overview}
    onActiveSessions={(ids) => (activeSessionIds = ids)}
  />

  <AnalysesSection
    active={section === 'analyses'}
    {deadBadge}
    {cyclesBadge}
    onAck={ackAnalyses}
  />

  <TracePathSection active={section === 'path'} />

  <ArchitectureSection active={section === 'architecture'} />

  {#if section === 'memory'}
    {#if quarantined.length > 0}
      <!--
        V32 Phase C2 (memory quarantine). First card in the section on purpose:
        these notes are the only ones that need a decision, and everything below
        is already live memory. No pin control here — a quarantined note is not
        in memory at all yet, so "pinned" is a property that only starts to mean
        anything after Promote (which preserves whatever the writer asked for).
      -->
      <section class="card quarantine">
        <div class="history-head">
          ⚠ Quarantined notes <span class="muted">({quarantined.length})</span>
          {#if quarantined.length > 1}
            <!-- Bulk decisions, M-23's polarity kept: the sweep that releases
                 containment wears the danger colour and the `…`; the sweep
                 that only deletes stays plain. Both confirm below. -->
            <button
              class="mini danger"
              disabled={bulkBusy || reviewBusy !== null}
              title="Accept every held note into project memory — asks for confirmation"
              onclick={() => armBulk('promote')}>Promote all…</button
            >
            <button
              class="mini"
              disabled={bulkBusy || reviewBusy !== null}
              title="Delete every held note permanently — asks for confirmation"
              onclick={() => armBulk('discard')}>Discard all…</button
            >
          {/if}
        </div>
        {#if bulkConfirm}
          <div class="qconfirm" class:warn={bulkConfirm === 'promote'}>
            {#if bulkConfirm === 'promote'}
              <p>
                Promoting all {quarantined.length} notes accepts every one of
                them into project memory unread — including any whose text an
                attacker may have planted, and any whose text is a captured
                credential. Each one is then returned by recall and rides the
                launch-time guidance into every future session. If you have not
                read them all, review them one by one instead. Continue?
              </p>
              <div class="qconfirm-row">
                <button class="mini danger" disabled={bulkBusy} onclick={() => reviewAll('promote')}
                  >Yes, promote all {quarantined.length} into memory</button
                >
                <button class="mini" onclick={() => (bulkConfirm = null)}>Cancel</button>
              </div>
            {:else}
              <p>
                Discarding all {quarantined.length} notes deletes them
                permanently and cannot be undone. Nothing else changes: they are
                already held out of every read path, so this releases nothing.
                Continue?
              </p>
              <div class="qconfirm-row">
                <button class="mini" disabled={bulkBusy} onclick={() => reviewAll('discard')}
                  >Yes, discard all {quarantined.length} permanently</button
                >
                <button class="mini" onclick={() => (bulkConfirm = null)}>Cancel</button>
              </div>
            {/if}
          </div>
        {/if}
        <!-- #48, F-24: the old copy named only ONE of the three causes that put
             a note here (the session taint latch), which is why a
             credential-screen hold read as an injected-instruction hold. Each
             row now states its own cause; this paragraph says what the queue is
             and which way round the two buttons cut. -->
        <p class="caveat">
          Held out of recall, listings and the launch-time injection until you
          decide. A note lands here because its session had already used an
          external tool, because the write could not be attributed to a tab, or
          because the write-time credential screen matched it — each row says
          which. Click a row to open the note: full text, its context (session,
          exact time, what promoting would pin) and the two decisions.
          <strong>Promote</strong> accepts a note into project memory (keeping
          its pinned state) and into every future session, so it is the one
          that asks you to be sure; <strong>Discard</strong> deletes it and
          releases nothing.
        </p>
        <div class="rows">
          {#each quarantined as n (n.note_id)}
            {@const why = quarantineReason(n)}
            <!--
              One held note = ONE row: ⚠, the cause, a peek at the text, the
              time. Everything else (the un-truncated text, the context grid,
              both decisions and their confirmations) is in the dialog below.

              #48, F-24's ordering is kept and still load-bearing: the WHY comes
              before the note text, at full weight, because three different
              causes land in this queue (the session taint latch, an
              unattributable write, and the write-time credential screen) and
              for the third one the note text IS the credential. Locked decision
              22 forbids showing the matched value more prominently than the rule
              that matched it — so the reason leads the row and the text follows
              it, dimmed and truncated. Both columns truncate rather than wrap;
              the row's `title` carries the full reason for a hover, and the
              dialog carries everything.

              The backend has published the cause since #48's F-24 fix, and
              composes it at the store boundary so a held note cannot exist
              without one. The `{:else}` leg is therefore about the NOTE, not
              about the build: rows written before the column existed carry no
              cause. It is still never reconstructed here — see
              `quarantineReason` in graph.ts for why guessing would be worse
              than a blank.
            -->
            <div
              class="qrow"
              class:open={detailQ === n.note_id}
              role="button"
              tabindex="0"
              title={why
                ? `${why.reason}\n\nClick to open the note, its context and the two decisions.`
                : 'Reason not recorded for this note.\n\nClick to open the note, its context and the two decisions.'}
              onclick={() => openQuarantined(n.note_id)}
              onkeydown={(e) => {
                if (e.key === 'Enter' || e.key === ' ') {
                  e.preventDefault();
                  openQuarantined(n.note_id);
                }
              }}
            >
              <span class="qmark" title="Quarantined pending review">⚠</span>
              {#if why}
                <span class="qreason">{why.reason}</span>
              {:else}
                <span class="qreason missing">Reason not recorded</span>
              {/if}
              <span class="qpeek">{n.text}</span>
              <span class="aloc">{fmtTime(n.ts_ms)}</span>
            </div>
          {/each}
        </div>
      </section>
    {/if}

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
        No session memory yet. As the agent reads, edits, and queries files (with the
        graph enabled), its working set and notes appear here.
      </p>
    {:else}
      <section class="card">
        <div class="history-head">
          Working set
          <span class="muted">
            {#if memory.current_session}({isActiveSession(memory.current_session)
                ? 'current session'
                : 'last session'}){/if}
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
          <p class="placeholder">No notes. Ask the agent to <code>context_note</code> a decision.</p>
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
        When enabled (Settings → Code Intelligence → Token efficiency → Context
        injection), cImp
        prepends a budget-bounded digest of the most relevant files to each
        prompt{injectMechanisms ? ` — ${injectMechanisms}` : ''}. Preview below
        shows what <em>would</em> be injected for a prompt, regardless of the
        toggle.
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
  {/if}

</div>

<!--
  The quarantine detail dialog (V32 Phase C2 / #48). Outside `.graph-monitor`
  on purpose: that container is `position: absolute; overflow-y: auto`, so a
  dialog rendered inside it would scroll with the list and clip at its edges.
  Same structure and z-order as the Events feed's event popup, so a held note
  reads like every other "click the row for the whole record" surface.

  `detailNote` is resolved from the live queue each render (see its
  declaration): a note that gets promoted, discarded, or aged out closes its
  own dialog instead of leaving a promotable ghost on screen.
-->
{#if detailNote}
  {@const n = detailNote}
  {@const why = quarantineReason(n)}
  {@const sess = sessionFor(n.session_id)}
  <div class="backdrop" onclick={closeQuarantined} role="presentation"></div>
  <div class="qdialog" role="dialog" aria-label="Quarantined note">
    <header class="qd-head">
      <div class="qd-title"><span class="qmark">⚠</span> Quarantined note</div>
      <button type="button" class="qd-close icon" onclick={closeQuarantined} aria-label="Close"
        >×</button
      >
    </header>
    <!-- The cause first, in full — the row could only show one truncated line
         of it, and this is the sentence the decision turns on. -->
    <div class="qd-why" class:missing={!why}>
      {#if why}
        <p class="qreason">{why.reason}</p>
        <div class="qd-rulerow">
          {#if why.rules.length > 0}
            <span class="qrules" title="The rules that matched">{why.rules.join(', ')}</span>
          {/if}
          <span class="qscreen" title="The screen that held this write">{why.screen}</span>
        </div>
      {:else}
        <p class="qreason">
          Reason not recorded for this note — it was held before cImp stored the
          cause, so which screen or rule matched cannot be recovered.
        </p>
      {/if}
    </div>
    <div class="qd-body">
      <!-- The full text, un-truncated — what is actually being decided about,
           and what the promote confirmation says to read first. -->
      <div class="qd-sec">Note text</div>
      <pre class="qtext">{n.text}</pre>
      <div class="qmeta">
        <span class="qlabel">Written</span>
        <span>{fmtDate(n.ts_ms)} · {fmtTime(n.ts_ms)}</span>
        <span class="qlabel">Session</span>
        <span class="qmono">{n.session_id || '(none — the write could not be attributed)'}</span>
        {#if sess}
          <span class="qlabel">That session</span>
          <span
            >{sess.agent} · active {fmtDate(sess.started_ms)}
            {fmtTime(sess.started_ms)} – {fmtTime(sess.last_ms)} · {sess.events} events</span
          >
        {/if}
        <span class="qlabel">If promoted</span>
        <span>
          {n.pinned
            ? 'saved as PINNED — kept across sessions and carried into the launch-time guidance'
            : 'saved unpinned — ordinary project memory, returned by recall'}
        </span>
        {#if why}
          <span class="qlabel">Held by</span>
          <span class="qmono"
            >{why.screen}{why.rules.length > 0 ? ` · ${why.rules.join(', ')}` : ''}</span
          >
        {/if}
        <span class="qlabel">Note id</span>
        <span class="qmono">{n.note_id}</span>
      </div>
    </div>
    {#if reviewConfirm?.note === n.note_id}
      <div class="qconfirm" class:warn={reviewConfirm.action === 'promote'}>
        {#if reviewConfirm.action === 'promote'}
          <p>
            Promoting accepts this text into project memory. It was written
            while the session had already used an external tool, so it may be an
            instruction someone planted — once promoted it is returned by
            recall, rides the launch-time guidance into new sessions, and no
            longer says where it came from. Read it above first. Continue?
          </p>
          <div class="qconfirm-row">
            <button
              class="mini danger"
              disabled={reviewBusy === n.note_id}
              onclick={() => reviewNote(n.note_id, 'promote')}>Yes, promote into memory</button
            >
            <button class="mini" onclick={() => (reviewConfirm = null)}>Cancel</button>
          </div>
        {:else}
          <p>
            Discarding deletes this note permanently and cannot be undone.
            Nothing else changes: it is already held out of every read path, so
            discarding releases nothing. Continue?
          </p>
          <div class="qconfirm-row">
            <button
              class="mini"
              disabled={reviewBusy === n.note_id}
              onclick={() => reviewNote(n.note_id, 'discard')}>Yes, discard permanently</button
            >
            <button class="mini" onclick={() => (reviewConfirm = null)}>Cancel</button>
          </div>
        {/if}
      </div>
    {/if}
    <!-- M-23's polarity, unchanged by the move: Promote wears the `…` and the
         danger colour because it is the click that puts this text back into
         every future session. Discard is confirmed too (deletion is permanent)
         but stays plain — it releases nothing. -->
    <footer class="qd-actions">
      <button
        class="mini danger"
        disabled={reviewBusy === n.note_id || bulkBusy}
        title="Accept into project memory — asks for confirmation"
        onclick={() => armReview(n.note_id, 'promote')}>Promote…</button
      >
      <button
        class="mini"
        disabled={reviewBusy === n.note_id || bulkBusy}
        title="Delete permanently — asks for confirmation"
        onclick={() => armReview(n.note_id, 'discard')}>Discard…</button
      >
      <button class="mini qd-dismiss" onclick={closeQuarantined}>Close</button>
    </footer>
  </div>
{/if}

<style>
  .graph-monitor {
    /* Sit ABOVE the pane's absolutely-positioned (empty) terminal slot —
       otherwise that transparent slot paints on top of this static content
       and swallows every button click. */
    position: absolute;
    inset: 0;
    overflow-y: auto;
    padding: 16px;
    font-size: 13px;
    color: var(--text-primary, #ddd);
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
    color: var(--text-primary, #ddd);
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
    border-left: 1px solid var(--border-subtle, #333);
    color: var(--text-primary, #ddd);
    opacity: 0.6;
    cursor: pointer;
    font-size: 15px;
  }
  .checks-chip .chip-x:hover {
    opacity: 1;
    background: rgba(255, 255, 255, 0.06);
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
  /* V32 Phase C2: the quarantine review queue. Warning-tinted rather than
     danger-tinted — a held note is a decision waiting, not a failure. */
  .card.quarantine {
    border-color: var(--border-warning, #c9820a);
  }
  .qmark {
    color: var(--surface-warning, #c9820a);
  }
  /* #48, F-24 — one held note, ONE LINE: ⚠, cause, text peek, time. Both text
     columns get `minmax(0, …)` so they truncate instead of forcing the row
     wider; the 2fr/3fr split keeps the cause readable at the narrow widths
     this card sits at while still showing enough of the note to tell two
     apart. The whole row is the click target (dialog). */
  .qrow {
    display: grid;
    grid-template-columns: auto minmax(0, 2fr) minmax(0, 3fr) auto;
    align-items: baseline;
    gap: 8px;
    padding: 3px 4px;
    border-bottom: 1px solid var(--border-faint, #2a2a2a);
    font-size: 12px;
    cursor: pointer;
  }
  .qrow:hover,
  .qrow:focus-visible,
  .qrow.open {
    background: var(--surface-2, rgba(255, 255, 255, 0.04));
    outline: none;
  }
  /* The cause is the row's headline, at full weight — decision 22 forbids the
     matched value being more prominent than the rule that matched it, and the
     value is `.qpeek` beside it. */
  .qreason {
    color: var(--text-primary, #ddd);
    font-weight: 600;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  /* A reason we could not be told is not a reason. Rendered as the app's
     "not a confident claim" treatment (quiet, italic) rather than as an
     explanation — see `quarantineReason` for why it is never reconstructed. */
  .qreason.missing,
  .qd-why.missing .qreason {
    color: var(--text-tertiary, #9aa0aa);
    font-weight: 400;
    font-style: italic;
  }
  /* A peek at the note, deliberately quieter than the cause and never wrapped:
     when the cause is the credential screen, this text IS the secret. */
  .qpeek {
    color: var(--text-secondary, #b8bec9);
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  /* The matched rule identifiers, verbatim. Monospace because they are
     identifiers the user may search the ruleset for. */
  .qrules {
    font-family: monospace;
    font-size: 11px;
    color: var(--text-warning, #f0c060);
    border: 1px solid var(--border-warning, #6a571a);
    border-radius: var(--radius-sm, 2px);
    padding: 0 3px;
  }
  .qscreen {
    font-family: monospace;
    font-size: 11px;
    color: var(--text-tertiary, #9aa0aa);
  }
  /* ── The detail dialog. Same conventions as the Events feed's event popup
     (backdrop + centred card + Escape/backdrop close), so "click the row to
     see the whole record" behaves identically wherever it appears. ─────── */
  .backdrop {
    position: fixed;
    inset: 0;
    background: rgba(0, 0, 0, 0.5);
    z-index: 100;
  }
  .qdialog {
    position: fixed;
    top: 50%;
    left: 50%;
    transform: translate(-50%, -50%);
    display: flex;
    flex-direction: column;
    width: min(760px, calc(100vw - 40px));
    max-height: min(80vh, 900px);
    box-sizing: border-box;
    padding: 14px 16px;
    background: var(--surface-3, #1e1e1e);
    /* Warning-bordered like the card it opens from: the dialog is the same
       decision, enlarged. */
    border: 1px solid var(--border-warning, #c9820a);
    border-radius: var(--radius-lg, 10px);
    box-shadow: var(--shadow-lg, 0 8px 32px rgba(0, 0, 0, 0.5));
    color: var(--text-primary, #ddd);
    font-size: 13px;
    z-index: 101;
  }
  .qd-head {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 12px;
    margin-bottom: 6px;
  }
  .qd-title {
    display: flex;
    align-items: center;
    gap: 6px;
    font-weight: 600;
  }
  .qd-close {
    border: none;
    background: transparent;
    color: var(--text-primary, #ddd);
    font-size: 16px;
    line-height: 1;
    cursor: pointer;
    opacity: 0.7;
    padding: 2px 6px;
  }
  .qd-close:hover {
    opacity: 1;
  }
  /* The cause block: the whole sentence, then the identifiers under it. */
  .qd-why {
    margin-bottom: 10px;
  }
  .qd-why .qreason {
    margin: 0;
    /* Unlike the row, the dialog has room — never truncate the cause here. */
    white-space: normal;
    overflow: visible;
    font-size: 13px;
    line-height: 1.4;
  }
  .qd-rulerow {
    display: flex;
    align-items: baseline;
    flex-wrap: wrap;
    gap: 6px;
    margin-top: 4px;
  }
  .qd-body {
    flex: 1;
    min-height: 0;
    overflow-y: auto;
  }
  .qd-sec {
    font-size: 11px;
    font-weight: 600;
    text-transform: uppercase;
    opacity: 0.7;
    margin-bottom: 4px;
  }
  .qd-actions {
    display: flex;
    justify-content: flex-end;
    gap: 6px;
    margin-top: 12px;
  }
  .qd-dismiss {
    margin-left: 6px;
  }
  /* The payload treatment the Events popup uses for request/response bodies:
     sunken, wrapped, independently scrollable so a long note cannot push the
     context grid and the two buttons out of reach. */
  .qtext {
    margin: 0 0 10px;
    padding: 8px 10px;
    font-size: 12px;
    font-family: inherit;
    white-space: pre-wrap;
    word-break: break-word;
    max-height: 40vh;
    overflow-y: auto;
    background: var(--surface-sunken, rgba(0, 0, 0, 0.3));
    border: 1px solid var(--border-faint, #2a2a2a);
    border-radius: var(--radius-md, 3px);
  }
  .qmeta {
    display: grid;
    grid-template-columns: auto 1fr;
    gap: 2px 10px;
    font-size: 11px;
    color: var(--text-secondary, #b8bec9);
  }
  .qmeta .qlabel {
    color: var(--text-tertiary, #9aa0aa);
    text-transform: uppercase;
    font-size: 10px;
    letter-spacing: 0.03em;
    align-self: baseline;
  }
  .qmeta .qmono {
    font-family: monospace;
    word-break: break-all;
  }
  /* M-23's confirmation. Amber for Promote (it releases containment), neutral
     for Discard (it releases nothing) — the same polarity TaintMenu uses. */
  .qconfirm {
    margin: 0 4px 4px;
    padding: 6px 8px;
    border: 1px solid var(--border-default, #3f4554);
    border-radius: var(--radius-md, 3px);
    background: var(--surface-2, rgba(255, 255, 255, 0.03));
  }
  .qconfirm.warn {
    border-color: var(--border-warning, #6a571a);
    background: var(--surface-warning-faint, rgba(240, 160, 32, 0.1));
  }
  /* In the dialog the confirmation sits between the scrolling body and the
     buttons, so it uses the dialog's own gutters rather than the card's. */
  .qdialog .qconfirm {
    margin: 10px 0 0;
    flex: none;
  }
  .qconfirm p {
    margin: 0 0 6px;
    font-size: 12px;
    line-height: 1.4;
    color: var(--text-secondary, #b8bec9);
  }
  .qconfirm.warn p {
    color: var(--text-warning, #f0c060);
  }
  .qconfirm-row {
    display: flex;
    gap: 6px;
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
  button.mini.danger {
    background: transparent;
    border-color: var(--border-danger, #b3261e);
    color: var(--text-danger-soft, #ffb4ab);
  }
  button.mini.danger:hover {
    background: var(--surface-danger, rgba(179, 38, 30, 0.15));
  }
  .preview-md {
    background: rgba(0, 0, 0, 0.25);
    border: 1px solid var(--border-subtle, #333);
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
