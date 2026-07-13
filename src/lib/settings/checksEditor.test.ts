import { describe, expect, test } from 'vitest';

import type { CheckDef, ChecksSuggestion, ChecksTestResult } from './types';
import {
  PARSER_KINDS,
  PARSER_LABELS,
  classifyTestResult,
  clearAutoOnEdit,
  computeChip,
  newCheckDef,
  showsPattern,
  showsReportFile,
} from './checksEditor';

function testResult(over: Partial<ChecksTestResult> = {}): ChecksTestResult {
  return {
    exit_code: 0,
    duration_ms: 1,
    timed_out: false,
    diag_count: 0,
    stdout_bytes: 0,
    stderr_bytes: 0,
    diagnostics: [],
    error: null,
    ...over,
  };
}

function check(over: Partial<CheckDef> = {}): CheckDef {
  return { ...newCheckDef('c'), ...over };
}

function suggestion(over: Partial<ChecksSuggestion> = {}): ChecksSuggestion {
  return { count: 0, dismissed: false, auto_configure: false, ...over };
}

describe('conditional fields', () => {
  test('regex-custom reveals the pattern field, nothing else does', () => {
    expect(showsPattern('regex-custom')).toBe(true);
    for (const p of PARSER_KINDS) {
      if (p !== 'regex-custom') expect(showsPattern(p)).toBe(false);
    }
  });

  test('junit-xml and sarif reveal the report_file field, nothing else does', () => {
    expect(showsReportFile('junit-xml')).toBe(true);
    expect(showsReportFile('sarif')).toBe(true);
    for (const p of PARSER_KINDS) {
      if (p !== 'junit-xml' && p !== 'sarif') expect(showsReportFile(p)).toBe(false);
    }
  });

  test('PARSER_KINDS is exactly the PARSER_LABELS keys, in order (no magic count)', () => {
    // PARSER_KINDS is derived from PARSER_LABELS, which is an exhaustiveness-
    // checked Record<ParserKind, string> — so this guards the derivation (and
    // its order), not a hand-bumped literal. A new ParserKind can't slip past:
    // the compiler forces a label, which flows into both here and the dropdown.
    expect([...PARSER_KINDS]).toEqual(Object.keys(PARSER_LABELS));
    expect(new Set(PARSER_KINDS).size).toBe(PARSER_KINDS.length); // no dupes
    expect(PARSER_KINDS).toContain('cargo-test');
    expect(PARSER_KINDS).toContain('jest-json');
  });
});

describe('clearAutoOnEdit', () => {
  test('an auto entry becomes user-owned on edit', () => {
    const d = clearAutoOnEdit(check({ auto: true }));
    expect(d.auto).toBe(false);
  });

  test('a user-owned entry stays user-owned', () => {
    const d = clearAutoOnEdit(check({ auto: false }));
    expect(d.auto).toBe(false);
  });

  test('newCheckDef is user-owned by default', () => {
    expect(newCheckDef().auto).toBe(false);
  });
});

describe('classifyTestResult', () => {
  test('validation/spawn error wins', () => {
    expect(classifyTestResult(testResult({ error: 'escaping cwd' }))).toBe('error');
  });

  test('timeout is reported before anything else runtime', () => {
    expect(classifyTestResult(testResult({ timed_out: true, exit_code: null }))).toBe('timeout');
  });

  test('diagnostics parsed → diagnostics', () => {
    expect(classifyTestResult(testResult({ diag_count: 3, exit_code: 1, stderr_bytes: 200 }))).toBe(
      'diagnostics',
    );
  });

  test('zero diags + output + failure → wrong-parser', () => {
    expect(
      classifyTestResult(testResult({ diag_count: 0, exit_code: 1, stdout_bytes: 500 })),
    ).toBe('wrong-parser');
  });

  test('zero diags + output but exit 0 → clean (no false positive on a quiet cargo check)', () => {
    expect(
      classifyTestResult(testResult({ diag_count: 0, exit_code: 0, stdout_bytes: 4000 })),
    ).toBe('clean');
  });

  test('zero diags + failure but no output → clean (nothing to blame the parser for)', () => {
    expect(
      classifyTestResult(testResult({ diag_count: 0, exit_code: 1, stdout_bytes: 0, stderr_bytes: 0 })),
    ).toBe('clean');
  });
});

describe('computeChip', () => {
  test('empty checks + valid proposals → suggest', () => {
    expect(computeChip(suggestion({ count: 2 }), [])).toEqual({ mode: 'suggest', count: 2 });
  });

  test('dismissed → nothing', () => {
    expect(computeChip(suggestion({ count: 2, dismissed: true }), [])).toBeNull();
  });

  test('empty checks + zero proposals → nothing', () => {
    expect(computeChip(suggestion({ count: 0 }), [])).toBeNull();
  });

  test('configured manually (no auto-configure) → nothing', () => {
    expect(computeChip(suggestion(), [check({ auto: false })])).toBeNull();
  });

  test('auto-configure applied entries → applied, listing the auto names', () => {
    const chip = computeChip(suggestion({ auto_configure: true }), [
      check({ name: 'cargo', auto: true }),
      check({ name: 'mine', auto: false }),
    ]);
    expect(chip).toEqual({ mode: 'applied', names: ['cargo'] });
  });

  test('auto-configure on but no auto entries → nothing', () => {
    expect(computeChip(suggestion({ auto_configure: true }), [check({ auto: false })])).toBeNull();
  });
});
