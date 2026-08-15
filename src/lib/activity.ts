// The unified, persistent tool-activity store (backend `crate::activity`) —
// invoke wrappers for the Tool Activity tab. Entries cover graph/context tool
// calls, completed offload_task runs, Code Audit tool runs, AND proxied MCP
// tool calls; they survive app restarts, and each keeps a (truncated) copy of
// the actual request/response for the detail popup.
import { invoke } from '@tauri-apps/api/core';

/// Who a row is attributed to — the "which tab is doing what" column (#51).
/// Mirror of Rust `activity::Attribution`, an externally-tagged serde enum:
/// the two unit variants arrive as bare strings, the two id-carrying variants
/// as single-key objects.
///
/// **Collapsing any two of these is a bug** (the Rust doc says the same, and
/// the reason is worth repeating on this side, where the rendering happens):
/// * `{ tab }` — a configured AI tab. The ONLY state that may render as a tab.
/// * `{ unrecognized }` — a non-empty id naming no configured tab. Rendering it
///   as a tab would attribute activity to a tab that does not exist, inside the
///   view whose whole job is attribution.
/// * `'headless'` — positively no tab (`claude -p`, cron, worker tasks, cImp's
///   own internal work). A fact about the caller, not missing data.
/// * `'unattributed'` — the writer did not know, or the row predates #51.
///   "Nobody was on a tab" and "we weren't recording it" are different facts
///   and only one of them is evidence, so this must never look like `headless`.
export type Attribution =
  | 'unattributed'
  | 'headless'
  | { tab: string }
  | { unrecognized: string };

/// The four attribution states, flattened to a tag the UI can switch on.
export type AttributionState = 'tab' | 'unrecognized' | 'headless' | 'unattributed';

/// Classify an `Attribution` at the parse boundary.
///
/// Deliberately defensive: anything this build does not recognize (a variant
/// added later, a malformed payload, a non-string id) degrades to
/// `'unattributed'` — "we don't know" — and NEVER to `'tab'`. The upstream
/// serde enum guarantees the shape today; that guarantee is exactly the kind
/// that quietly gains a fallback path later, and the cost of being wrong here
/// is inventing a tab that never ran anything.
export function attributionState(a: Attribution | null | undefined): AttributionState {
  if (a === 'headless') return 'headless';
  if (a === 'unattributed' || a === null || a === undefined) return 'unattributed';
  if (typeof a === 'object') {
    if ('tab' in a && typeof a.tab === 'string' && a.tab !== '') return 'tab';
    if ('unrecognized' in a && typeof a.unrecognized === 'string' && a.unrecognized !== '') {
      return 'unrecognized';
    }
  }
  return 'unattributed';
}

/// The id carried by the two id-bearing states, else null. Callers must pair
/// this with `attributionState` — the id alone cannot tell a real tab from an
/// id that merely named one.
export function attributionId(a: Attribution | null | undefined): string | null {
  if (a !== null && a !== undefined && typeof a === 'object') {
    if ('tab' in a && typeof a.tab === 'string' && a.tab !== '') return a.tab;
    if ('unrecognized' in a && typeof a.unrecognized === 'string' && a.unrecognized !== '') {
      return a.unrecognized;
    }
  }
  return null;
}

/// Whether a row is attributable to the REAL tab `id` — the predicate a
/// "filter by tab" must use (mirror of Rust `Attribution::is_tab`). False for
/// `{ unrecognized: id }` on purpose: filtering by a tab id must never surface
/// a row that merely quoted that id.
export function isTabAttribution(a: Attribution | null | undefined, id: string): boolean {
  return attributionState(a) === 'tab' && attributionId(a) === id;
}

