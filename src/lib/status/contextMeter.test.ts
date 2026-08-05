import { describe, expect, test } from 'vitest';
import {
  cacheHitPct,
  cacheSplitLabel,
  clampPct,
  contextTitle,
  contextTokensLabel,
  hasContextData,
  hasQuotaData,
  humanizeTokens,
} from './contextMeter';
import type { ContextSnapshot, UsageSnapshot } from '../ipc';

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

describe('hasContextData', () => {
  test('any reported number lights the group up', () => {
    expect(hasContextData({ used_percentage: 0 })).toBe(true);
    expect(hasContextData({ total_input_tokens: 0 })).toBe(true);
    expect(hasContextData({ context_window_size: 200_000 })).toBe(true);
    expect(hasContextData({ cache_read_tokens: 0 })).toBe(true);
    expect(hasContextData({ cache_creation_tokens: 0 })).toBe(true);
  });

  test('metadata alone is not renderable data', () => {
    expect(hasContextData(null)).toBe(false);
    expect(hasContextData(undefined)).toBe(false);
    expect(hasContextData({})).toBe(false);
    expect(hasContextData({ session_name: 'refactor', effort: 'high', fast_mode: true })).toBe(
      false,
    );
  });
});

describe('hasQuotaData', () => {
  const w = { utilization: 0, resets_at: null };

  test('either window independently counts', () => {
    expect(hasQuotaData({ five_hour: w, seven_day: null })).toBe(true);
    expect(hasQuotaData({ five_hour: null, seven_day: w })).toBe(true);
  });

  test('a context-only push carries no quota', () => {
    const snap: UsageSnapshot = {
      five_hour: null,
      seven_day: null,
      context: { used_percentage: 12.5 },
    };
    expect(hasQuotaData(snap)).toBe(false);
    expect(hasQuotaData(null)).toBe(false);
  });
});

describe('cacheHitPct', () => {
  test('read over the whole input of the turn', () => {
    const ctx: ContextSnapshot = {
      cache_read_tokens: 20_000,
      cache_creation_tokens: 5_000,
      input_tokens: 25_000,
    };
    // Cache-creation tokens count against the hit rate — they were not
    // served from cache (deliberately unlike usageMath.cacheHitRatio).
    expect(cacheHitPct(ctx)).toBeCloseTo(40, 6);
  });

  test('missing halves of the split default to zero tokens, not to absence', () => {
    expect(cacheHitPct({ cache_read_tokens: 100 })).toBe(100);
    expect(cacheHitPct({ cache_read_tokens: 50, input_tokens: 50 })).toBe(50);
  });

  test('unreported read and an idle turn are unknown, not 0%', () => {
    expect(cacheHitPct(null)).toBeNull();
    expect(cacheHitPct({})).toBeNull();
    expect(cacheHitPct({ input_tokens: 500 })).toBeNull();
    // Everything reported as zero: nothing was sent, so no ratio exists.
    expect(
      cacheHitPct({ cache_read_tokens: 0, cache_creation_tokens: 0, input_tokens: 0 }),
    ).toBeNull();
  });

  test('a reported zero read against real input is a genuine 0%', () => {
    expect(cacheHitPct({ cache_read_tokens: 0, input_tokens: 1_000 })).toBe(0);
  });
});

describe('figure labels', () => {
  test('context tokens fall back per-field, and vanish only when both are absent', () => {
    expect(contextTokensLabel({ total_input_tokens: 25_004, context_window_size: 200_000 })).toBe(
      '25k/200k',
    );
    expect(contextTokensLabel({ total_input_tokens: 25_004 })).toBe('25k/?');
    expect(contextTokensLabel({ context_window_size: 200_000 })).toBe('?/200k');
    expect(contextTokensLabel({})).toBeNull();
    expect(contextTokensLabel(null)).toBeNull();
  });

  test('cache split labels each half independently', () => {
    expect(cacheSplitLabel({ cache_read_tokens: 20_000, cache_creation_tokens: 5_000 })).toBe(
      'read 20k · new 5k',
    );
    expect(cacheSplitLabel({ cache_read_tokens: 20_000 })).toBe('read 20k · new ?');
    expect(cacheSplitLabel({ cache_creation_tokens: 0 })).toBe('read ? · new 0');
    expect(cacheSplitLabel({})).toBeNull();
  });
});

describe('contextTitle', () => {
  test('surfaces whatever session metadata rode along', () => {
    const t = contextTitle({
      used_percentage: 12.5,
      session_name: 'refactor the parser',
      agent_name: 'reviewer',
      effort: 'high',
      thinking: 'on',
      fast_mode: false,
    });
    expect(t).toContain('session: refactor the parser');
    expect(t).toContain('agent: reviewer');
    expect(t).toContain('effort: high');
    expect(t).toContain('thinking: on');
    // A reported `false` is still information — it must not be dropped.
    expect(t).toContain('fast mode: off');
  });

  test('degrades to the bare header when nothing rode along', () => {
    expect(contextTitle({}).split('\n')).toHaveLength(1);
    expect(contextTitle(null)).toContain('Live context window');
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
