// V14 Phase D: pure math helpers for the Usage section's stacked-bar chart
// and cache-hit-ratio readout. Factored out of `CodeIntelligenceView.svelte`
// so the normalization/ratio arithmetic is unit-testable without mounting
// the component (see `usageMath.test.ts`).

import type { TokenKinds, TurnUsage } from './graph';

/// The subset of `TurnUsage` the chart math actually needs — lets callers
/// (and tests) pass plain object literals without the transcript-only
/// fields (`msg_id`/`model`/`ts_ms`).
///
/// **V40 Phase G (locked decision 19).** It was four fixed token fields; it is
/// the declared-category map now, so a harness that bills three categories (or
/// six) needs no change here and a category nobody reported has no key rather
/// than a zero.
export type TurnTokens = Pick<TurnUsage, 'tokens' | 'tool_chars'>;

/// Every reported category summed. Absent keys contribute nothing — which is
/// the same arithmetic as a zero, and the only place where treating absence as
/// zero is honest: a category nobody billed adds nothing to a total either way.
/// Non-finite values are skipped rather than poisoning the sum with NaN.
export function kindsTotal(t: TokenKinds): number {
  let sum = 0;
  for (const v of Object.values(t)) if (Number.isFinite(v)) sum += v;
  return sum;
}

/// One turn's total token footprint: every reported category (exact, from the
/// transcript's `usage` block) plus the estimated tool tokens
/// (`tool_chars / 4`, rounded). This is what the stacked bar's height is
/// normalized against — NOT just input + output, since a tool-heavy turn can be
/// dominated by tool-result chars.
export function turnTotal(t: TurnTokens): number {
  return kindsTotal(t.tokens) + Math.round(t.tool_chars / 4);
}

/// The tallest turn's total in `turns` — the stacked-bar chart's
/// normalization denominator. `1` (never `0`) so a chart with only
/// zero-token turns doesn't divide by zero; every bar then correctly
/// renders at 0% height rather than NaN%.
export function maxTurnTotal(turns: readonly TurnTokens[]): number {
  return Math.max(1, ...turns.map(turnTotal));
}

/// A turn's bar height as a percentage (0–100) of `max` (see
/// `maxTurnTotal`). Clamped to 100 so a stale/inconsistent `max` (e.g. a
/// caller-supplied constant smaller than this turn's own total) can never
/// overflow the chart.
export function barHeightPct(total: number, max: number): number {
  if (max <= 0) return 0;
  return Math.min(100, Math.max(0, (total / max) * 100));
}

/// The pricing category ids cImp's price table has rates for. These are
/// **cImp's own provider vocabulary**, not a harness's: they are literally
/// `PriceRates`' field names, and the backend's read boundary maps the four
/// stored `usage_stat` columns onto them (see `graph/index/usage.rs`). What a HARNESS
/// declares is which of them it reports.
export const KIND_INPUT = 'input';
export const KIND_CACHE_WRITE = 'cache_write';
export const KIND_CACHE_READ = 'cache_read';
export const KIND_OUTPUT = 'output';

/// `cache_read / (cache_read + input)` — the honest cache-hit ratio shown as a
/// percentage in the Sessions table.
///
/// **`null` when there is no ratio to show**: either the session's harness
/// declares neither cache-read nor input (nothing to divide) or both are zero
/// (no denominator). V40 Phase G made this `null` rather than `0`, because "0%
/// cache hit" is a claim about a session that spent tokens, and a harness that
/// does not bill cache reads never made it. The backend's
/// `SessionUsageRow.cache_hit_ratio` still returns `0.0` for the no-denominator
/// case — it is an `f64` on a struct that cannot carry absence, and this is the
/// consumer that renders it.
export function cacheHitRatio(totals: TokenKinds): number | null {
  const cacheRead = totals[KIND_CACHE_READ];
  const input = totals[KIND_INPUT];
  if (cacheRead === undefined && input === undefined) return null;
  const denom = (cacheRead ?? 0) + (input ?? 0);
  return denom > 0 ? (cacheRead ?? 0) / denom : null;
}

