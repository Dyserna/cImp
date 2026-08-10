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
  /// surface-only detection flag).
  kind: 'graph' | 'offload' | 'audit' | 'mcp' | 'injection_flag';
  /// Canonicalized project root the call ran against ('' when unknown).
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

/// Server-side narrowing of the feed. Mirror of Rust
/// `activity::ActivityFilter`: every field is optional and an unset field
/// means "don't constrain this axis".
///
/// Note what it CANNOT express: `tab` is a real tab id (matched with
/// `Attribution::is_tab_named` semantics, so an `unrecognized` row never
/// matches), and there is no way to ask for the headless / unattributed /
/// unrecognized states. See `FeedFilter` below for why the Events tab
/// therefore narrows on the client.
export interface ActivityFilter {
  kind?: string | null;
  source?: string | null;
  tab?: string | null;
}

/// The feed (graph + offload), newest first, payload-free. Pass `sinceTs` to
/// fetch only entries newer than a high-water mark; omit it for the full
/// list (the Tool Activity tab polls the full list — it needs an
/// authoritative snapshot to reflect deletions). Pass `filter` to have the
/// backend narrow the snapshot before serializing it.
export function activityList(
  sinceTs?: number,
  filter?: ActivityFilter,
): Promise<ActivityEntry[]> {
  return invoke<ActivityEntry[]>('activity_list', {
    sinceTs: sinceTs ?? null,
    filter: filter ?? null,
  });
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
// CLIENT-SIDE, deliberately — even though `activity_list` now takes an
// `ActivityFilter`. Two reasons, both of which have to be fixed backend-side
// before the switch is worth making:
//
//  1. The wire filter's `tab` is a real tab id only. Three of the four
//     attribution states (headless / unattributed / unrecognized) have no wire
//     representation, and those are precisely the selections this view exists
//     to offer — "which rows had no tab at all" is the question, not a corner
//     case.
//  2. The kind / source / tab OPTION lists are derived from the feed, so they
//     must come from an UNFILTERED read. A pre-narrowed poll can only ever
//     offer back the option already selected, which silently strands the user
//     on one filter.
//
// So the poll fetches everything and narrows here. All of the filter logic is
// in the pure functions below, so the switch stays local once (1) and (2) are
// solved: map a `FeedFilter` to an `ActivityFilter`, pass it to `activityList`,
// and drop the `filterEntries` call in the view — the predicates stay for the
// tests and for whatever still has to be narrowed on this side.

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
/// distinct from the wire-level `ActivityFilter` above: every axis is always
/// set (a `<select>` always has a value) and `FILTER_ANY` is how "don't
/// constrain it" is spelled.
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