/// One activity, without payloads. Mirror of Rust `activity::ActivityEntry`.
export interface ActivityEntry {
  /// Stable id (unique across restarts) — delete/detail key on it.
  id: number;
  ts_ms: number;
  /// `graph` = a graph/context tool call; `offload` = an offload_task run;
  /// `audit` = one Code Audit tool run (V23); `mcp` = one proxied MCP tool
  /// call (`<server>__<tool>` through the warm host); `injection_flag` = one
  /// V32 injection-containment event (SSRF screen, external-fetch budget,
  /// canary hit, taint-latch refusal, a memory-quarantine hold, or a
  /// surface-only detection flag); `offload_server` = one local offload
  /// **server process** transition — spawned, healthy, stopped, or failed.
  ///
  /// `offload` and `offload_server` are different feeds on purpose: the first
  /// is one *task* a server ran, the second is the server itself. A backend
  /// that is failing produces no task rows at all, which is exactly why its
  /// process history could not share their retention window (`OFFLOAD_SERVER_CAP`
  /// backend-side).
  ///
  /// `sandbox` = one OS-sandbox fact (V33 Phase A): a child that ran
  /// UNSANDBOXED and why, or a grant/drive-mapping event. Its own kind rather
  /// than an `injection_flag` source because the two answer different
  /// questions — V32's screens say a model was refused something, this says
  /// the OS boundary was or was not in place — and because it carries its own
  /// retention lane (`SANDBOX_CAP` backend-side).
  kind:
    | 'graph'
    | 'offload'
    | 'offload_server'
    | 'audit'
    | 'mcp'
    | 'injection_flag'
    | 'sandbox';
  /// The project this row belongs to, canonicalized backend-side
  /// (`activity::root_key`). Lets a per-project consumer filter out other
  /// projects' activity.
  ///
  /// **Empty means "not attributable to a project" — a claim, not a gap**
  /// (#48 F-16). It covers a writer that could not derive one (an offload run
  /// with no session cwd) AND a row that is genuinely not about a project (the
  /// harness `contract_drift` report), and it is deliberately ONE sentinel: it is
  /// also what a row written before this column existed deserializes to, and such
  /// a row is honestly unknown. A future recorder that positively has "no
  /// project" as a *fact* must not reuse this — at that point `root` becomes a
  /// tagged union the way [`Attribution`] is, and this comment is the note saying
  /// so. Mirror of the Rust doc on `activity::ActivityEntry::root`; the two must
  /// keep saying the same thing.
  ///
  /// **Consumers must never silently HIDE a row for being rootless.** F-16's live
  /// reproduction was a memory-quarantine row that recorded a CREDENTIAL being
  /// held, written with `root: ""` — so in a project-scoped review it was the one
  /// row that vanished. The producers that owe a root are now asserted backend-
  /// side (`Screen::requires_project_root`), but the screens that legitimately
  /// have none still write `""`, so any view that narrows by root must say what it
  /// is not showing. See [`FeedFilter`] for where that rule lands here, and
  /// `timeline.ts`'s `evidenceNotices` for the pattern that says it out loud.
  root: string;
  /// Agent (claude/opencode/offload/read_advisor/auto_check) for graph
  /// entries; the backend name for offload entries. For `injection_flag` rows
  /// it names the SCREEN that fired: `ssrf` / `budget` / `canary` /
  /// `latch_refusal` / `memory_quarantine` / `signature` / `classifier`, plus
  /// `unscreened` for a result delivered after the detection surface did LESS
  /// than a full pass over it (a truncated or skipped scan — "not flagged" is
  /// not "clean"), `updater` for the V32 C3 detection auto-updater (whose
  /// `tool` is the component and whose `ok` is the outcome — `rejected` is the
  /// only false), `latch_override` for a user-applied latch move,
  /// `latch_beacon` for a native-web beacon engaging one, and `contamination`
  /// for the moment a tab's conversation stopped being clean (one row per tab,
  /// naming the tool and page that did it — #48 finding F-3). Rendered as free
  /// text, so a source this build does not know still reads correctly: it just
  /// gets no accent colour. Every row's request payload carries an
  /// `origin` (`internal` / `ipc` / `http`) naming who asked — `ipc` is the only
  /// one that means a human acted (#45) — and a `session` naming the harness
  /// conversation, when the writer knew it.
  source: string;
  tool: string;
  target: string;
  chars: number;
  ms: number;
  /// Call outcome — but read it together with `kind`/`source`, not alone:
  /// `injection_flag` rows invert it (a DENIAL is `false`, a detection flag
  /// that was still delivered is `true`), and the telemetry-channel sources
  /// record `false` to mean "this signal fired", not "this call failed".
  /// `status()` in EventsView.svelte is the one place that untangles it.
  ok: boolean;
  /// #51: which tab this row belongs to. Always present on the wire (Rust
  /// serializes the default), so no optionality here — an absent field on an
  /// old persisted row is repaired backend-side into `'unattributed'`.
  tab: Attribution;
  /// #51: the harness conversation the caller was in, when the writer knew it.
  /// A SEPARATE field from `tab` on purpose: a tab outlives its conversations,
  /// so `tab` alone cannot answer "which conversation was this?". Null for a
  /// worker task, a tab whose session the registry withholds, and every
  /// pre-#51 row.
  session: string | null;
}

