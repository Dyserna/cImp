import { describe, expect, test } from 'vitest';
import {
  clampPct,
  claudePushTabActive,
  commandIsClaude,
  hasQuotaData,
  humanizeTokens,
  usagePushHarness,
} from './contextMeter';
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

describe('claudePushTabActive', () => {
  const claude = { kind: 'ai_tool', id: 'claude', command: 'claude' };
  const claudeLocal = { kind: 'ai_tool', id: 'claude-local', command: 'claude' };
  const opencode = { kind: 'ai_tool', id: 'opencode', command: 'opencode' };
  const shell = { kind: 'shell', id: 'shell-default-1', command: 'claude' };

  test('mirrors the backend command_is check', () => {
    expect(commandIsClaude('claude')).toBe(true);
    expect(commandIsClaude('CLAUDE.EXE')).toBe(true);
    expect(commandIsClaude('C:\\Users\\me\\bin\\claude.exe')).toBe(true);
    expect(commandIsClaude('/usr/local/bin/claude.cmd')).toBe(true);
    expect(commandIsClaude('  claude  ')).toBe(true);
    expect(commandIsClaude('opencode')).toBe(false);
    expect(commandIsClaude('claude-code')).toBe(false);
    expect(commandIsClaude('')).toBe(false);
    expect(commandIsClaude(undefined)).toBe(false);
  });

  test('claude-local counts even with the subscription tab disabled', () => {
    // M15: statusline injection is per command, so this tab pushes too.
    expect(claudePushTabActive([claude, claudeLocal, opencode], ['claude-local'])).toBe(true);
  });

  test('a reserved tab that is switched off does not count', () => {
    expect(claudePushTabActive([claude, claudeLocal], ['opencode'])).toBe(false);
    expect(claudePushTabActive([claude, claudeLocal], ['claude'])).toBe(true);
  });

  test('user-created claude-command tabs count and are never id-gated', () => {
    const custom = { kind: 'ai_tool', id: 'ai-abc123', command: 'C:\\tools\\claude.exe' };
    expect(claudePushTabActive([custom], [])).toBe(true);
  });

  test('non-AI tabs and non-claude commands never count', () => {
    expect(claudePushTabActive([shell, opencode], ['opencode'])).toBe(false);
    expect(claudePushTabActive([], ['claude'])).toBe(false);
    expect(claudePushTabActive(null, ['claude'])).toBe(false);
    // A Preview tab has no `command` field at all.
    expect(claudePushTabActive([{ kind: 'preview', id: 'preview-1' }], ['claude'])).toBe(false);
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

describe('usagePushHarness', () => {
  const claude = { kind: 'ai_tool', id: 'claude', command: 'claude' };
  const opencode = { kind: 'ai_tool', id: 'opencode', command: 'opencode' };

  test('names the harness whose usage source the widget should poll', () => {
    expect(usagePushHarness([claude], ['claude'])).toBe('claude');
  });

  test('is null when nothing running can push a reading', () => {
    // OpenCode has NO usage source; the widget must not poll — and must never
    // render a harness that cannot report quota as a harness at 0%.
    expect(usagePushHarness([opencode], ['opencode'])).toBe(null);
    expect(usagePushHarness([], [])).toBe(null);
  });
});
