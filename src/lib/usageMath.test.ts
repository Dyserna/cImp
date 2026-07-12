import { describe, expect, test } from 'vitest';
import {
  turnTotal,
  maxTurnTotal,
  barHeightPct,
  cacheHitRatio,
  fmtTok,
  costUsd,
  sessionCost,
  fmtUsd,
  matchPricing,
  turnCost,
} from './usageMath';

function turn(
  over: Partial<{ in_tok: number; cache_read: number; cache_make: number; out_tok: number; tool_chars: number }>,
) {
  return { in_tok: 0, cache_read: 0, cache_make: 0, out_tok: 0, tool_chars: 0, ...over };
}

describe('turnTotal', () => {
  test('sums exact tokens plus estimated tool tokens (chars / 4)', () => {
    expect(
      turnTotal(turn({ in_tok: 100, cache_read: 50, cache_make: 5, out_tok: 20, tool_chars: 40 })),
    ).toBe(100 + 50 + 5 + 20 + 10);
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

  test('a fractional count at the 1000 boundary never prints a bare "1000"', () => {
    // Regression (2026-07 review): the < 1000 branch tested BEFORE rounding,
    // so 999.6 rounded up inside it and printed "1000" with no suffix.
    expect(fmtTok(999.6)).toBe('1.0k');
    expect(fmtTok(999.4)).toBe('999');
  });
});

describe('costUsd', () => {
  test('tokens × $/MTok, in dollars', () => {
    expect(costUsd(1_000_000, 5)).toBe(5);
    expect(costUsd(500_000, 10)).toBe(5);
    expect(costUsd(0, 25)).toBe(0);
  });

  test('non-finite inputs (half-typed custom price field) cost 0, not NaN', () => {
    expect(costUsd(Number.NaN, 5)).toBe(0);
    expect(costUsd(1000, Number.NaN)).toBe(0);
    expect(costUsd(1000, Number.POSITIVE_INFINITY)).toBe(0);
  });
});

describe('sessionCost', () => {
  const rates = { input: 5, cache_write: 6.25, cache_read: 0.5, output: 25 };

  test('maps each UsageTotals category to its rate and sums the total', () => {
    const c = sessionCost(
      { in_tok: 1_000_000, cache_make: 2_000_000, cache_read: 4_000_000, out_tok: 100_000 },
      rates,
    );
    expect(c.input).toBe(5);
    expect(c.cache_write).toBe(12.5);
    expect(c.cache_read).toBe(2);
    expect(c.output).toBe(2.5);
    expect(c.total).toBe(22);
  });

  test('an all-zero session costs zero', () => {
    const c = sessionCost({ in_tok: 0, cache_make: 0, cache_read: 0, out_tok: 0 }, rates);
    expect(c.total).toBe(0);
  });
});

describe('matchPricing (V16 Feature 8)', () => {
  const rates = { input: 5, cache_write: 10, cache_read: 0.5, output: 25 };
  const rows = [
    { model_prefix: 'claude-opus-4', name: 'family', ...rates },
    { model_prefix: 'claude-opus-4-8', name: 'exact', ...rates },
    { model_prefix: '', name: 'manual-only', ...rates },
  ];

  test('longest matching prefix wins', () => {
    expect(matchPricing('claude-opus-4-8', rows)?.name).toBe('exact');
    // A dated snapshot still hits the longest prefix.
    expect(matchPricing('claude-opus-4-8-20260115', rows)?.name).toBe('exact');
    // A sibling model falls back to the shorter family prefix.
    expect(matchPricing('claude-opus-4-7', rows)?.name).toBe('family');
  });

  test('empty prefixes never auto-match; unknown models match nothing', () => {
    expect(matchPricing('gpt-5.5', rows)).toBeNull();
    expect(matchPricing('', rows)).toBeNull();
    expect(matchPricing(null, rows)).toBeNull();
    expect(matchPricing(undefined, rows)).toBeNull();
  });
});

describe('turnCost (V16 Feature 8)', () => {
  const rates = { input: 5, cache_write: 10, cache_read: 0.5, output: 25 };

  test('prices each segment at its own rate; tool chars bill at input rate on chars/4', () => {
    const c = turnCost(
      { in_tok: 1_000_000, cache_read: 1_000_000, cache_make: 1_000_000, out_tok: 1_000_000, tool_chars: 4_000_000 },
      rates,
    );
    expect(c.input).toBe(5);
    expect(c.cache_read).toBe(0.5);
    expect(c.cache_write).toBe(10);
    expect(c.output).toBe(25);
    expect(c.tool).toBe(5); // 1M est. tokens at the input rate
    expect(c.total).toBe(45.5);
  });

  test('an all-zero turn costs zero', () => {
    expect(
      turnCost({ in_tok: 0, cache_read: 0, cache_make: 0, out_tok: 0, tool_chars: 0 }, rates).total,
    ).toBe(0);
  });
});

describe('fmtUsd', () => {
  test('2 decimals from $1 up, 4 below so sub-cent costs stay visible', () => {
    expect(fmtUsd(22)).toBe('$22.00');
    expect(fmtUsd(1)).toBe('$1.00');
    expect(fmtUsd(0.1234)).toBe('$0.1234');
    expect(fmtUsd(0.0004)).toBe('$0.0004');
    expect(fmtUsd(0)).toBe('$0.0000');
  });

  test('non-finite guards to $0.00', () => {
    expect(fmtUsd(Number.NaN)).toBe('$0.00');
  });
});
