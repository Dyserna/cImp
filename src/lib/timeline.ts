import { invoke } from '@tauri-apps/api/core';
import type { Checkpoint } from './workbench';
import type { LatchRow } from './latch';

/// V33 step 5 — the Workbench Timeline's data model: checkpoints merged with
/// the contamination lifecycle, so a user can see which restore point was live
/// when a tab's conversation stopped being clean.
///
/// **Everything that decides what the view SAYS lives here, not in
/// `TimelineView.svelte`.** Same reason `latch.ts` holds `injectionChipState`
/// and `reducedTabLine`: `.svelte` files have no test harness in this repo
/// (vitest runs in node, there is no DOM and no component harness), and this is
/// a surface whose only job is to not overstate what it knows. A claim that
/// cannot be tested is a claim nobody is defending.

// ── The events ─────────────────────────────────────────────────────────────

/// One contamination-lifecycle event. Mirror of Rust
/// `offload::outbound::ContaminationEvent`.
export interface ContaminationEvent {
  /// The activity row's id — stable across restarts.
  id: number;
  /// Epoch **millis**. Checkpoints carry epoch *seconds* (`ts_unix`); see
  /// [`precedes`] for the one place the two are compared.
  ts_ms: number;
  /// `activity::root_key` form.
  ///
  /// **Empty means "not attributable to a project" — a claim, not a gap** (#48
  /// F-16): the same one sentinel `ActivityEntry.root` carries, covering a writer
  /// that could not derive a root (the contamination screens take theirs from
  /// `tab_root_key`, which degrades to `""` when even the process cwd cannot be
  /// read) and a row written before the column existed. A future recorder that
  /// positively has "no project" as a fact must not reuse it.
  ///
  /// This is the one field in this file that a view NARROWS by
  /// ([`buildTimelineRows`]), so it is also the one that can hide a row: `''`
  /// matches no real root, and F-16's live reproduction was exactly that — a row
  /// recording that a credential was held, missing from a project-scoped view.
  /// [`evidenceNotices`] is what keeps that visible; a rootless event is announced
  /// on its own terms and never counted as another project's.
  root: string;
  /// `false` = the bit was SET, `true` = it was released.
  cleared: boolean;
  /// `agent:tab`, verbatim.
  scope: string;
  /// The `agent` half, or `null` when the backend could not read a payload
  /// scope. Never derived from a display string — see the Rust doc comment.
  agent: string | null;
  /// The `tab` half. `null` on the same terms as `agent`.
  tab: string | null;
  /// The conversation the row was filed under.
  ///
  /// **Not the join key, and the view must never present it as one.**
  /// Contamination is one row per TAB — H-2 made the bit sticky — so this names
  /// the conversation contamination *started* in. A tab contaminated in
  /// `sess-a` and `/clear`ed into `sess-b` stays contaminated and writes no
  /// second row.
  session: string | null;
  /// The tool that carried the content in, or the basis the bit was released on.
  tool: string;
  host: string | null;
  url: string | null;
  /// `internal` / `ipc` / `http`.
  origin: string | null;
  detail: string;
}

/// The `contamination_events` payload: the events plus the project root the
/// Timeline's checkpoints belong to (`activity::root_key` form). The frontend
/// cannot compute the second — the key is canonicalized backend-side from the
/// app's launch cwd — which is half of why the command exists.
export interface ContaminationFeed {
  root: string;
  events: ContaminationEvent[];
}

export function fetchContaminationEvents(root?: string): Promise<ContaminationFeed> {
  return invoke<ContaminationFeed>('contamination_events', { root: root ?? null });
}

// ── The join ───────────────────────────────────────────────────────────────

/// Whether a checkpoint was taken at or before an event.
///
/// **The unit boundary, in one place.** `Checkpoint.ts_unix` is epoch SECONDS
/// (git's commit date); `ContaminationEvent.ts_ms` is epoch MILLIS. Comparing
/// them raw is off by a factor of 1000 in the direction that makes every
/// checkpoint look older than every event, which fails silently — the join would
/// still return an answer, just always the newest checkpoint.
///
/// `<=` rather than `<`: an equal timestamp means the checkpoint precedes. A
/// checkpoint is taken *before* the prompt whose tool call contaminates, so at
/// the resolution where the two collide the checkpoint is the earlier fact, and
/// the merge order below is built on the same rule.
///
/// The residual, which no comparison can fix: seconds truncate, so a checkpoint
/// at 1000.7 s reports 1000 and looks up to 999 ms older than it is. Sub-second
/// ordering between a checkpoint and an event is therefore not knowable here.
export function precedes(cp: Checkpoint, event: ContaminationEvent): boolean {
  return cp.ts_unix * 1000 <= event.ts_ms;
}

