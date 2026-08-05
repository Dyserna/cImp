// NC-3: pure helpers for the bottom-bar usage widget's live context group.
// Factored out of `UsageMeter.svelte` (same convention as `usageMath.ts`) so
// the absence rules and the ratio arithmetic are unit-testable without
// mounting the component — see `contextMeter.test.ts`.
//
// The governing rule everywhere below: **absent is not zero**. Each field of
// the status-line push is independently optional (`rate_limits` exists only
// for subscription auth after the first API response; the `context_window`
// block only on a new enough Claude Code; individual fields inside either can
// be missing). Every helper therefore returns `null` for "not reported" and
// leaves the rendering of that to the caller, which shows "—" / a hollow
// track rather than a confident 0%.

import type { ContextSnapshot, UsageSnapshot } from '../ipc';

/// Compact a token count the way the terminal status line does: `940`, `12k`,
/// `200k`, `1.0M`. Null/non-finite in → `'?'` out, so a formatted figure never
/// implies a zero it wasn't told.
export function humanizeTokens(n: number | null | undefined): string {
  if (n == null || !Number.isFinite(n)) return '?';
  if (n < 1000) return String(Math.round(n));
  // Above 999_500 the rounded thousands would render a nonsensical '1000k'.
  if (n < 999_500) return `${Math.round(n / 1000)}k`;
  return `${(n / 1_000_000).toFixed(1)}M`;
}

/// True when the context block carries at least one *number* to draw.
/// Metadata alone (a session name, an effort string) has nothing to render,
/// so it does not light the group up. Mirrors the backend's
/// `ContextSnapshot::is_substantive`, which decides whether the push is worth
/// writing at all.
export function hasContextData(ctx: ContextSnapshot | null | undefined): boolean {
  if (!ctx) return false;
  return (
    ctx.used_percentage != null ||
    ctx.total_input_tokens != null ||
    ctx.context_window_size != null ||
    ctx.cache_read_tokens != null ||
    ctx.cache_creation_tokens != null
  );
}

/// True when the snapshot carries at least one quota window. Either window is
/// independently absent-able, and both are absent under API-key auth — the
/// quota columns are then dropped rather than drawn as placeholders.
export function hasQuotaData(snap: UsageSnapshot | null | undefined): boolean {
  return !!(snap?.five_hour || snap?.seven_day);
}

/// Share (0–100) of the latest turn's input tokens that were served from the
/// prompt cache: `read / (read + cache-creation + uncached input)`.
///
/// Deliberately a *different* denominator from `usageMath.cacheHitRatio`
/// (`read / (read + in_tok)`), which mirrors the graph/transcript path's
/// historical per-session figure: cache-*creation* tokens are input tokens
/// that were not served from cache, so including them keeps this live figure
/// from overstating the hit rate. The two are labelled differently in the UI
/// for that reason; do not "unify" them without changing both labels.
///
/// `null` (→ "—") when the read figure is missing or nothing was sent at all,
/// so an idle turn never reads as a genuine 0% hit rate.
export function cacheHitPct(ctx: ContextSnapshot | null | undefined): number | null {
  const read = ctx?.cache_read_tokens;
  if (read == null || !Number.isFinite(read)) return null;
  const denom = read + (ctx?.cache_creation_tokens ?? 0) + (ctx?.input_tokens ?? 0);
  if (!(denom > 0)) return null;
  return (read / denom) * 100;
}

/// "used/size" for the context row (`25k/200k`). `null` when neither figure
/// was reported — the caller shows "—" rather than "?/?".
export function contextTokensLabel(ctx: ContextSnapshot | null | undefined): string | null {
  if (ctx?.total_input_tokens == null && ctx?.context_window_size == null) return null;
  return `${humanizeTokens(ctx?.total_input_tokens)}/${humanizeTokens(ctx?.context_window_size)}`;
}

/// "read 20k · new 5k" for the cache row. `null` when neither half of the
/// split was reported.
export function cacheSplitLabel(ctx: ContextSnapshot | null | undefined): string | null {
  if (ctx?.cache_read_tokens == null && ctx?.cache_creation_tokens == null) return null;
  return `read ${humanizeTokens(ctx?.cache_read_tokens)} · new ${humanizeTokens(
    ctx?.cache_creation_tokens,
  )}`;
}

/// Tooltip for the context group. The session metadata that rides the push
/// (`session_name`, `agent.name`, `effort`, `thinking`, `fast_mode`) is
/// surfaced here rather than as more bottom-bar text — it also tells the user
/// *which* Claude tab's session the numbers belong to, since both Claude tabs
/// push to the same file and the freshest write wins.
export function contextTitle(ctx: ContextSnapshot | null | undefined): string {
  const bits = ['Live context window (from the Claude tab status line)'];
  if (ctx?.session_name) bits.push(`session: ${ctx.session_name}`);
  if (ctx?.agent_name) bits.push(`agent: ${ctx.agent_name}`);
  if (ctx?.effort) bits.push(`effort: ${ctx.effort}`);
  if (ctx?.thinking) bits.push(`thinking: ${ctx.thinking}`);
  if (ctx?.fast_mode != null) bits.push(`fast mode: ${ctx.fast_mode ? 'on' : 'off'}`);
  return bits.join('\n');
}

/// A percentage clamped into the 0–100 a bar can render. Only ever called
/// with a reported value; absence is handled by the caller (hollow track).
export function clampPct(p: number): number {
  if (!Number.isFinite(p)) return 0;
  return Math.min(100, Math.max(0, p));
}