/// The full record: entry + captured payloads. Mirror of Rust
/// `activity::ActivityRecord` (which flattens the entry).
export interface ActivityRecord extends ActivityEntry {
  request: string;
  response: string;
}

// ── Row status (#48, M-24) ────────────────────────────────────────────────
//
// **Why this is here and not in either feed component.** Both the Tool Activity
// tab and the Events tab render this store, and both collapsed every
// `injection_flag` row into one treatment: Tool Activity painted the whole kind
// chip danger-red ("the only kind with a tinted chip"), and Events mapped
// `ok ? 'flagged' : 'denied'`. So `unscreened`, the two detector screens,
// `memory_quarantine` and `latch_override` all arrived on screen as the same
// alarm — and `unscreened`, whose entire meaning is *"we did not look at all of
// it"*, read as *"we blocked something"*, which is the opposite of the truth.
// `latch_override` — a user GRANTING capability back — read as containment
// firing.
//
// One classifier, in the `.ts` file that has a test harness, consumed by both
// feeds: the security vocabulary of this app must not differ between two tabs
// showing the same rows.

/// Sources that are TELEMETRY CHANNELS rather than tool invocations:
/// `read_advisor` reports advisor reminders and full-file-`Read` bypasses,
/// `harness` reports contract/sub-agent drift. Both record `ok: false` to mean
/// "this signal fired", not "this call failed" — so painting them with the error
/// colour made the feed read as mostly-broken when nothing had broken (20 of 28
/// red rows on one machine were bypass canaries).
export const CANARY_SOURCES = new Set(['read_advisor', 'harness']);

/// What one feed row actually reports, as one word.
///
/// The word IS the class name and the rendered label (the existing Events-tab
/// idiom), so a state that has no word here cannot be drawn.
///
/// Three plain call outcomes:
/// - `ok` — the call worked.
/// - `failed` — the call failed.
/// - `signal` — a telemetry channel fired ([`CANARY_SOURCES`]); not a failure.
///
/// …and nine `injection_flag` outcomes, which are NOT interchangeable:
/// - `denied` — a screen stopped the call. The only one that means "we blocked
///   something", and the only one wearing danger. Name and semantics kept from
///   the Events tab's own `denied`/`flagged` split, which this EXTENDS rather
///   than replaces: two chip vocabularies for the same rows across two views
///   would be a fresh reporting-honesty defect of exactly M-24's kind.
/// - `flagged` — a detector matched and the result was delivered anyway
///   (detection is surface-only, locked decision 5).
/// - `unscreened` — part of the result was never looked at. Nothing found,
///   nothing stopped: the absence of a verdict is not a verdict of absence.
/// - `held` — a memory write was stored and withheld pending human review.
/// - `engaged` — containment came ON for a tab (a native-web beacon; a
///   conversation becoming contaminated). Nothing was refused.
/// - `granted` — a user gave capability back (a latch override; a contamination
///   flag cleared). A release, not a block — which is exactly why it must not
///   share a treatment with one.
/// - `update` / `rejected` — the detection auto-updater acted, or refused a
///   bundle. Not a screen over a tool call at all.
/// - `recorded` — an `injection_flag` source this build has no category for.
///   Deliberately NOT folded into `blocked` or `flagged`: a future screen must
///   render as "we do not have a word for this" rather than inherit a claim.
///
/// …and four `offload_server` outcomes. `stopped` and `down` are the pair that
/// must not merge, for the same reason `denied` and `granted` must not: cImp
/// killing a server on purpose and a server failing to come up are opposite
/// facts that both end with no process running, and only one of them is
/// something going wrong.
/// - `started` — the process was spawned. Says nothing about health yet.
/// - `ready` — it answered `/health`; the window and slot count were read.
/// - `stopped` — cImp stopped it deliberately (see the row's target for which
///   intent: user, restart, or app shutdown). Not a failure.
/// - `down` — it never came up, or it ended without cImp stopping it.
export type RowStatus =
  | 'ok'
  | 'failed'
  | 'signal'
  | 'started'
  | 'ready'
  | 'stopped'
  | 'down'
  | 'denied'
  | 'flagged'
  | 'unscreened'
  | 'held'
  | 'engaged'
  | 'granted'
  | 'update'
  | 'rejected'
  | 'recorded'
  /// V33 Phase A: a child ran outside the OS sandbox. Distinct from `denied`
  /// (a model was refused) and from `failed` (the command itself broke) — the
  /// command ran fine, the boundary was absent.
  | 'unsandboxed';