/// What the Timeline found when it looked for the restore point that was live
/// when an event happened.
///
/// The three non-`none` cases exist because **a per-tab throttle does not give
/// every tab its own checkpoint**: throttling and dedup are separate gates, and
/// a tab that prompts into an unchanged tree gets none at all. So the nearest
/// preceding checkpoint routinely belongs to a different tab, or to no tab. The
/// view has to say which — presenting another tab's checkpoint as "this tab's
/// restore point" turns a security surface into a misleading one.
export type CheckpointLink =
  /// The nearest preceding checkpoint was taken by the same tab.
  | { kind: 'own'; checkpoint: Checkpoint }
  /// It was taken by a different, named tab.
  | { kind: 'other-tab'; checkpoint: Checkpoint }
  /// Either side carries no tab: a burst/manual/pre-restore checkpoint, one
  /// written before checkpoints recorded a tab, or an event whose scope the
  /// backend could not read.
  | { kind: 'unattributed'; checkpoint: Checkpoint }
  /// There is no preceding checkpoint at all.
  | { kind: 'none' };

/// The checkpoint that was live when `event` happened, and how confidently it
/// can be attributed to the event's tab.
///
/// Picks the nearest preceding checkpoint **overall**, not the nearest preceding
/// checkpoint *of this tab*: the question the row answers is "what did the
/// project look like just before this?", and that is the last snapshot taken,
/// whoever took it. Whether it is also *this tab's* is a separate fact, carried
/// in `kind` and stated in the copy rather than silently folded into the choice.
export function linkCheckpoint(
  event: ContaminationEvent,
  checkpoints: readonly Checkpoint[],
): CheckpointLink {
  let best: Checkpoint | null = null;
  for (const cp of checkpoints) {
    if (!precedes(cp, event)) continue;
    // Ties at one-second resolution are broken by `seq`, which is monotonic —
    // otherwise the answer would depend on the order the list arrived in.
    if (!best || cp.ts_unix > best.ts_unix || (cp.ts_unix === best.ts_unix && cp.seq > best.seq)) {
      best = cp;
    }
  }
  if (!best) return { kind: 'none' };
  if (best.tab === null || event.tab === null) return { kind: 'unattributed', checkpoint: best };
  return best.tab === event.tab
    ? { kind: 'own', checkpoint: best }
    : { kind: 'other-tab', checkpoint: best };
}

/// The sentence a row prints about its link. Exported (rather than inlined in
/// the markup) because these four sentences ARE the honesty requirement of this
/// step, and the only way to defend them is to assert them.
export function linkLine(link: CheckpointLink): string {
  switch (link.kind) {
    case 'own':
      return 'This tab took the last checkpoint before this event — restoring it rolls the files back to just before the content arrived.';
    case 'other-tab':
      return `The last checkpoint before this event was taken by ${link.checkpoint.tab}, not by this tab. It restores the whole project to that moment; it is not this tab's own restore point.`;
    case 'unattributed':
      return 'The last checkpoint before this event carries no tab (checkpoints taken by a burst of file activity, by hand, or before cImp recorded the tab), so cImp cannot say it belongs to this tab. It restores the whole project to that moment.';
    case 'none':
      return 'No checkpoint was taken before this event, so there is nothing here to restore to. Checkpoints are throttled per tab and skipped when the tree has not changed, so a tab can be contaminated without ever having produced one.';
  }
}

/// The checkpoint a "Restore to before this" action would target, or `null`
/// when there is none.
///
/// A separate accessor rather than a `kind !== 'none'` test at each call site:
/// the `none` case must render NO restore control at all — not a disabled one,
/// which reads as "temporarily unavailable" when the truth is "no such point
/// exists".
export function restoreTarget(link: CheckpointLink): Checkpoint | null {
  return link.kind === 'none' ? null : link.checkpoint;
}

