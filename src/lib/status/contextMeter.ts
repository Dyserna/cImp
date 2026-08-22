// Pure helpers for the bottom-bar usage widget. Factored out of
// `UsageMeter.svelte` (same convention as `usageMath.ts`) so the absence rules
// and the arithmetic are unit-testable without mounting the component — see
// `contextMeter.test.ts`.
//
// The file is named for the live context group it was introduced for (NC-3).
// That group was retired from the widget — the meter now ends at the reset
// clock — so the context-specific helpers (`hasContextData`, `cacheHitPct`,
// `contextUsedPct`, `contextTokensLabel`, `cacheSplitLabel`, `contextTitle`,
// `contextAttribution`) went with it, together with the `usage.show_context`
// setting that gated them. Nothing on the backend push path changed: the push
// file still carries the `context_window` block and the terminal status line
// still renders it.
//
// The governing rule for what remains: **absent is not zero**. Each field of
// the status-line push is independently optional (`rate_limits` exists only
// for subscription auth after the first API response, and individual fields
// inside it can be missing). A helper therefore returns `null` for "not
// reported" and leaves the rendering of that to the caller, which shows "—" /
// a hollow track rather than a confident 0%.

import {
  findHarness,
  findHarnessByCommand,
  harnessesWith,
  reservedAiTabIds,
  type HarnessInfo,
} from '../harness';
import type { UsageReading } from '../ipc';

/// Compact a token count the way the terminal status line does: `940`, `12k`,
/// `200k`, `1.0M`. Null/non-finite in → `'?'` out, so a formatted figure never
/// implies a zero it wasn't told.
///
/// Its widget callers were the retired context/cache figures; it is kept (and
/// kept under test) as the frontend's single mirror of the terminal renderer's
/// bucketing, for the next figure that needs it. Delete it, not re-derive it,
/// if that never comes.
export function humanizeTokens(n: number | null | undefined): string {
  if (n == null || !Number.isFinite(n)) return '?';
  if (n < 1000) return String(Math.round(n));
  // Above 999_500 the rounded thousands would render a nonsensical '1000k'.
  if (n < 999_500) return `${Math.round(n / 1000)}k`;
  return `${(n / 1_000_000).toFixed(1)}M`;
}

/// True when the reading carries at least one quota window.
///
/// The backend emits only the windows that HAVE a reading, so this is a length
/// check rather than a per-window one — a window with nothing to report is
/// absent from the list instead of present at 0. Under API-key auth the list is
/// empty (no `rate_limits` in the push at all) and the quota columns are
/// dropped rather than drawn as placeholders.
export function hasQuotaData(reading: UsageReading | null | undefined): boolean {
  return !!reading?.windows?.length;
}

/// A percentage clamped into the 0–100 a bar can render. Only ever called
/// with a reported value; absence is handled by the caller (hollow track).
/// The percentage *text* is clamped through this too, so a payload reporting
/// 143% can't print a number the bar beside it contradicts (the terminal
/// renderer clamps the same way — `harness/claude/statusline.rs`).
export function clampPct(p: number): number {
  if (!Number.isFinite(p)) return 0;
  return Math.min(100, Math.max(0, p));
}

// ---- who actually pushes -------------------------------------------------

/// A tab as far as the push question is concerned. Structural rather than
/// `TabConfig` so the check is unit-testable without building whole configs
/// (Preview tabs have no `command` at all, hence the optional field).
export interface PushCapableTab {
  kind: string;
  id: string;
  command?: string;
}

/// The harness id whose usage source the widget polls, or `null` when no tab
/// that can push one is running.
///
/// **V40 Phase F (locked decision 19).** This block used to be three identity
/// literals: a hand-written mirror of Rust's `command_is`, a reserved-id array,
/// and a function that answered one harness's id verbatim. All three are the
/// registry's now — the harness is found by its declared
/// `binaries`, the reserved ids come from its `tab_ids`, and *whether a running
/// tab pushes at all* is the declared `usage_push` feature rather than an
/// inference from the product's name.
///
/// The rules that survive, because they are about tabs and not about a harness:
///
/// * the tab's COMMAND decides, not its id — the status-line overlay is
///   injected per command, so a variant tab and any user-created tab running
///   the same binary push exactly like the reserved one does (M15). Gating on
///   `enabled_ai_tabs.includes(<reserved id>)` hid valid readings from every
///   one of them;
/// * a reserved id counts only while enabled; a user-created tab is always
///   present, because it has no enable checkbox to be off;
/// * registry order breaks a tie, so two pushing harnesses running at once give
///   the widget one stable answer instead of one that flips per poll.
///
/// `null` is a first-class answer the widget renders as *nothing to poll* —
/// never as a harness sitting at 0%.
export function usagePushHarness(
  list: readonly HarnessInfo[],
  tabs: readonly PushCapableTab[] | null | undefined,
  enabledAiTabs: readonly string[] | null | undefined,
): string | null {
  if (!tabs) return null;
  const enabled = enabledAiTabs ?? [];
  const reserved = reservedAiTabIds(list);
  for (const h of harnessesWith(list, 'usage_push')) {
    const pushing = tabs.some(
      (t) =>
        t.kind === 'ai_tool' &&
        findHarnessByCommand(list, t.command)?.id === h.id &&
        (!reserved.includes(t.id) || enabled.includes(t.id)),
    );
    if (pushing) return h.id;
  }
  return null;
}

/// How many stacked rows the bottom strip must be tall enough for, given the
/// harness currently feeding it — the declared `statuslineRows` (locked
/// decision 19: the 44 px `.status-bar` height was two rows of one harness's
/// quota pair, asserted in a stylesheet). `0` means "this harness declares no
/// status-line widget", and the caller leaves the stylesheet's default alone.
export function statuslineRowsFor(
  list: readonly HarnessInfo[],
  harness: string | null | undefined,
): number {
  return findHarness(list, harness)?.affordances.statuslineRows ?? 0;
}
