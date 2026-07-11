// V14 Phase D: pure math helpers for the Usage section's stacked-bar chart
// and cache-hit-ratio readout. Factored out of `CodeIntelligenceView.svelte`
// so the normalization/ratio arithmetic is unit-testable without mounting
// the component (see `usageMath.test.ts`).

import type { TurnUsage } from './graph';

/// The subset of `TurnUsage` the chart math actually needs — lets callers
/// (and tests) pass plain object literals without the transcript-only
/// fields (`msg_id`/`model`/`cache_make`/`ts_ms`).
export type TurnTokens = Pick<TurnUsage, 'in_tok' | 'cache_read' | 'out_tok' | 'tool_chars'>;

/// One turn's total token footprint: input + cache-read + output tokens
/// (exact, from the transcript's `usage` block) plus the estimated tool
/// tokens (`tool_chars / 4`, rounded). This is what the stacked bar's
/// height is normalized against — NOT just `in_tok + out_tok`, since a
/// tool-heavy turn can be dominated by tool-result chars.
export function turnTotal(t: TurnTokens): number {
  return t.in_tok + t.cache_read + t.out_tok + Math.round(t.tool_chars / 4);
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

/// Dollar formatter for the cost table: 2 decimals from $1 up, 4 below so
/// sub-cent costs (a short session at Haiku rates) don't collapse to
/// "$0.00". Non-finite guards to "$0.00" like `costUsd`.
export function fmtUsd(n: number): string {
  if (!Number.isFinite(n)) return '$0.00';
  return '$' + (Math.abs(n) >= 1 ? n.toFixed(2) : n.toFixed(4));
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