// ── The merged rows ────────────────────────────────────────────────────────

/// One Timeline row. **A non-checkpoint row is new to this view as of step 5**;
/// before it, every row was a `Checkpoint` and the view keyed its `{#each}` on
/// `cp.id`. The discriminant is `kind`, and the key is `key` (checkpoint ids and
/// activity ids come from different id spaces and could collide).
export type TimelineRow =
  | { kind: 'checkpoint'; key: string; tsMs: number; checkpoint: Checkpoint }
  | {
      kind: 'contamination';
      key: string;
      tsMs: number;
      /// `agent:tab` for the event(s) behind this row.
      scope: string;
      agent: string | null;
      tab: string | null;
      /// The row that SET the bit, or `null` when only the clearing row is still
      /// retained (see [`evidenceNotices`] — retention is finite).
      opened: ContaminationEvent | null;
      /// The row that released it, when one has been written.
      cleared: ContaminationEvent | null;
      /// Always `{ kind: 'none' }` when `opened` is null: with no anchor there
      /// is no "before this" to join against.
      link: CheckpointLink;
    };

/// Merge checkpoints and contamination events into one newest-first list.
///
/// Events are filtered to `root` — the Timeline's checkpoints are this root's,
/// and a row from another project has no restore point here. What the other
/// roots' events become is [`evidenceNotices`]' job, not silence.
///
/// A **rootless** event (`root === ''`, "not attributable to a project" — see
/// [`ContaminationEvent.root`](ContaminationEvent)) matches no real root and is
/// therefore withheld here too. That is deliberate, not an oversight: it has no
/// project to correlate against these checkpoints. It is NOT silent, though —
/// [`evidenceNotices`] announces it as its own case, because #48 F-16 was a
/// rootless forensic row disappearing from exactly this kind of view.
export function buildTimelineRows(
  checkpoints: readonly Checkpoint[],
  events: readonly ContaminationEvent[],
  root: string,
): TimelineRow[] {
  const here = events.filter((e) => e.root === root);
  const opens = here.filter((e) => !e.cleared).sort((a, b) => a.ts_ms - b.ts_ms);
  const clears = here.filter((e) => e.cleared).sort((a, b) => a.ts_ms - b.ts_ms);
  const usedClears = new Set<number>();

  const rows: TimelineRow[] = checkpoints.map((cp) => ({
    kind: 'checkpoint' as const,
    key: `cp:${cp.id}`,
    tsMs: cp.ts_unix * 1000,
    checkpoint: cp,
  }));

  for (const opened of opens) {
    // The earliest not-yet-claimed clear at or after this opening, for the same
    // scope. Contamination can be set, cleared, and set again on one tab, so a
    // clear belongs to exactly one opening.
    const cleared =
      clears.find(
        (c) => !usedClears.has(c.id) && c.scope === opened.scope && c.ts_ms >= opened.ts_ms,
      ) ?? null;
    if (cleared) usedClears.add(cleared.id);
    rows.push({
      kind: 'contamination',
      key: `ct:${opened.id}`,
      tsMs: opened.ts_ms,
      scope: opened.scope,
      agent: opened.agent,
      tab: opened.tab,
      opened,
      cleared,
      link: linkCheckpoint(opened, checkpoints),
    });
  }

  // A clear whose opening is gone is still evidence — of a lifecycle that
  // started outside what the store still holds. Rendering only the openings
  // would delete the one row that says the flag was released.
  for (const cleared of clears) {
    if (usedClears.has(cleared.id)) continue;
    rows.push({
      kind: 'contamination',
      key: `ct:${cleared.id}`,
      tsMs: cleared.ts_ms,
      scope: cleared.scope,
      agent: cleared.agent,
      tab: cleared.tab,
      opened: null,
      cleared,
      link: { kind: 'none' },
    });
  }

  return rows.sort(compareRows);
}