/// Classify one row.
///
/// `ok` is read as the denial predicate ONLY for the screens that follow it.
/// `Screen::is_denial` is the backend's rule and `record_flag` publishes it as
/// `ok: false` — but `updater` rows are documented as the one source written
/// outside `record_flag`, where `ok` is the bundle OUTCOME (`rejected ⇒ false`).
/// Reading `!ok` as "denied" there would report a refused rules bundle as a
/// blocked tool call, which is the same collapse this function exists to undo,
/// so `updater` is matched on `source` before `ok` is consulted at all.
export function rowStatus(e: ActivityEntry): RowStatus {
  if (e.kind === 'offload_server') {
    // Keyed on `tool` (the transition), never on `ok` alone: `ok` is true for
    // BOTH a healthy start and a deliberate stop, so reading it by itself
    // would render "the server is gone" and "the server is up" as one word.
    switch (e.tool) {
      case 'start':
        return 'started';
      case 'ready':
        return 'ready';
      case 'stop':
        return 'stopped';
      case 'fail':
        return 'down';
      default:
        // A transition added backend-side that this build predates. `ok` is
        // documented as the transition's outcome, which is a claim we can
        // still make; the verb it belongs to is not.
        return e.ok ? 'ok' : 'down';
    }
  }
  if (e.kind === 'sandbox') {
    // Keyed on `tool`, like `offload_server` and for the same reason: `ok`
    // here distinguishes a CHOSEN unsandboxed state (the switch is off) from
    // an unavailable one, so it cannot also carry the verb. Locked decision
    // 17 requires those two to stay visibly distinct, which the row's
    // `target` text spells out ("off (user choice)" / "unavailable").
    return e.tool === 'unsandboxed' ? 'unsandboxed' : e.ok ? 'ok' : 'failed';
  }
  if (e.kind === 'injection_flag') {
    switch (e.source) {
      // Checked first: its `ok` is an outcome, not a denial (see above).
      case 'updater':
        return e.ok ? 'update' : 'rejected';
      case 'signature':
      case 'classifier':
        return 'flagged';
      case 'unscreened':
        return 'unscreened';
      case 'memory_quarantine':
        return 'held';
      case 'latch_beacon':
      case 'contamination':
        return 'engaged';
      case 'latch_override':
      case 'contamination_cleared':
        return 'granted';
      case 'ssrf':
      case 'budget':
      case 'canary':
      case 'latch_refusal':
        return 'denied';
      default:
        // A screen added backend-side that this build predates. `ok: false`
        // still carries `Screen::is_denial`, which is a claim we can make; a
        // delivered one gets the no-category word rather than a borrowed one.
        return e.ok ? 'recorded' : 'denied';
    }
  }
  if (!e.ok) return CANARY_SOURCES.has(e.source) ? 'signal' : 'failed';
  return 'ok';
}

/// The tooltip each status wears. The first five are the Events tab's own
/// sentences, kept; every state ADDED here says explicitly what was *not*
/// blocked, because reading a containment row as a block is the whole of M-24.
export const STATUS_TITLE: Record<RowStatus, string> = {
  ok: 'Call succeeded',
  failed: 'Call failed',
  started:
    'The offload server process was spawned. It is not healthy yet — the `ready` row is what says it came up, and the gap between them is the model load.',
  ready:
    'The offload server answered /health. Its duration is time-to-healthy, model load included.',
  stopped:
    'cImp stopped this offload server deliberately — the row says which (user, restart, or app shutdown), and its duration is how long the server had been up. Not a failure.',
  down: 'The offload server did not come up, or it ended without cImp stopping it. Open the row for the reason.',
  denied: 'Blocked by an injection-containment screen',
  flagged: 'Delivered, but a detection screen flagged it — nothing was blocked',
  signal: 'A telemetry signal fired — not a failure',
  unscreened:
    'Part of this result was never screened — a size cap dropped some of it, or a layer did not finish. Nothing was found and nothing was blocked: the absence of a verdict is not a verdict of absence.',
  held: 'A memory write was stored but withheld from every read path until you review it in Code Intelligence → Memory. Nothing was blocked.',
  engaged:
    'Containment came ON for a tab — a native-web beacon, or a conversation becoming contaminated. Nothing was blocked.',
  granted:
    'A user action gave capability BACK — a latch override, or a contamination flag cleared. This is a release, not a block.',
  update:
    'The detection auto-updater acted on the rules/classifier bundle. Not a screen over a tool call.',
  rejected:
    'The detection auto-updater REFUSED a rules/classifier bundle. Not a screen over a tool call, and not a blocked call.',
  recorded:
    'A containment event this build has no category for. Open the row for the detail.',
  unsandboxed:
    'This command ran WITHOUT the OS sandbox — the row says whether that was your choice (the Sandboxing switch is off) or a missing prerequisite. The command itself ran normally; only the OS boundary was absent.',
};

