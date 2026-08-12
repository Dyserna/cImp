// Tiny shared display formatters used by more than one view. Pure string
// helpers only — token/count arithmetic lives in `usageMath.ts`.

/// Wall-clock time for an epoch-ms timestamp, '—' when unset (0). Always the
/// 24-hour cycle ('h23': midnight is 00, never 24), whatever the locale's
/// default — a deliberate app-wide choice, not an oversight.
export function fmtTime(ms: number): string {
  return ms ? new Date(ms).toLocaleTimeString([], { hourCycle: 'h23' }) : '—';
}

/// Locale calendar date for an epoch-ms timestamp, '—' when unset (0).
export function fmtDate(ms: number): string {
  return ms ? new Date(ms).toLocaleDateString() : '—';
}