/// Newest first, with a deterministic answer at every tie.
///
/// At an equal timestamp a contamination row sorts ABOVE its checkpoint, which
/// is the same rule [`precedes`] applies: the checkpoint is the earlier fact, so
/// in a newest-first list it is the lower one. The two must agree — a row that
/// says "the last checkpoint before this" while rendering below that checkpoint
/// is telling the user the opposite of what the list shows.
export function compareRows(a: TimelineRow, b: TimelineRow): number {
  if (a.tsMs !== b.tsMs) return b.tsMs - a.tsMs;
  const rank = (r: TimelineRow): number => (r.kind === 'contamination' ? 0 : 1);
  if (rank(a) !== rank(b)) return rank(a) - rank(b);
  if (a.kind === 'checkpoint' && b.kind === 'checkpoint') {
    return b.checkpoint.seq - a.checkpoint.seq;
  }
  if (a.kind === 'contamination' && b.kind === 'contamination') {
    return (b.opened?.id ?? b.cleared?.id ?? 0) - (a.opened?.id ?? a.cleared?.id ?? 0);
  }
  return 0;
}

/// The icon a row wears. Contamination is deliberately not a checkpoint glyph:
/// it is an *event*, not a restore point.
export function rowIcon(row: TimelineRow): string {
  if (row.kind === 'contamination') return row.cleared ? '☑' : '☣';
  return triggerIcon(row.checkpoint.trigger);
}

/// The row's `title=` — what the icon means, spelled out.
export function rowTitle(row: TimelineRow): string {
  if (row.kind === 'contamination') {
    return row.cleared
      ? 'External content entered this tab here; the flag has since been cleared'
      : 'External content entered this tab here — the conversation has been contaminated ever since';
  }
  return triggerTitle(row.checkpoint.trigger);
}

/// **A `default` arm, deliberately.** `Checkpoint` is hand-mirrored from Rust
/// with no codegen, so a fifth `Trigger` variant would arrive here as a string
/// this union does not contain: an exhaustive switch would return `undefined`
/// and the row would render a blank cell with no error anywhere. Now that the
/// view is no longer homogeneous, the blank would read as "not a checkpoint".
export function triggerIcon(trigger: Checkpoint['trigger']): string {
  switch (trigger) {
    case 'prompt':
      return '💬';
    case 'burst':
      return '⚡';
    case 'manual':
      return '📌';
    case 'pre-restore':
      return '⏮';
    /// V33 (C8). A wrench, not another lightning bolt: the point of this
    /// trigger is that a specific TOOL is about to run, and it has to be
    /// tellable apart from `burst` (which fires *after* writes land) at a
    /// glance, in a column one character wide.
    case 'tool':
      return '🔧';
    default:
      return '•';
  }
}

export function triggerTitle(trigger: Checkpoint['trigger']): string {
  switch (trigger) {
    case 'prompt':
      return 'Automatic — fired by a prompt';
    case 'burst':
      return 'Automatic — fired by a burst of file activity';
    case 'manual':
      return 'Manual — "Checkpoint now"';
    case 'pre-restore':
      return 'Automatic safety net taken right before a restore';
    case 'tool':
      return 'Automatic — taken immediately before a file-changing tool call; restoring here undoes that one call';
    default:
      return `Checkpoint (trigger "${String(trigger)}" is not one this build knows)`;
  }
}

/// A checkpoint's `source` (`harness:tool_name`) rendered for a Timeline row,
/// or `null` when there is nothing to show.
///
/// **Absent is the normal case, not a gap.** `undefined` (a backend older than
/// contract C8), `null` (no tool behind this checkpoint) and `''` (present but
/// empty — "empty is not absent" would otherwise let a blank badge through)
/// all collapse to `null` so the caller renders nothing at all rather than a
/// placeholder on every prompt/burst/manual row.
///
/// A value that is not in `harness:tool` shape is shown verbatim rather than
/// dropped: this is hand-mirrored data with no codegen, and silently hiding an
/// unrecognized provenance string is exactly the failure `triggerIcon`'s
/// `default:` arm exists to avoid.
export function checkpointSource(
  source: string | null | undefined,
): { harness: string | null; text: string; title: string } | null {
  const raw = (source ?? '').trim();
  if (raw === '') return null;
  const cut = raw.indexOf(':');
  const tool = cut > 0 ? raw.slice(cut + 1).trim() : '';
  // A colon with nothing usable after it is not `harness:tool` — fall back to
  // the whole value rather than render an empty badge with a confident title.
  const harness = tool === '' ? null : raw.slice(0, cut);
  const text = tool === '' ? raw : tool;
  return {
    harness,
    text,
    title: harness
      ? `Taken immediately before this tab's ${harness} "${text}" tool call — restoring here undoes that call`
      : `Taken immediately before the "${text}" tool call — restoring here undoes that call`,
  };
}

