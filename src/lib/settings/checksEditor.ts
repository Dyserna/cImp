// V22 Phase E — pure decision logic for the ChecksEditor, extracted so it can
// be unit-tested without a Svelte/Tauri host (the frontend test infra is plain
// vitest, no component testing). The `.svelte` stays thin and delegates the
// conditional-field map, test-result classification, chip derivation, and the
// auto-flag clearing to the functions here.

import type { CheckDef, ChecksSuggestion, ChecksTestResult, ParserKind } from './types';

/// Every `ParserKind` wire name, in the order the editor's dropdown lists them
/// (mainstream first, then SARIF/long-tail, then the regex escape hatch and the
/// generic fallback). Mirror of the Rust `ParserKind` variants — kept aligned
/// with the `types.ts` union (which the Rust tripwire pins to the enum).
export const PARSER_KINDS: readonly ParserKind[] = [
  'cargo-json',
  'cargo-test',
  'tsc',
  'eslint-json',
  'jest-json',
  'pytest',
  'go',
  'go-test-json',
  'dotnet',
  'junit-xml',
  'sarif',
  'regex-custom',
  'generic-gcc',
];

/// Short human labels for the parser dropdown.
export const PARSER_LABELS: Record<ParserKind, string> = {
  'cargo-json': 'Cargo (cargo check --message-format=json)',
  'cargo-test': 'Cargo test',
  tsc: 'TypeScript (tsc)',
  'eslint-json': 'ESLint (--format json)',
  'jest-json': 'Jest / Vitest (--json)',
  pytest: 'pytest',
  go: 'Go build / vet',
  'go-test-json': 'Go test (-json)',
  dotnet: '.NET / MSBuild (dotnet build)',
  'junit-xml': 'JUnit XML report',
  sarif: 'SARIF 2.1',
  'regex-custom': 'Custom regex',
  'generic-gcc': 'Generic (file:line:col)',
};

/// Whether the parser reveals the `pattern` field (the `regex-custom` escape
/// hatch is the only parser that reads it).
export function showsPattern(parser: ParserKind): boolean {
  return parser === 'regex-custom';
}

/// Whether the parser reveals the `report_file` field — the tools that write a
/// report to disk rather than to stdout (JUnit XML and SARIF).
export function showsReportFile(parser: ParserKind): boolean {
  return parser === 'junit-xml' || parser === 'sarif';
}

/// A fresh, empty user-authored check (`auto = false` — anything the user
/// creates by hand is theirs and protected from re-detection).
export function newCheckDef(name = ''): CheckDef {
  return {
    name,
    cmd: '',
    parser: 'cargo-json',
    timeout_secs: 120,
    cwd: null,
    env: [],
    report_file: null,
    pattern: null,
    auto: false,
  };
}

/// Clear the auto-detection marker when the user edits an entry: an
/// auto-created check the user touches becomes user-owned, so a later
/// re-detection stops fighting the manual change (the contract documented on
/// `CheckDef.auto`). Returns the same object with `auto = false`; a no-op when
/// it was already user-owned.
export function clearAutoOnEdit<T extends CheckDef>(def: T): T {
  if (def.auto) def.auto = false;
  return def;
}

/// Classification of a `checks_test` dry run, driving the inline result badge.
/// - `error`        — validation/spawn failed before a report was produced.
/// - `timeout`      — the command was killed at its timeout.
/// - `diagnostics`  — the parser produced ≥ 1 diagnostic (the normal outcome).
/// - `wrong-parser` — the command produced output AND failed (non-zero exit),
///                    yet the parser matched ZERO diagnostics: almost always
///                    the wrong parser for this command's output shape. Gating
///                    on a non-zero exit avoids flagging a genuinely clean run
///                    (e.g. `cargo check` prints JSON envelope lines and exits 0
///                    with no diagnostics — that's `clean`, not a mismatch).
/// - `clean`        — zero diagnostics and the command succeeded (exit 0).
export type TestVerdict =
  | 'error'
  | 'timeout'
  | 'diagnostics'
  | 'wrong-parser'
  | 'clean';

export function classifyTestResult(r: ChecksTestResult): TestVerdict {
  if (r.error) return 'error';
  if (r.timed_out) return 'timeout';
  if (r.diag_count > 0) return 'diagnostics';
  const producedOutput = r.stdout_bytes + r.stderr_bytes > 0;
  const failed = r.exit_code !== 0; // null (only on timeout, handled) or non-zero
  if (producedOutput && failed) return 'wrong-parser';
  return 'clean';
}

/// The passive-nudge chip state for the Code Intelligence view, derived from the
/// suggestion payload and the current `checks` list.
/// - `suggest` — `checks` is empty and detection found N valid proposals: the
///   "N suggested checks" nudge (linking to the editor).
/// - `applied` — `checks_auto_configure` is on and detection already applied
///   entries (the `auto === true` checks): the chip reports what was applied
///   instead of nudging. (`checks_suggestion` returns count 0 once `checks` is
///   non-empty, so the two modes never collide.)
/// Both honor the per-project dismissal; `null` = show nothing.
export type ChipState =
  | { mode: 'suggest'; count: number }
  | { mode: 'applied'; names: string[] }
  | null;

export function computeChip(
  suggestion: ChecksSuggestion,
  checks: CheckDef[],
): ChipState {
  if (suggestion.dismissed) return null;
  if (checks.length > 0) {
    // Checks already configured: only nudge when auto-configure applied them
    // (report what landed). A user's own manual checks warrant no chip.
    if (suggestion.auto_configure) {
      const names = checks.filter((c) => c.auto).map((c) => c.name);
      if (names.length > 0) return { mode: 'applied', names };
    }
    return null;
  }
  // `checks` empty: the propose-then-approve nudge.
  if (suggestion.count > 0) return { mode: 'suggest', count: suggestion.count };
  return null;
}
