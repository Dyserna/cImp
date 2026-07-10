// Tiny shared display formatters used by more than one view. Pure string
// helpers only — token/count arithmetic lives in `usageMath.ts`.

/// Locale wall-clock time for an epoch-ms timestamp, '—' when unset (0).
export function fmtTime(ms: number): string {
  return ms ? new Date(ms).toLocaleTimeString() : '—';
}