/// How a contamination bit was released, in the user's words.
///
/// The three bases are very different claims about what is in the model's
/// context window — "the user judged the content harmless" versus "the user took
/// the whole risk knowingly" versus "the user restored and the tab then started a
/// new conversation" — and the audit trail is only legible if the row says which.
/// Falls through to the raw basis for a value this build does not know, rather
/// than to a confident sentence about the wrong one.
export function clearedLine(event: ContaminationEvent): string {
  switch (event.tool) {
    case 'clear_contamination':
      return 'Cleared — marked a false positive. Nothing was restarted and nothing was rolled back; the conversation was left as it was.';
    case 'unlatch':
      // Decision 15's 2026-08-10 amendment. Without this arm the row rendered
      // the bare fallback `Cleared (unlatch).`, which names a wire value and
      // says nothing about what the user decided.
      return 'Cleared — the user restored full access to this tab, which releases the flag with it (the larger risk was accepted deliberately).';
    case 'session_clear_observed':
      return 'Cleared — a checkpoint was restored, and cImp then saw this tab start a new session.';
    default:
      return `Cleared (${event.tool}).`;
  }
}

// ── What the view must say when it cannot show everything ───────────────────

/// A banner above the rows. Never decorative: each one exists because rendering
/// the list alone would be a confident claim that is not true.
export interface EvidenceNotice {
  kind: 'error' | 'not-retained' | 'other-root' | 'rootless';
  text: string;
}

export interface EvidenceInput {
  /// Every event the backend returned, ALL roots.
  events: readonly ContaminationEvent[];
  /// The Timeline's own root, from the feed.
  root: string;
  /// Every latch row currently known (`latchByTab`). The live truth about which
  /// tabs are contaminated *now*, independent of what the feed still holds.
  latch: readonly LatchRow[];
  /// The contamination fetch's error, if it failed.
  error: string | null;
}