/// The four $/MTok rates the session-cost popup multiplies against a
/// session's totals. Structural subset of the settings-side `LlmPricingModel`
/// (which additionally carries `provider`/`model`), so a pricing row can be
/// passed directly.
///
/// Its four field names ARE the four pricing category ids a `TokenKinds` is
/// keyed by, which is what makes `sessionCost` a direct lookup rather than a
/// hand-written mapping table (V40 Phase G).
export interface PriceRates {
  input: number;
  cache_write: number;
  cache_read: number;
  output: number;
}

/// tokens × ($ per million tokens) → dollars. Guards non-finite inputs to 0
/// so a half-typed custom price field ("", ".", NaN) renders as $0 rather
/// than NaN across the whole table.
export function costUsd(tokens: number, perMTok: number): number {
  if (!Number.isFinite(tokens) || !Number.isFinite(perMTok)) return 0;
  return (tokens / 1_000_000) * perMTok;
}

/// Per-category dollar cost of a session plus the grand total — the popup's
/// third table row and the line under it.
///
/// **A direct key lookup** since V40 Phase G: a `TokenKinds` is keyed by the
/// same four pricing ids `PriceRates` names its fields after, so the four-line
/// `in_tok → input, cache_make → cache_write, …` mapping this used to carry is
/// gone. An absent category costs $0, which is the one honest reading of
/// absence here — you cannot be billed for tokens nobody reported.
export function sessionCost(
  totals: TokenKinds,
  rates: PriceRates,
): { input: number; cache_write: number; cache_read: number; output: number; total: number } {
  const at = (k: keyof PriceRates): number => costUsd(totals[k] ?? 0, rates[k]);
  const input = at('input');
  const cache_write = at('cache_write');
  const cache_read = at('cache_read');
  const output = at('output');
  return { input, cache_write, cache_read, output, total: input + cache_write + cache_read + output };
}

/// V16 Feature 8: the pricing-row fields the auto-matcher needs — a
/// structural subset of the settings-side `LlmPricingModel`.
export interface PricingRow extends PriceRates {
  model_prefix: string;
}

/// V16 Feature 8: pick the pricing row whose `model_prefix` prefixes the
/// transcript model id (a vendor's model-id prefix matches the bare
/// alias and any dated snapshot). Longest prefix wins; rows with an empty
/// prefix never auto-match (manual-pick only). `null` when nothing matches —
/// the caller renders token-mode with a hint rather than a made-up cost.
export function matchPricing<T extends PricingRow>(model: string | null | undefined, rows: readonly T[]): T | null {
  // Same scan as `matchPricingIndex` (below) — kept as a single source of
  // truth so the row form and index form can never disagree.
  const i = matchPricingIndex(model, rows);
  return i >= 0 ? rows[i] : null;
}

/// V16 Feature 8: one turn's estimated dollar cost per bar segment (input /
/// cache-read / cache-write / output / est. tool). Tool-result chars have no
/// exact token count; they bill at the input rate on the same chars/4
/// estimate the token view uses. Values are dollars — the stacked bar's
/// flex-grow weights in cost mode.
export function turnCost(
  t: TurnTokens,
  rates: PriceRates,
): { input: number; cache_read: number; cache_write: number; output: number; tool: number; total: number } {
  const c = sessionCost(t.tokens, rates);
  const tool = costUsd(Math.round(t.tool_chars / 4), rates.input);
  return { ...c, tool, total: c.total + tool };
}

/// V24 Phase C: one merged run of same-lane turns in the lane strip under the
/// chart. `count` = how many contiguous turns it spans (drives the flex width);
/// `label` is the single-letter badge.
export interface LaneSegment {
  origin: string;
  label: string;
  count: number;
}

/// The single-letter badge for a lane: the first character of its DECLARED id,
/// uppercased.
///
/// V40 Phase G: this was a `origin === 'agent' ? 'A' : 'S'` ternary — two
/// letters for two hard-coded lanes. Deriving it from the id keeps `session` →
/// `S` and `agent` → `A` exactly as they render today while giving any third
/// lane a badge of its own instead of silently borrowing `S`.
export function laneLabel(origin: string): string {
  return (origin.charAt(0) || '?').toUpperCase();
}

