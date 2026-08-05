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
/// so it does not light the group up.
///
/// Field-for-field mirror of the backend's `ContextSnapshot::is_substantive`
/// (`usage/mod.rs`), which decides whether the push is worth writing and
/// whether a merged file is worth returning at all — the two predicates
/// disagreeing means the widget either hides data the backend kept, or lights
/// an empty group up. Any field added there must be added here.
export function hasContextData(ctx: ContextSnapshot | null | undefined): boolean {
  if (!ctx) return false;
  return (
    ctx.used_percentage != null ||
    ctx.remaining_percentage != null ||
    ctx.total_input_tokens != null ||
    ctx.context_window_size != null ||
    ctx.cache_read_tokens != null ||
    ctx.cache_creation_tokens != null ||
    ctx.input_tokens != null ||
    ctx.output_tokens != null
  );
}

/// Percentage of the context window in use, for the bar and the "%" text.
///
/// `used_percentage` when Claude Code reported it; otherwise derived from
/// `remaining_percentage`, which upstream reports separately and which would
/// otherwise be a parsed, shipped field with no reader at all. Deriving is
/// safe in this one direction: the two are documented as complements of the
/// same window, so `100 − remaining` is that window's used share. `null` when
/// neither is reported — the caller draws a hollow "unknown" track.
export function contextUsedPct(ctx: ContextSnapshot | null | undefined): number | null {
  if (ctx?.used_percentage != null && Number.isFinite(ctx.used_percentage)) {
    return ctx.used_percentage;
  }
  if (ctx?.remaining_percentage != null && Number.isFinite(ctx.remaining_percentage)) {
    return 100 - ctx.remaining_percentage;
  }
  return null;
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
/// `null` (→ "—") when nothing was sent at all, so an idle turn never reads as
/// a genuine 0% hit rate — and, per this file's "absent is not zero" rule,
/// whenever the denominator is not *fully* reported.
///
/// All three terms are required (M16). They come from one `current_usage`
/// object, so in the documented payload they arrive together; when they don't
/// — the hoisted/reshaped block the backend deliberately tolerates
/// (`statusline/mod.rs`) — the missing term is unknown, not zero, and
/// substituting zero renders a confident figure that is wrong in the
/// flattering direction (a lone `cache_read_tokens` becomes a solid 100%).
/// "—" is the honest answer there.
export function cacheHitPct(ctx: ContextSnapshot | null | undefined): number | null {
  const read = ctx?.cache_read_tokens;
  const creation = ctx?.cache_creation_tokens;
  const input = ctx?.input_tokens;
  if (read == null || creation == null || input == null) return null;
  if (!Number.isFinite(read) || !Number.isFinite(creation) || !Number.isFinite(input)) return null;
  const denom = read + creation + input;
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

/// Tooltip for the context group: the full session metadata riding the push
/// (`session_name`, `agent.name`, `effort`, `thinking`, `fast_mode`). The
/// identity half of it is *also* rendered on screen — see
/// [`contextAttribution`] — because every Claude tab pushes into the same file
/// and only one of them owns the context slot at a time; a tooltip nobody
/// hovers is not an attribution.
export function contextTitle(ctx: ContextSnapshot | null | undefined): string {
  const bits = ['Live context window (from the Claude tab status line)'];
  if (ctx?.session_name) bits.push(`session: ${ctx.session_name}`);
  if (ctx?.agent_name) bits.push(`agent: ${ctx.agent_name}`);
  if (ctx?.effort) bits.push(`effort: ${ctx.effort}`);
  if (ctx?.thinking) bits.push(`thinking: ${ctx.thinking}`);
  if (ctx?.fast_mode != null) bits.push(`fast mode: ${ctx.fast_mode ? 'on' : 'off'}`);
  return bits.join('\n');
}

/// Short visible attribution for the context group: *whose* session the
/// numbers belong to. Several Claude tabs push into one file and only one of
/// them owns the context slot at a time (see `usage::merge_push`), so a
/// reading that looks fresh can still belong to the tab you are not working
/// in — the tooltip alone was not enough to notice that (M14). `null` when the
/// push carried no identity at all, in which case there is nothing to say.
export function contextAttribution(
  ctx: ContextSnapshot | null | undefined,
  maxLen = 18,
): string | null {
  const bits = [ctx?.session_name, ctx?.agent_name].filter(
    (b): b is string => typeof b === 'string' && b.trim().length > 0,
  );
  if (bits.length === 0) return null;
  const text = bits.map((b) => b.trim()).join(' · ');
  return text.length > maxLen ? `${text.slice(0, maxLen - 1)}…` : text;
}

/// A percentage clamped into the 0–100 a bar can render. Only ever called
/// with a reported value; absence is handled by the caller (hollow track).
/// The percentage *text* is clamped through this too, so a payload reporting
/// 143% can't print a number the bar beside it contradicts (the terminal
/// renderer clamps the same way — `statusline/mod.rs`).
export function clampPct(p: number): number {
  if (!Number.isFinite(p)) return 0;
  return Math.min(100, Math.max(0, p));
}

// ---- who actually pushes -------------------------------------------------

/// The reserved AI tab ids `enabled_ai_tabs` can gate. Any other AI-tool tab
/// is user-created and simply exists (it has no enable checkbox).
const RESERVED_AI_TAB_IDS = ['claude', 'claude-local', 'opencode'];

/// A tab as far as the push question is concerned. Structural rather than
/// `TabConfig` so the check is unit-testable without building whole configs
/// (Preview tabs have no `command` at all, hence the optional field).
export interface PushCapableTab {
  kind: string;
  id: string;
  command?: string;
}

/// Mirror of `tabs::config::command_is(command, "claude")`: match on the
/// path's file stem, case-insensitively, so `claude`, `C:\bin\claude.exe` and
/// `/usr/local/bin/claude.cmd` all count. Both separators are accepted
/// because Windows is the primary platform and a config written there can be
/// read anywhere.
export function commandIsClaude(command: string | undefined | null): boolean {
  if (!command) return false;
  const base = command.trim().replace(/[\\/]+$/, '').split(/[\\/]/).pop() ?? '';
  // `Path::file_stem` strips one trailing extension, and only when it isn't
  // the whole name (".claude" has no stem to speak of).
  const stem = base.replace(/(?!^)\.[^.]*$/, '');
  return stem.toLowerCase() === 'claude';
}

/// True when at least one AI tab that is actually running invokes `claude` —
/// i.e. when *something* can push a status-line reading into the usage file.
///
/// The statusline overlay is injected per tab by command (`command_is(…,
/// "claude")` in `tabs::config`), not by tab id, so `claude-local` and any
/// user-created claude-command tab push exactly like the subscription tab
/// does. Gating the widget on `enabled_ai_tabs.includes('claude')` (as it used
/// to) hid valid context readings from every one of them (M15).
///
/// Reserved ids only count while enabled; user-created tabs are always
/// present. Quota display needs no gate of its own: `rate_limits` exists only
/// under subscription auth, so a local/API-key tab simply pushes no quota.
export function claudePushTabActive(
  tabs: readonly PushCapableTab[] | null | undefined,
  enabledAiTabs: readonly string[] | null | undefined,
): boolean {
  if (!tabs) return false;
  const enabled = enabledAiTabs ?? [];
  return tabs.some(
    (t) =>
      t.kind === 'ai_tool' &&
      commandIsClaude(t.command) &&
      (!RESERVED_AI_TAB_IDS.includes(t.id) || enabled.includes(t.id)),
  );
}
