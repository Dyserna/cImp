import { describe, expect, test } from 'vitest';
import { turnTotal, maxTurnTotal, barHeightPct, cacheHitRatio, fmtTok } from './usageMath';

function turn(over: Partial<{ in_tok: number; cache_read: number; out_tok: number; tool_chars: number }>) {
  return { in_tok: 0, cache_read: 0, out_tok: 0, tool_chars: 0, ...over };
}

describe('turnTotal', () => {
  test('sums exact tokens plus estimated tool tokens (chars / 4)', () => {
    expect(turnTotal(turn({ in_tok: 100, cache_read: 50, out_tok: 20, tool_chars: 40 }))).toBe(
      100 + 50 + 20 + 10,
    );
  });

  test('rounds a non-multiple-of-4 tool_chars', () => {
    expect(turnTotal(turn({ tool_chars: 10 }))).toBe(3); // 2.5 rounds to 3 (banker's-free JS round)
  });

  test('an all-zero turn totals zero', () => {
    expect(turnTotal(turn({}))).toBe(0);
  });
});

describe('maxTurnTotal', () => {
  test('picks the tallest turn', () => {
    const turns = [turn({ in_tok: 10 }), turn({ in_tok: 500 }), turn({ in_tok: 20 })];
    expect(maxTurnTotal(turns)).toBe(500);
  });

  test('never returns 0 (avoids a divide-by-zero in barHeightPct)', () => {
    expect(maxTurnTotal([])).toBe(1);
    expect(maxTurnTotal([turn({})])).toBe(1);
  });
});

describe('barHeightPct', () => {
  test('normalizes to a 0-100 percentage', () => {
    expect(barHeightPct(50, 100)).toBe(50);
    expect(barHeightPct(100, 100)).toBe(100);
    expect(barHeightPct(0, 100)).toBe(0);
  });

  test('clamps above 100 and below 0', () => {
    expect(barHeightPct(150, 100)).toBe(100);
    expect(barHeightPct(-10, 100)).toBe(0);
  });

  test('a non-positive max never divides by zero', () => {
    expect(barHeightPct(10, 0)).toBe(0);
    expect(barHeightPct(10, -5)).toBe(0);
  });
});

describe('cacheHitRatio', () => {
  test('matches the backend formula: cache_read / (cache_read + in_tok)', () => {
    expect(cacheHitRatio(75, 25)).toBe(0.75);
    expect(cacheHitRatio(0, 100)).toBe(0);
    expect(cacheHitRatio(100, 0)).toBe(1);
  });

  test('no denominator (nothing recorded yet) is 0, not NaN', () => {
    expect(cacheHitRatio(0, 0)).toBe(0);
  });
});

describe('fmtTok', () => {
  test('small counts stay exact', () => {
    expect(fmtTok(0)).toBe('0');
    expect(fmtTok(412)).toBe('412');
    expect(fmtTok(999)).toBe('999');
  });

  test('thousands: one decimal below 10k, integer k above', () => {
    expect(fmtTok(1000)).toBe('1.0k');
    expect(fmtTok(9_949)).toBe('9.9k');
    expect(fmtTok(61_800)).toBe('62k');
    expect(fmtTok(999_400)).toBe('999k');
  });

  test('the k→M boundary never shows "1000k"', () => {
    expect(fmtTok(999_500)).toBe('1.00M');
  });

  test('millions: two decimals below 10M, one above', () => {
    expect(fmtTok(1_234_000)).toBe('1.23M');
    expect(fmtTok(61_234_567)).toBe('61.2M');
  });

  test('garbage in, zero out', () => {
    expect(fmtTok(-5)).toBe('0');
    expect(fmtTok(Number.NaN)).toBe('0');
  });
});