/// V24 Phase C: collapse a turn series into contiguous same-lane runs for the
/// strip. A single-lane session yields exactly one segment; alternating lanes
/// yield one segment per switch. Pure so the segmentation is unit-testable
/// without mounting the chart. Order matches `turns`.
export function laneSegments(turns: readonly { origin: string }[]): LaneSegment[] {
  const segs: LaneSegment[] = [];
  for (const t of turns) {
    const last = segs[segs.length - 1];
    if (last && last.origin === t.origin) {
      last.count += 1;
    } else {
      segs.push({ origin: t.origin, label: laneLabel(t.origin), count: 1 });
    }
  }
  return segs;
}

/// V24 Phase C: minimum rendered width (px) for a lane segment to show its
/// `S`/`A` label inline (≈2 characters at the lane's 9px font); narrower
/// segments carry the label in their tooltip only.
export const LANE_LABEL_MIN_PX = 14;

/// V24 Phase C: does a lane segment spanning `count` of `totalTurns` render
/// wide enough (in a `laneWidthPx`-wide lane) to show its inline `S`/`A`
/// label? False before the lane has measured (`laneWidthPx === 0`), so the
/// tooltip carries the letter until layout settles. Segment width tracks the
/// bars: `laneWidthPx * count / totalTurns`.
export function laneLabelVisible(count: number, totalTurns: number, laneWidthPx: number): boolean {
  if (totalTurns <= 0 || laneWidthPx <= 0) return false;
  return (laneWidthPx * count) / totalTurns >= LANE_LABEL_MIN_PX;
}

/// V24 Phase C: the extra class on a chart bar for its turn's lane — the
/// accent-outline + desaturation treatment applies only to fan-out turns.
///
/// V40 Phase G: which lanes those are is the harness's DECLARATION
/// (`harness_usage`'s `origins[].subagent`), passed in, rather than the literal
/// `origin === 'agent'` this used to test. `subagentOrigins` empty ⇒ no bar is
/// marked, which is the fail-quiet direction: before the declaration arrives
/// the chart is un-annotated rather than annotated by guess.
export function agentBarClass(origin: string, subagentOrigins: readonly string[]): '' | 'agent' {
  return subagentOrigins.includes(origin) ? 'agent' : '';
}

/// V24 Phase E: a Sessions-list row's two independent visual states.
/// `active` = the session is live right now (in `usage.active_session_ids` —
/// open tabs ∪ recency); MANY rows can be active at once. `selected` = this is
/// the drilled-in session (Phase C). They coexist (a live session can also be
/// the one you clicked into), and the markup renders both markers together.
/// Pure so the state logic is unit-testable without mounting the list.
export function sessionRowState(
  sessionId: string,
  selectedId: string | null | undefined,
  activeIds: readonly string[] | null | undefined,
): { active: boolean; selected: boolean } {
  return {
    active: !!activeIds && activeIds.includes(sessionId),
    selected: sessionId === selectedId,
  };
}

/// Dollar formatter for the cost table: 2 decimals from $1 up, 4 below so
/// sub-cent costs (a short session at Haiku rates) don't collapse to
/// "$0.00". Non-finite guards to "$0.00" like `costUsd`.
export function fmtUsd(n: number): string {
  if (!Number.isFinite(n)) return '$0.00';
  return '$' + (Math.abs(n) >= 1 ? n.toFixed(2) : n.toFixed(4));
}

// ── V24 Phase D: Cost card — per-model what-if pricing ─────────────────
// The Cost card (replacing the old single-rate cost popup) prices each model
// in a session separately, each row picking its own rates from a select. The
// decision logic below is pure so the row math / pricing precedence is
// unit-testable without mounting the card.

/// The fixed all-zero "Free ($0)" rates — the always-listed option that lets
/// the user compare actual spend against "what if this ran for free". Its
/// `sessionCost` is $0 across the board by construction.
export const FREE_RATES: PriceRates = { input: 0, cache_write: 0, cache_read: 0, output: 0 };