/// The feed (graph + offload), newest first, payload-free. Pass `sinceTs` to
/// fetch only entries newer than a high-water mark; omit it for the full
/// list (both feeds poll the full list — they need an authoritative snapshot
/// to reflect deletions).
///
/// There is no server-side filter, by design: the store is capped per lane at
/// ~1,570 payload-free rows, so the read this would shrink cannot grow; the
/// filter bar's option lists have to come from an unfiltered read anyway; and
/// a backend filter would mean a second copy of the four-state rule below,
/// with only one copy exercised. `FeedFilter` narrows here instead.
export function activityList(sinceTs?: number): Promise<ActivityEntry[]> {
  return invoke<ActivityEntry[]>('activity_list', { sinceTs: sinceTs ?? null });
}

/// One activity's full record for the detail popup. Resolves null when the
/// entry vanished (deleted / aged out) between the list poll and the click.
export function activityDetail(id: number): Promise<ActivityRecord | null> {
  return invoke<ActivityRecord | null>('activity_detail', { id });
}

/// Delete one entry (persists immediately).
export function activityDelete(id: number): Promise<void> {
  return invoke<void>('activity_delete', { id });
}

/// Clear the whole history (persists immediately).
export function activityClear(): Promise<void> {
  return invoke<void>('activity_clear');
}

// ── Feed filtering (Events tab, #51) ──────────────────────────────────────
//
// CLIENT-SIDE, and this is the only implementation — a server-side filter was
// written and then deliberately removed. Three reasons:
//
//  1. A wire filter can only name a real tab. Three of the four attribution
//     states (headless / unattributed / unrecognized) have no wire form, and
//     those are precisely the selections this view exists to offer — "which
//     rows had no tab at all" is the question, not a corner case.
//  2. The kind / source / tab OPTION lists are derived from the feed, so they
//     must come from an UNFILTERED read. A pre-narrowed poll can only offer
//     back the option already selected, stranding the user on one filter — so
//     a backend filter would have been a SECOND request beside the full one,
//     not a replacement for it.
//  3. There is nothing to shrink. The store is capped per lane at ~1,570
//     payload-free rows by construction, and the Tool Activity tab has
//     full-polled this same store every couple of seconds since v0.41.0.
//
// What settled it was the duplication rather than the dead code: filtering on
// both sides means two copies of the rule in `matchesTabFilter` below, only
// one of them exercised. That rule — whether an `unrecognized` id counts as
// its tab — is what the whole view rests on, and it fails by showing MORE than
// was asked. One implementation, in the layer that runs it.

/// The "no constraint" value for every filter axis. A sentinel rather than
/// `null` because these are bound straight to `<select>` values, which are
/// strings.
export const FILTER_ANY = '*';

/// A tab-filter selection. Either `FILTER_ANY`, one of the three non-tab
/// STATES, or `tab:<id>` for one real tab.
///
/// The `tab:` prefix is load-bearing: without it a tab whose id happened to be
/// `headless` would silently take over the headless-state option.
export type TabFilterValue = string;
export const TAB_FILTER_HEADLESS = 'headless';
export const TAB_FILTER_UNATTRIBUTED = 'unattributed';
export const TAB_FILTER_UNRECOGNIZED = 'unrecognized';
export const TAB_FILTER_PREFIX = 'tab:';

/// Build the filter value that selects exactly the real tab `id`.
export function tabFilterValue(id: string): TabFilterValue {
  return TAB_FILTER_PREFIX + id;
}