/// The notices the Timeline must show above its rows.
///
/// The `not-retained` one is the reason this function exists. Activity rows are
/// capped per screen and evicted oldest-first, so a contamination row **can**
/// age out — and a tab working against another directory records its
/// contamination there. In both cases the tab is still flagged and the Timeline
/// has nothing to show for it, and "nothing to show" rendered as an empty list
/// is indistinguishable from "this project was never contaminated". One of those
/// is reassuring and wrong.
///
/// It deliberately does not name a cause: eviction, a different root, and a
/// cleared history all produce the same observation, and picking one would be a
/// guess printed as a fact.
///
/// The `rootless` one is #48 F-16 in this view. `root === ''` means "not
/// attributable to a project", so such an event is withheld by
/// [`buildTimelineRows`] like any non-matching root — but it must not be
/// DESCRIBED as another project's, and it must not be silent, because the row
/// F-16 found missing was one recording that a credential had been held. Its own
/// notice, and it is subtracted from the `other-root` count so that sentence
/// keeps meaning what it says.
export function evidenceNotices(input: EvidenceInput): EvidenceNotice[] {
  const notices: EvidenceNotice[] = [];
  if (input.error) {
    notices.push({
      kind: 'error',
      text: `Contamination history could not be read: ${input.error}. The checkpoints below are unaffected — but this view cannot currently tell you whether any tab's conversation was contaminated.`,
    });
  }

  const seenTabs = new Set(
    input.events.map((e) => e.tab).filter((t): t is string => t !== null),
  );
  const orphaned = input.latch
    .filter((r) => r.contaminated && !seenTabs.has(r.tab))
    .map((r) => `${r.consumer}:${r.tab}`)
    .sort();
  if (orphaned.length > 0) {
    notices.push({
      kind: 'not-retained',
      text: `${orphaned.length} tab${orphaned.length === 1 ? ' is' : 's are'} flagged as contaminated right now with no event retained to show for it (${orphaned.join(', ')}). The event can age out of the activity feed, and a tab working in another directory records it there. Nothing below can be correlated to those tabs — this is not "they were never contaminated".`,
    });
  }

  // #48 F-16: `''` is "not attributable to a project", so a rootless event is
  // neither here nor at another root — and calling it another project's would be
  // a fact this row does not carry. Counted (and announced) separately, and
  // excluded from `elsewhere` below so that sentence stays true.
  const isRootless = (e: ContaminationEvent) => e.root.trim() === '';
  const elsewhere = input.events.filter(
    (e) => !isRootless(e) && e.root !== input.root && !e.cleared,
  ).length;
  if (elsewhere > 0) {
    notices.push({
      kind: 'other-root',
      text: `${elsewhere} contamination event${elsewhere === 1 ? '' : 's'} came from another project directory and ${elsewhere === 1 ? 'is' : 'are'} not shown. This Timeline covers ${input.root}; a tab running in a worktree writes both its checkpoints and its containment history against that worktree instead.`,
    });
  }

  // Only when this Timeline HAS a root: with no root of its own it is not
  // narrowing by one, so nothing is being withheld to announce.
  const rootless = input.events.filter((e) => isRootless(e) && !e.cleared).length;
  if (rootless > 0 && input.root.trim() !== '') {
    notices.push({
      kind: 'rootless',
      text: `${rootless} contamination event${rootless === 1 ? '' : 's'} ${rootless === 1 ? 'names' : 'name'} no project directory and ${rootless === 1 ? 'is' : 'are'} not shown. That is not the same as coming from another project: cImp could not attribute ${rootless === 1 ? 'it' : 'them'} to one at all, so ${rootless === 1 ? 'it cannot' : 'they cannot'} be correlated with the checkpoints below. Open the Events tab to read ${rootless === 1 ? 'it' : 'them'} — the row is retained in full there.`,
    });
  }
  return notices;
}

// ── Step 5d: the flag and the latch are two different holds ─────────────────

/// Why clearing the contamination flag may not give the user their memory
/// writes back — the sentence, in the one place both surfaces read it from.
///
/// **The gap it closes.** `Latch::proxy_gate` quarantines a PERSISTENT-WRITE
/// whenever the latch is EXTERNAL, on the latch's own authority; the
/// contamination bit only ever *widens* that verdict. So clearing the flag on an
/// EXTERNAL-latched tab releases nothing the user can see: the notes are still
/// held, and step 4 shipped with no copy anywhere saying why. Returns `null` for
/// every other latch position, where clearing the flag really is the whole hold —
/// a warning shown when it does not apply is how warnings stop being read.
///
/// Lives here rather than in either component for the reason `latch.ts` gives
/// for `reducedTabLine`: two surfaces stating the same rule in their own words
/// is how they come to disagree (#48, G-2).
export function latchAlsoHoldsMemory(latch: string | undefined): string | null {
  if (latch !== 'external') return null;
  return 'Clearing the flag will not release this tab’s memory writes on its own: the tab is latched to external content, and the latch quarantines writes on its own authority, whatever the flag says. Move the latch as well — the containment badge’s "Switch to local" (which keeps the flag) or "Restore full access" (which also clears it) — before notes save normally again.';
}

/// What the Workbench says when checkpoints are off, so this view does not exist.
///
/// The Timeline is gated on `settings.workbench.checkpoints`, and containment
/// must not become unreachable in a configuration the user is allowed to choose.
/// With tabs actually contaminated, the off-state banner has to say so and point
/// at the control that still works — silence there reads as "nothing is wrong".
export function evidenceOffNotice(contaminatedScopes: readonly string[]): string {
  const base =
    'Contamination events are shown on this Timeline, so with checkpoints off there is nothing here to correlate them against.';
  if (contaminatedScopes.length === 0) {
    return `${base} The containment badge on a tab still shows and clears its flag.`;
  }
  const list = [...contaminatedScopes].sort().join(', ');
  return `${contaminatedScopes.length} tab${contaminatedScopes.length === 1 ? ' is' : 's are'} flagged as contaminated right now (${list}). ${base} Open the tab’s containment badge to review or clear the flag.`;
}