/// Index of the pricing row that best auto-matches `model` (longest
/// `model_prefix` wins — same rule as `matchPricing`), or `-1` when nothing
/// matches. The index form the Cost card's per-row select needs to seed its
/// default option.
export function matchPricingIndex<T extends PricingRow>(
  model: string | null | undefined,
  rows: readonly T[],
): number {
  if (!model) return -1;
  let bestI = -1;
  let bestLen = -1;
  for (let i = 0; i < rows.length; i++) {
    const p = rows[i].model_prefix;
    if (!p || !model.startsWith(p)) continue;
    if (p.length > bestLen) {
      bestLen = p.length;
      bestI = i;
    }
  }
  return bestI;
}

/// Resolve a Cost-card row's chosen $/MTok rates from its select index.
/// `idx` in `[0, rows.length)` picks that table row; `rows.length` is the
/// Custom sentinel (hand-typed `custom` rates); `rows.length + 1` is Free
/// (all-zero). Any other / stale index (e.g. after the table shrank) falls
/// back to Free — a safe $0 rather than pricing at the wrong row.
export function resolveRates(
  idx: number,
  rows: readonly PriceRates[],
  custom: PriceRates,
): PriceRates {
  if (idx >= 0 && idx < rows.length) return rows[idx];
  if (idx === rows.length) return custom;
  return FREE_RATES;
}

// ── V24 Phase D: stable-identity pricing selection ─────────────────────
// The Cost card select stores only USER OVERRIDES, keyed by model id, with
// values that survive pricing-table edits — unlike a raw index into a table
// that is refetched on every card open. `undefined` (no override) means
// "follow the live auto-match against the CURRENT table". A per-model derived
// row-state map (`costRowState`) recomputes selIdx/rates/matchedRow against
// the current table on every render-relevant change, so a stale/vanished pick
// falls back to auto-match rather than silently pricing at the wrong row (or
// applying Free while the select shows the first option).

/// A pricing row carrying a stable identity (`provider` + `model`) on top of
/// the auto-match `model_prefix`. Structural subset of the settings-side
/// `LlmPricingModel`; lets the resolver key an override by something that
/// survives table reordering/edits (a positional index does not).
export interface IdentifiedPricingRow extends PricingRow {
  provider: string;
  model: string;
}

/// A pricing row's stable identity — `provider + ' ' + model`. Used as the
/// Cost-card override key so a user's row pick tracks the row across table
/// edits instead of pointing at whatever now sits at that index.
export function pricingRowKey(row: { provider: string; model: string }): string {
  return `${row.provider} ${row.model}`;
}

/// A Cost-card model row's user pricing override, stored keyed by model id.
/// `{ kind: 'row', key }` names a table row by its stable `pricingRowKey`;
/// `'custom'` / `'free'` are the two synthetic trailing options. Absence of an
/// override (undefined) = follow the auto-match against the current table.
export type CostOverride =
  | { kind: 'row'; key: string }
  | { kind: 'custom' }
  | { kind: 'free' };

/// A Cost-card row's resolved select state against the CURRENT pricing table:
/// the `<select>`'s concrete `selIdx`, the effective `rates`, and the
/// auto-`matchedRow` (for the provider label / auto-match hint, independent of
/// any override).
export interface CostRowState<T> {
  selIdx: number;
  rates: PriceRates;
  matchedRow: T | null;
}

/// The Cost-card select index for `model` given its stored `override` and the
/// current `rows`. Select layout: table rows fill `[0, rows.length)`, then
/// Custom (`rows.length`), then Free (`rows.length + 1`).
///  - no override → auto-match by longest `model_prefix`; Custom when nothing
///    matches (an explicit $0 the user opts into, never a made-up cost);
///  - `{ kind: 'row', key }` → that row's CURRENT index, or auto-match when the
///    key no longer exists (table edited) — never a wrong row, never a silent
///    Free;
///  - `{ kind: 'custom' }` / `{ kind: 'free' }` → the synthetic sentinels.
export function costSelIdx<T extends IdentifiedPricingRow>(
  model: string | null | undefined,
  override: CostOverride | undefined,
  rows: readonly T[],
): number {
  const auto = (): number => {
    const i = matchPricingIndex(model, rows);
    return i >= 0 ? i : rows.length; // no match → Custom sentinel
  };
  if (!override) return auto();
  if (override.kind === 'custom') return rows.length;
  if (override.kind === 'free') return rows.length + 1;
  const i = rows.findIndex((r) => pricingRowKey(r) === override.key);
  return i >= 0 ? i : auto(); // vanished key → auto-match, not Free
}

