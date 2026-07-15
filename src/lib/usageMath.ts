// V14 Phase D: pure math helpers for the Usage section's stacked-bar chart
// and cache-hit-ratio readout. Factored out of `CodeIntelligenceView.svelte`
// so the normalization/ratio arithmetic is unit-testable without mounting
// the component (see `usageMath.test.ts`).

import type { TurnUsage } from './graph';

/// The subset of `TurnUsage` the chart math actually needs — lets callers
/// (and tests) pass plain object literals without the transcript-only
/// fields (`msg_id`/`model`/`ts_ms`). V16 Feature 8 added `cache_make`
/// (cache-write) as its own bar segment, so it joined the subset.
export type TurnTokens = Pick<TurnUsage, 'in_tok' | 'cache_read' | 'cache_make' | 'out_tok' | 'tool_chars'>;

/// One turn's total token footprint: input + cache-read + cache-write +
/// output tokens (exact, from the transcript's `usage` block) plus the
/// estimated tool tokens (`tool_chars / 4`, rounded). This is what the
/// stacked bar's height is normalized against — NOT just `in_tok +
/// out_tok`, since a tool-heavy turn can be dominated by tool-result chars.
export function turnTotal(t: TurnTokens): number {
  return t.in_tok + t.cache_read + t.cache_make + t.out_tok + Math.round(t.tool_chars / 4);
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

/// `cache_read / (cache_read + in_tok)` — the honest cache-hit ratio shown
/// as a percentage in the Sessions table. `0` when there's no denominator
/// (no turns recorded yet), matching the backend's
/// `SessionUsageRow.cache_hit_ratio` computation exactly (see
/// `GraphIndex::usage_all_sessions`).
export function cacheHitRatio(cacheRead: number, inTok: number): number {
  const denom = cacheRead + inTok;
  return denom > 0 ? cacheRead / denom : 0;
}

/// The four $/MTok rates the session-cost popup multiplies against a
/// session's `UsageTotals`. Structural subset of the settings-side
/// `LlmPricingModel` (which additionally carries `provider`/`model`), so a
/// pricing row can be passed directly.
export interface PriceRates {
  input: number;
  cache_write: number;
  cache_read: number;
  output: number;
}

/// The token counts a cost is computed from — field names match
/// `UsageTotals` (`graph.ts`) so a `SessionUsageRow.totals` passes directly.
export interface CostTokens {
  in_tok: number;
  out_tok: number;
  cache_read: number;
  cache_make: number;
}

/// tokens × ($ per million tokens) → dollars. Guards non-finite inputs to 0
/// so a half-typed custom price field ("", ".", NaN) renders as $0 rather
/// than NaN across the whole table.
export function costUsd(tokens: number, perMTok: number): number {
  if (!Number.isFinite(tokens) || !Number.isFinite(perMTok)) return 0;
  return (tokens / 1_000_000) * perMTok;
}

/// Per-category dollar cost of a session plus the grand total — the popup's
/// third table row and the line under it. Category mapping: `in_tok` bills
/// at the input rate, `cache_make` (cache-creation) at cache_write,
/// `cache_read` at cache_read, `out_tok` at output.
export function sessionCost(
  totals: CostTokens,
  rates: PriceRates,
): { input: number; cache_write: number; cache_read: number; output: number; total: number } {
  const input = costUsd(totals.in_tok, rates.input);
  const cache_write = costUsd(totals.cache_make, rates.cache_write);
  const cache_read = costUsd(totals.cache_read, rates.cache_read);
  const output = costUsd(totals.out_tok, rates.output);
  return { input, cache_write, cache_read, output, total: input + cache_write + cache_read + output };
}

/// V16 Feature 8: the pricing-row fields the auto-matcher needs — a
/// structural subset of the settings-side `LlmPricingModel`.
export interface PricingRow extends PriceRates {
  model_prefix: string;
}

/// V16 Feature 8: pick the pricing row whose `model_prefix` prefixes the
/// transcript model id (e.g. prefix `"claude-opus-4-8"` matches the bare
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
  t: Pick<TurnUsage, 'in_tok' | 'cache_read' | 'cache_make' | 'out_tok' | 'tool_chars'>,
  rates: PriceRates,
): { input: number; cache_read: number; cache_write: number; output: number; tool: number; total: number } {
  const input = costUsd(t.in_tok, rates.input);
  const cache_read = costUsd(t.cache_read, rates.cache_read);
  const cache_write = costUsd(t.cache_make, rates.cache_write);
  const output = costUsd(t.out_tok, rates.output);
  const tool = costUsd(Math.round(t.tool_chars / 4), rates.input);
  return { input, cache_read, cache_write, output, tool, total: input + cache_read + cache_write + output + tool };
}

/// V24 Phase C: one merged run of same-origin turns in the S/A lane under the
/// chart. `count` = how many contiguous turns it spans (drives the flex width);
/// `label` is the single-letter badge (`S` main session / `A` sub-agent).
export interface LaneSegment {
  origin: 'session' | 'agent';
  label: 'S' | 'A';
  count: number;
}

/// V24 Phase C: collapse a turn series into contiguous same-origin runs for the
/// lane. A single-origin session yields exactly one segment; alternating
/// origins yield one segment per switch. Pure so the segmentation is
/// unit-testable without mounting the chart. Order matches `turns`.
export function laneSegments(turns: readonly { origin: 'session' | 'agent' }[]): LaneSegment[] {
  const segs: LaneSegment[] = [];
  for (const t of turns) {
    const last = segs[segs.length - 1];
    if (last && last.origin === t.origin) {
      last.count += 1;
    } else {
      segs.push({ origin: t.origin, label: t.origin === 'agent' ? 'A' : 'S', count: 1 });
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

/// V24 Phase C: the copy-pasteable resume command for a session — `opencode
/// -s <id>` for OpenCode sessions, `claude --resume <id>` for everything else
/// (Claude and the default). Shown as the selected card's hint line.
export function resumeCommand(agent: string, sessionId: string): string {
  return agent === 'opencode' ? `opencode -s ${sessionId}` : `claude --resume ${sessionId}`;
}

/// V24 Phase C: the extra class on a chart bar for its turn's origin — the
/// accent-outline + desaturation treatment applies only to sub-agent turns.
export function agentBarClass(origin: 'session' | 'agent'): '' | 'agent' {
  return origin === 'agent' ? 'agent' : '';
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

/// The Cost card's grand total — the sum of every model row's
/// `sessionCost(...).total` at that row's chosen rates. `rates(i)` supplies
/// the i-th model's selected rates. Pure so the footer figure is testable
/// without the component.
export function costGrandTotal(
  perModel: readonly { totals: CostTokens }[],
  rates: (i: number) => PriceRates,
): number {
  return perModel.reduce((sum, m, i) => sum + sessionCost(m.totals, rates(i)).total, 0);
}

/// A Cost-card row's secondary S/A share line — the origin split formatted
/// with `fmtTok`, e.g. `"session 12.3k · agents 4.1k tok"`. Both sides are
/// always shown (a zero reads as `"agents 0"`), so a session with no agent
/// fan-out still reads honestly.
export function originShareLine(origins: { session_tok: number; agent_tok: number }): string {
  return `session ${fmtTok(origins.session_tok)} · agents ${fmtTok(origins.agent_tok)} tok`;
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
