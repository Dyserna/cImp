import { describe, expect, test } from 'vitest';
import { clampPct, hasQuotaData, humanizeTokens, statuslineRowsFor, usagePushHarness } from './contextMeter';
import {
  FIXTURE_HARNESSES,
  fixtureHarnessWith,
  fixtureHarnessWithout,
} from '../harness.fixture';
import type { UsageReading } from '../ipc';

describe('humanizeTokens', () => {
  test('matches the terminal status line buckets', () => {
    expect(humanizeTokens(0)).toBe('0');
    expect(humanizeTokens(940)).toBe('940');
    expect(humanizeTokens(12_345)).toBe('12k');
    expect(humanizeTokens(200_000)).toBe('200k');
    expect(humanizeTokens(1_000_000)).toBe('1.0M');
  });

  test('absent and non-finite render as unknown, not zero', () => {
    expect(humanizeTokens(null)).toBe('?');
    expect(humanizeTokens(undefined)).toBe('?');
    expect(humanizeTokens(NaN)).toBe('?');
    // A reported zero is still a zero — only absence becomes '?'.
    expect(humanizeTokens(0)).toBe('0');
  });
});

describe('hasQuotaData', () => {
  // A window at a REPORTED zero: the backend emits only windows that have a
  // reading, so its presence — not its number — is what makes quota drawable.
  const w = {
    id: 'five_hour',
    label: 'current session',
    short: '(5h)',
    description: 'Rolling 5-hour session quota',
    used: 0,
    resets_at: null,
  };
  const base = { stale: false, quota_stale: false, context_stale: false };

  test('any reported window counts, even one at zero', () => {
    expect(hasQuotaData({ ...base, windows: [w] })).toBe(true);
    expect(hasQuotaData({ ...base, windows: [{ ...w, id: 'seven_day' }] })).toBe(true);
  });

  test('a context-only push carries no quota', () => {
    const reading: UsageReading = {
      ...base,
      windows: [],
      context: { used_percentage: 12.5 },
    };
    expect(hasQuotaData(reading)).toBe(false);
    expect(hasQuotaData(null)).toBe(false);
  });
});

// V40 Phase F: every id below comes from the committed registry fixture, so
// this suite says what it means — "the harness that pushes readings", "one that
// does not" — instead of naming a product. `pusher` is whichever harness
// declares `usage_push`; `quiet` is one that does not.
const pusher = fixtureHarnessWith('usage_push')!;
const quiet = fixtureHarnessWithout('usage_push')!;
const all = FIXTURE_HARNESSES;

describe('usagePushHarness', () => {
  // The reserved tab of the pushing harness, its VARIANT tab (same binary,
  // second reserved id) and a tab of the harness that pushes nothing.
  const primary = { kind: 'ai_tool', id: pusher.tab_ids[0], command: pusher.binaries[0] };
  const variant = { kind: 'ai_tool', id: pusher.tab_ids[1], command: pusher.binaries[0] };
  const other = { kind: 'ai_tool', id: quiet.tab_ids[0], command: quiet.binaries[0] };
  const shell = { kind: 'shell', id: 'shell-default-1', command: pusher.binaries[0] };

  test('resolves the command on its file stem, like the backend does', () => {
    const bin = pusher.binaries[0];
    for (const command of [
      bin,
      bin.toUpperCase() + '.EXE',
      'C:/Users/me/bin/' + bin + '.exe',
      `/usr/local/bin/${bin}.cmd`,
      `  ${bin}  `,
    ]) {
      expect(usagePushHarness(all, [{ kind: 'ai_tool', id: 'ai-1', command }], [])).toBe(pusher.id);
    }
  });

  test('a command no harness declares resolves to no harness', () => {
    // Locked decision 2's frontend half: not the default harness, `null`.
    for (const command of [`${pusher.binaries[0]}-code`, '', undefined]) {
      expect(usagePushHarness(all, [{ kind: 'ai_tool', id: 'ai-1', command }], [])).toBe(null);
    }
  });

  test('the variant tab counts even with the primary one disabled', () => {
    // M15: status-line injection is per command, so this tab pushes too.
    expect(usagePushHarness(all, [primary, variant, other], [variant.id])).toBe(pusher.id);
  });

  test('a reserved tab that is switched off does not count', () => {
    expect(usagePushHarness(all, [primary, variant], [other.id])).toBe(null);
    expect(usagePushHarness(all, [primary, variant], [primary.id])).toBe(pusher.id);
  });

  test('user-created tabs count and are never id-gated', () => {
    const custom = {
      kind: 'ai_tool',
      id: 'ai-abc123',
      command: 'C:/tools/' + pusher.binaries[0] + '.exe',
    };
    expect(usagePushHarness(all, [custom], [])).toBe(pusher.id);
  });

  test('non-AI tabs never count', () => {
    expect(usagePushHarness(all, [shell], [])).toBe(null);
    // A Preview tab has no `command` field at all.
    expect(usagePushHarness(all, [{ kind: 'preview', id: 'preview-1' }], [primary.id])).toBe(null);
  });

  test('a harness with no usage source is never polled', () => {
    // It must not be rendered as a harness sitting at 0% either — the widget
    // hides on `null`.
    expect(usagePushHarness(all, [other], [other.id])).toBe(null);
    expect(usagePushHarness(all, [], [])).toBe(null);
    expect(usagePushHarness(all, null, [])).toBe(null);
  });

  test('before the registry answers, nothing is polled', () => {
    // The store starts empty; a widget that guessed a harness here would poll
    // an id the backend may not have.
    expect(usagePushHarness([], [primary], [primary.id])).toBe(null);
  });
});

describe('statuslineRowsFor', () => {
  test('the pushing harness declares how tall the strip must be', () => {
    expect(statuslineRowsFor(all, pusher.id)).toBe(pusher.affordances.statuslineRows);
    expect(statuslineRowsFor(all, pusher.id)).toBeGreaterThan(0);
  });

  test('an unknown or absent harness leaves the stylesheet default alone', () => {
    expect(statuslineRowsFor(all, 'nobody')).toBe(0);
    expect(statuslineRowsFor(all, null)).toBe(0);
  });
});

describe('clampPct', () => {
  test('keeps bar widths inside 0–100', () => {
    expect(clampPct(-5)).toBe(0);
    expect(clampPct(42.4)).toBe(42.4);
    expect(clampPct(150)).toBe(100);
    expect(clampPct(NaN)).toBe(0);
  });
});