/// A Cost-card row's full resolved state — `selIdx` (via `costSelIdx`), the
/// effective `rates` (via `resolveRates`), and the auto-`matchedRow`. Pure so
/// the whole per-model pricing decision is unit-testable without the card.
export function costRowState<T extends IdentifiedPricingRow>(
  model: string | null | undefined,
  override: CostOverride | undefined,
  rows: readonly T[],
  custom: PriceRates,
): CostRowState<T> {
  const selIdx = costSelIdx(model, override, rows);
  return { selIdx, rates: resolveRates(selIdx, rows, custom), matchedRow: matchPricing(model, rows) };
}

/// Translate a Cost-card select's chosen index into the stable override to
/// store. A table-row pick is recorded by its stable key (survives table
/// edits); the two trailing options as their sentinels.
export function costOverrideForIdx<T extends IdentifiedPricingRow>(
  idx: number,
  rows: readonly T[],
): CostOverride {
  if (idx >= 0 && idx < rows.length) return { kind: 'row', key: pricingRowKey(rows[idx]) };
  if (idx === rows.length) return { kind: 'custom' };
  return { kind: 'free' };
}

/// Is a fetched `SessionUsageDetail.row` the backend's "empty" sentinel —
/// returned when the session vanished or the graph is off (empty `agent`,
/// zero `started_ms`)? A real row always carries both, so the card can use
/// this to refuse selected mode (which would render "Session ·  ·
/// 1970-01-01…") and stay live with a transient notice instead.
export function isEmptyDetailRow(row: { agent: string; started_ms: number }): boolean {
  return row.agent === '' && row.started_ms === 0;
}

/// Whether a session is actually live right now, per a `UsageSnapshot`'s
/// `active_session_ids` (open tab ∪ recency). A fresh empty session has
/// recorded nothing yet, so `usage.current` / `memory.current_session` still
/// point at the PREVIOUS session — the live card and the Working-set label say
/// "last session" then instead of claiming it's this one.
///
/// A free function rather than a closure over the snapshot because #130 put
/// the snapshot in `codeIntel/UsageOverview.svelte` while the Memory section,
/// which asks the same question of the same list, stayed in
/// `CodeIntelligenceView.svelte`. Two call sites, one predicate: absent ids
/// answer false, exactly as the `usage?.active_session_ids ?? []` spelling
/// they both replaced did.
export function isActiveSessionIn(
  ids: readonly string[] | undefined,
  sid?: string | null,
): boolean {
  return !!sid && (ids ?? []).includes(sid);
}

/// What the Overview poll should do with an arriving `graph_usage`
/// snapshot, given whether the PREVIOUS tick was already in the
/// store-error state. Pure so the "empty is not absent" rule is testable
/// without mounting `CodeIntelligenceView`.
export interface UsageApplyDecision {
  /// Assign the snapshot to `usage` (false ⇒ keep the last-good one).
  apply: boolean;
  /// The store-error flag to carry into the next tick.
  errored: boolean;
  /// Flash the transient notice NOW — true only on the TRANSITION into the
  /// error state, so a store that stays broken at the 2s cadence doesn't
  /// re-flash 30 times a minute.
  flash: boolean;
}

/// `snap === null` means the IPC call itself failed (already logged by the
/// caller): keep the last-good snapshot and leave the error-state machine
/// untouched — a transport hiccup is not the store-busy condition the
/// notice describes. `store_error != null` means the backend read the store
/// and failed, so the payload is non-authoritative (its empty `sessions` is
/// an artifact, not data) — don't apply it, and flash on entry. Anything
/// else is authoritative and applies, INCLUDING a `store_error: null`
/// snapshot with zero sessions (a genuinely empty store must render as 0).
export function decideUsageApply(
  snap: { store_error?: string | null } | null,
  prevErrored: boolean,
): UsageApplyDecision {
  if (!snap) return { apply: false, errored: prevErrored, flash: false };
  if (snap.store_error != null) return { apply: false, errored: true, flash: !prevErrored };
  return { apply: true, errored: false, flash: false };
}

