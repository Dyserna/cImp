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

import type { UsageSnapshot } from '../ipc';

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

/// True when the snapshot carries at least one quota window. Either window is
/// independently absent-able, and both are absent under API-key auth — the
/// quota columns are then dropped rather than drawn as placeholders.
export function hasQuotaData(snap: UsageSnapshot | null | undefined): boolean {
  return !!(snap?.five_hour || snap?.seven_day);
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