/// Does this row's attribution satisfy the tab filter?
///
/// **The one rule that must not regress:** `tab:x` matches `{ tab: 'x' }` and
/// never `{ unrecognized: 'x' }`. The unrecognized rows are reachable through
/// their own option instead, where they are labelled as what they are.
export function matchesTabFilter(a: Attribution | null | undefined, value: TabFilterValue): boolean {
  if (value === FILTER_ANY) return true;
  const state = attributionState(a);
  if (value === TAB_FILTER_HEADLESS) return state === 'headless';
  if (value === TAB_FILTER_UNATTRIBUTED) return state === 'unattributed';
  if (value === TAB_FILTER_UNRECOGNIZED) return state === 'unrecognized';
  if (value.startsWith(TAB_FILTER_PREFIX)) {
    return isTabAttribution(a, value.slice(TAB_FILTER_PREFIX.length));
  }
  // An option this build doesn't know (a stale persisted selection) narrows to
  // nothing rather than silently widening to everything — a filter that shows
  // MORE than asked is the failure mode that misleads in an attribution view.
  return false;
}

/// The three filter axes the Events tab exposes — the UI-level selection,
/// distinct from a wire-level filter: every axis is always
/// set (a `<select>` always has a value) and `FILTER_ANY` is how "don't
/// constrain it" is spelled.
///
/// **There is deliberately no `root` axis, and adding one carries a rule**
/// (#48 F-16). [`ActivityEntry.root`](ActivityEntry) uses `''` for "not
/// attributable to a project", which is a CLAIM and not a gap — so a
/// `root === selected` comparison drops every rootless row, including the ones
/// F-16 was about (a memory-quarantine row recording that a credential was held).
/// Whoever adds this axis owes two things with it:
///
///  1. rootless rows are **surfaced, not silenced** — either kept in the view or
///     announced above it, the way `timeline.ts`'s `evidenceNotices` announces
///     the events it is withholding; and
///  2. they are never described as belonging to *another* project, because `''`
///     does not say that.
///
/// The one root filter that exists today is the Timeline's
/// (`timeline.ts`'s `buildTimelineRows` / `evidenceNotices`), and it is written
/// to that rule — copy it rather than re-deriving it.
export interface FeedFilter {
  kind: string;
  source: string;
  tab: TabFilterValue;
}

export const NO_FILTER: FeedFilter = {
  kind: FILTER_ANY,
  source: FILTER_ANY,
  tab: FILTER_ANY,
};

export function isNoFilter(f: FeedFilter): boolean {
  return f.kind === FILTER_ANY && f.source === FILTER_ANY && f.tab === FILTER_ANY;
}

/// Narrow a feed by all three axes. Returns `entries` ITSELF when nothing is
/// constrained, so an unfiltered Events tab keeps `mergeEntries`' referential
/// stability instead of handing Svelte a fresh array every poll.
export function filterEntries(entries: ActivityEntry[], f: FeedFilter): ActivityEntry[] {
  if (isNoFilter(f)) return entries;
  return entries.filter(
    (e) =>
      (f.kind === FILTER_ANY || e.kind === f.kind) &&
      (f.source === FILTER_ANY || e.source === f.source) &&
      matchesTabFilter(e.tab, f.tab),
  );
}

/// Splice a freshly-fetched feed onto the one already on screen, REUSING the
/// object already held for any id that is already present.
///
/// **The invariant this rests on:** the backend assigns an id at record time
/// and never rewrites an entry afterwards — `crate::activity` only ever
/// appends, deletes, or clears — so an id already held identifies
/// byte-identical content. If an update-in-place path is ever added there, this
/// reuse goes stale and must be revisited.
///
/// Why it matters: reuse is what keeps each rendered row's expressions
/// referentially stable. A freshly parsed IPC payload otherwise hands every row
/// a NEW object identity on every poll, so the whole feed (up to ~1.4k rows at
/// the per-lane caps, each with several helper calls) re-evaluates even though
/// only the newest entry actually changed. That full-table churn is what shows
/// up as hover lag once a second agent tab is filling the feed.
///
/// Returns `prev` itself when nothing moved, so the caller's assignment is a
/// no-op reference write that Svelte skips entirely.
export function mergeEntries(prev: ActivityEntry[], next: ActivityEntry[]): ActivityEntry[] {
  const byId = new Map(prev.map((e) => [e.id, e]));
  let identical = prev.length === next.length;
  const merged = next.map((e, i) => {
    const kept = byId.get(e.id) ?? e;
    if (identical && kept !== prev[i]) identical = false;
    return kept;
  });
  return identical ? prev : merged;
}