/// The Cost card's grand total — the sum of every model row's
/// `sessionCost(...).total` at that row's chosen rates. `rates(i)` supplies
/// the i-th model's selected rates. Pure so the footer figure is testable
/// without the component.
export function costGrandTotal(
  perModel: readonly { totals: TokenKinds }[],
  rates: (i: number) => PriceRates,
): number {
  return perModel.reduce((sum, m, i) => sum + sessionCost(m.totals, rates(i)).total, 0);
}

/// A Cost-card row's secondary lane-share line — the per-lane split formatted
/// with `fmtTok`, e.g. `"main session 12.3k · sub-agents 4.1k tok"`.
///
/// `lanes` is the harness's DECLARED origins in declared order; every declared
/// lane is shown even at 0 (a session with no fan-out still reads honestly, and
/// the harness declaring the lane is what makes its 0 a real statement). A lane
/// with no `label` falls back to its id, never to another harness's wording.
///
/// V40 Phase F named the lanes from the declaration; **V40 Phase G removed the
/// two-lane SHAPE** — `origins` was `OriginSplit { session_tok, agent_tok }`,
/// so a one-lane or three-lane harness could not be printed at all.
export function originShareLine(
  origins: Record<string, number>,
  lanes: readonly { id: string; label?: string }[],
): string {
  if (lanes.length === 0) return '';
  return (
    lanes.map((l) => `${l.label ?? l.id} ${fmtTok(origins[l.id] ?? 0)}`).join(' · ') + ' tok'
  );
}

// ── V28: Overview dashboard donuts ─────────────────────────────────────
// The dashboard card's two donuts: session-vs-agent tokens (outer ring)
// nested over the per-origin kind breakdown (inner ring), and per-model
// cost share. The aggregation + ring geometry is pure so it's testable
// without mounting the SVG.

/// Per-lane sums of the EXACT token categories across a turn series — the
/// token donut's source. Tool-result chars are excluded on purpose: the donut
/// shows transcript-exact tokens only; the chars/4 tool estimate stays a
/// stacked-bar concern (it overlaps the next turn's input anyway).
///
/// V40 Phase G: keyed by declared lane id rather than a fixed
/// `{ session, agent }` pair. `lanes` seeds the declared lanes so one with no
/// turns still gets a (zero-valued, empty-map) entry and keeps its legend row;
/// a lane that appears only in the DATA is added as it is met, so a stored row
/// is never silently dropped because the declaration has not arrived yet.
export type OriginKinds = Record<string, TokenKinds>;

export function originKindTotals(
  turns: readonly Pick<TurnUsage, 'tokens' | 'origin'>[],
  lanes: readonly string[] = [],
): OriginKinds {
  const out: OriginKinds = {};
  for (const id of lanes) out[id] = {};
  for (const t of turns) {
    const bucket = (out[t.origin] ??= {});
    for (const [k, v] of Object.entries(t.tokens)) {
      if (Number.isFinite(v)) bucket[k] = (bucket[k] ?? 0) + v;
    }
  }
  return out;
}

/// One drawn donut-ring segment. `a0`/`a1` are the DRAWN angles (radians,
/// 0 = 12 o'clock, increasing clockwise) — the inter-segment gap inset is
/// already applied, while `share` stays the true proportion.
export interface DonutArc {
  key: string;
  value: number;
  share: number;
  a0: number;
  a1: number;
}

/// Lay out one donut ring: proportional spans from 12 o'clock clockwise.
/// Zero/negative/non-finite values are dropped (they'd draw nothing); each
/// kept segment is inset by up to `gapAngle / 2` per side — clamped to 30%
/// of its own span so a sliver never inverts — which keeps segment
/// BOUNDARIES at their exact cumulative angles (nested rings built from
/// consistent data therefore stay aligned). A single nonzero value takes
/// the full circle with no gap. Empty when the total is 0 — the caller
/// renders a placeholder, never a fabricated ring.
export function donutArcs(
  items: readonly { key: string; value: number }[],
  gapAngle: number,
): DonutArc[] {
  const kept = items.filter((x) => Number.isFinite(x.value) && x.value > 0);
  const total = kept.reduce((s, x) => s + x.value, 0);
  if (total <= 0) return [];
  const solo = kept.length === 1;
  const out: DonutArc[] = [];
  let acc = 0;
  for (const x of kept) {
    const span = (x.value / total) * 2 * Math.PI;
    const pad = solo ? 0 : Math.min(gapAngle / 2, span * 0.3);
    out.push({
      key: x.key,
      value: x.value,
      share: x.value / total,
      a0: acc + pad,
      a1: acc + span - pad,
    });
    acc += span;
  }
  return out;
}

/// SVG path for an annular sector between radii `rIn` < `rOut`, from angle
/// `a0` to `a1` (radians, 0 = 12 o'clock, clockwise, center `cx`,`cy`).
/// A span within ε of the full circle renders as a complete ring (outer
/// circle clockwise + inner counter-clockwise, nonzero winding) — a plain
/// arc command degenerates when its endpoints coincide.
export function arcPath(
  cx: number,
  cy: number,
  rOut: number,
  rIn: number,
  a0: number,
  a1: number,
): string {
  // toFixed can emit "-0.000" at the axis crossings (cos/sin epsilon) —
  // normalize so paths compare cleanly.
  const fx = (v: number): string => {
    const s = v.toFixed(3);
    return s === '-0.000' ? '0.000' : s;
  };
  const px = (r: number, a: number): string => `${fx(cx + r * Math.sin(a))} ${fx(cy - r * Math.cos(a))}`;
  const span = a1 - a0;
  if (span <= 0) return '';
  if (span >= 2 * Math.PI - 1e-4) {
    return (
      `M ${px(rOut, 0)} A ${rOut} ${rOut} 0 1 1 ${px(rOut, Math.PI)} ` +
      `A ${rOut} ${rOut} 0 1 1 ${px(rOut, 0)} Z ` +
      `M ${px(rIn, 0)} A ${rIn} ${rIn} 0 1 0 ${px(rIn, Math.PI)} ` +
      `A ${rIn} ${rIn} 0 1 0 ${px(rIn, 0)} Z`
    );
  }
  const large = span > Math.PI ? 1 : 0;
  return (
    `M ${px(rOut, a0)} A ${rOut} ${rOut} 0 ${large} 1 ${px(rOut, a1)} ` +
    `L ${px(rIn, a1)} A ${rIn} ${rIn} 0 ${large} 0 ${px(rIn, a0)} Z`
  );
}

/// Share-of-ring percentage for donut legends/tooltips: whole percents,
/// with the honest edges — a nonzero sliver reads "<1%", a dominant-but-
/// not-total share reads ">99%", and only a true 0/1 reads "0%"/"100%".
export function fmtPct(share: number): string {
  if (!Number.isFinite(share) || share <= 0) return '0%';
  if (share >= 1) return '100%';
  const pct = share * 100;
  if (pct < 1) return '<1%';
  if (pct > 99) return '>99%';
  return `${Math.round(pct)}%`;
}

/// Compact token-count formatter for the Sessions table's per-row billing
/// stats, where four multi-million counts share one line ("61.2M", "9.9k",
/// "412"). One decimal below 10k, integer k below 1M (the 999,500 boundary
/// keeps "1000k" from ever appearing), two decimals below 10M, one above.
/// Exact values belong in the row's tooltip, not here.
export function fmtTok(n: number): string {
  if (!Number.isFinite(n) || n <= 0) return '0';
  // Round BEFORE the branch test: a fractional n in [999.5, 1000) would
  // otherwise round up inside the small-count branch and print a bare
  // "1000" (no suffix) — inconsistent with fmtTok(1000) === "1.0k".
  // Current callers pass integers, but the utility shouldn't rely on it.
  const rounded = Math.round(n);
  if (rounded < 1000) return String(rounded);
  if (n < 999_500) {
    const k = n / 1000;
    return (k < 10 ? k.toFixed(1) : String(Math.round(k))) + 'k';
  }
  const m = n / 1_000_000;
  return (m < 10 ? m.toFixed(2) : m.toFixed(1)) + 'M';
}
