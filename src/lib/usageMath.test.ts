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
  laneSegments,
  laneLabelVisible,
  agentBarClass,
  sessionRowState,
  LANE_LABEL_MIN_PX,
  matchPricingIndex,
  resolveRates,
  pricingRowKey,
  costSelIdx,
  costRowState,
  costOverrideForIdx,
  isEmptyDetailRow,
  isActiveSessionIn,
  decideUsageApply,
  costGrandTotal,
  originShareLine,
  FREE_RATES,
  originKindTotals,
  kindsTotal,
  laneLabel,
  donutArcs,
  arcPath,
  fmtPct,
  type PriceRates,
  type CostOverride,
} from './usageMath';

import { FIRST_HARNESS } from './harness.fixture';

// V40 Phase F: every harness id below comes from the committed registry fixture
// (`harness.fixture.ts`), so this suite names no product and re-points itself
// when the registry changes.
const H1 = FIRST_HARNESS.id;

// V40 Phase G: a turn's tokens are a DECLARED-CATEGORY map now. `turn()` takes
// the same four names the suite has always used and emits them under the four
// pricing ids (`input` / `cache_read` / `cache_write` / `output`), so every
// existing expectation below still asserts the same arithmetic on the same
// numbers — which is the point: this refactor must not move a single rendered
// figure.
function kinds(
  over: Partial<{ in_tok: number; cache_read: number; cache_make: number; out_tok: number }> = {},
): Record<string, number> {
  return {
    input: over.in_tok ?? 0,
    cache_read: over.cache_read ?? 0,
    cache_write: over.cache_make ?? 0,
    output: over.out_tok ?? 0,
  };
}

function turn(
  over: Partial<{ in_tok: number; cache_read: number; cache_make: number; out_tok: number; tool_chars: number }>,
) {
  return { tokens: kinds(over), tool_chars: over.tool_chars ?? 0 };
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
  test('matches the backend formula: cache_read / (cache_read + input)', () => {
    expect(cacheHitRatio(kinds({ cache_read: 75, in_tok: 25 }))).toBe(0.75);
    expect(cacheHitRatio(kinds({ cache_read: 0, in_tok: 100 }))).toBe(0);
    expect(cacheHitRatio(kinds({ cache_read: 100, in_tok: 0 }))).toBe(1);
  });

  test('no denominator (nothing recorded yet) is null, not 0 and not NaN', () => {
    expect(cacheHitRatio(kinds())).toBeNull();
  });

  test('a harness that declares neither category has no ratio at all', () => {
    // V40 Phase G: absence is not zero. A harness billing one flat token
    // number reports no `cache_read` and no `input`, and "0% cache hit" would
    // be a claim it never made.
    expect(cacheHitRatio({ output: 500 })).toBeNull();
  });

  test('a partially declared shape still divides by what it has', () => {
    expect(cacheHitRatio({ input: 100 })).toBe(0);
    expect(cacheHitRatio({ cache_read: 100 })).toBe(1);
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

  test('maps each declared category to its rate and sums the total', () => {
    const c = sessionCost(
      kinds({ in_tok: 1_000_000, cache_make: 2_000_000, cache_read: 4_000_000, out_tok: 100_000 }),
      rates,
    );
    expect(c.input).toBe(5);
    expect(c.cache_write).toBe(12.5);
    expect(c.cache_read).toBe(2);
    expect(c.output).toBe(2.5);
    expect(c.total).toBe(22);
  });

  test('an all-zero session costs zero', () => {
    expect(sessionCost(kinds(), rates).total).toBe(0);
  });

  test('an ABSENT category costs nothing rather than throwing or NaN-ing', () => {
    // V40 Phase G: a harness that does not bill cache tokens reports no
    // `cache_read` / `cache_write` key at all. You cannot be charged for
    // tokens nobody reported, so absence prices at $0 — the one place reading
    // absence as zero is honest.
    const c = sessionCost({ input: 1_000_000, output: 100_000 }, rates);
    expect(c.input).toBe(5);
    expect(c.output).toBe(2.5);
    expect(c.cache_read).toBe(0);
    expect(c.cache_write).toBe(0);
    expect(c.total).toBe(7.5);
  });
});

describe('matchPricing (V16 Feature 8)', () => {
  const rates = { input: 5, cache_write: 10, cache_read: 0.5, output: 25 };
  const rows = [
    { model_prefix: 'vendor-opus-4', name: 'family', ...rates },
    { model_prefix: 'vendor-opus-4-8', name: 'exact', ...rates },
    { model_prefix: '', name: 'manual-only', ...rates },
  ];

  test('longest matching prefix wins', () => {
    expect(matchPricing('vendor-opus-4-8', rows)?.name).toBe('exact');
    // A dated snapshot still hits the longest prefix.
    expect(matchPricing('vendor-opus-4-8-20260115', rows)?.name).toBe('exact');
    // A sibling model falls back to the shorter family prefix.
    expect(matchPricing('vendor-opus-4-7', rows)?.name).toBe('family');
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
      turn({ in_tok: 1_000_000, cache_read: 1_000_000, cache_make: 1_000_000, out_tok: 1_000_000, tool_chars: 4_000_000 }),
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
    expect(turnCost(turn({}), rates).total).toBe(0);
  });
});

describe('laneSegments (V24 Phase C)', () => {
  const o = (...origins: string[]) => origins.map((origin) => ({ origin }));

  test('merges contiguous same-origin runs into segments', () => {
    // S S A A A S  ->  S×2, A×3, S×1
    expect(laneSegments(o('session', 'session', 'agent', 'agent', 'agent', 'session'))).toEqual([
      { origin: 'session', label: 'S', count: 2 },
      { origin: 'agent', label: 'A', count: 3 },
      { origin: 'session', label: 'S', count: 1 },
    ]);
  });

  test('a single-origin session collapses to exactly one segment', () => {
    expect(laneSegments(o('session', 'session', 'session'))).toEqual([
      { origin: 'session', label: 'S', count: 3 },
    ]);
    expect(laneSegments(o('agent', 'agent'))).toEqual([{ origin: 'agent', label: 'A', count: 2 }]);
  });

  test('labels: session -> S, agent -> A', () => {
    const segs = laneSegments(o('agent', 'session'));
    expect(segs.map((s) => s.label)).toEqual(['A', 'S']);
  });

  test('a THIRD declared lane gets its own badge instead of borrowing S', () => {
    // V40 Phase G: the badge is the first character of the declared id, so a
    // harness with a lane the two shipped ones do not have is representable.
    // Pre-G the label was `origin === 'agent' ? 'A' : 'S'`, which silently
    // rendered every unknown lane as the main session's letter.
    expect(laneSegments(o('review', 'session')).map((s) => s.label)).toEqual(['R', 'S']);
    expect(laneLabel('session')).toBe('S');
    expect(laneLabel('agent')).toBe('A');
    expect(laneLabel('')).toBe('?');
  });

  test('alternating origins yield one segment per turn', () => {
    expect(laneSegments(o('session', 'agent', 'session')).map((s) => s.count)).toEqual([1, 1, 1]);
  });

  test('no turns -> no segments', () => {
    expect(laneSegments([])).toEqual([]);
  });
});

describe('laneLabelVisible (V24 Phase C)', () => {
  test('shows the label once the segment is at least ~2 chars wide', () => {
    // 4 turns across a 400px lane => 100px each; a 1-turn segment clears the
    // threshold, so any segment does.
    expect(laneLabelVisible(1, 4, 400)).toBe(true);
    // A wide, cramped lane: 200 turns across 400px => 2px each; a 5-turn
    // segment is 10px < LANE_LABEL_MIN_PX (14) => hidden (tooltip only).
    expect(laneLabelVisible(5, 200, 400)).toBe(false);
    // ...but a 7-turn run at the same density is 14px => shown.
    expect(laneLabelVisible(7, 200, 400)).toBe(true);
  });

  test('hidden before the lane has measured (width 0) or with no turns', () => {
    expect(laneLabelVisible(3, 10, 0)).toBe(false);
    expect(laneLabelVisible(3, 0, 400)).toBe(false);
  });

  test('threshold constant is ~2 characters', () => {
    expect(LANE_LABEL_MIN_PX).toBe(14);
  });
});

describe('agentBarClass (V24 Phase C)', () => {
  test('a DECLARED sub-agent lane gets the "agent" class, the main lane none', () => {
    expect(agentBarClass('agent', ['agent'])).toBe('agent');
    expect(agentBarClass('session', ['agent'])).toBe('');
  });

  test('no declaration yet ⇒ no bar is marked (never marked by guess)', () => {
    // V40 Phase G: which lane is the fan-out one is the harness's statement
    // (`origins[].subagent`), not the word "agent". Before the declaration
    // arrives the chart is un-annotated rather than annotated wrongly.
    expect(agentBarClass('agent', [])).toBe('');
  });

  test('another harness can name its fan-out lane something else', () => {
    expect(agentBarClass('worker', ['worker'])).toBe('agent');
  });
});

describe('sessionRowState (V24 Phase E)', () => {
  const active = ['s1', 's2'];
  test('active when the id is in active_session_ids', () => {
    expect(sessionRowState('s1', null, active)).toEqual({ active: true, selected: false });
    expect(sessionRowState('s2', null, active)).toEqual({ active: true, selected: false });
  });
  test('selected when the id is the selected one', () => {
    expect(sessionRowState('s9', 's9', active)).toEqual({ active: false, selected: true });
  });
  test('active and selected coexist on the same row', () => {
    expect(sessionRowState('s1', 's1', active)).toEqual({ active: true, selected: true });
  });
  test('neither when unmatched', () => {
    expect(sessionRowState('other', 's1', active)).toEqual({ active: false, selected: false });
  });
  test('missing/empty inputs are safe (no active ids, no selection)', () => {
    expect(sessionRowState('s1', null, [])).toEqual({ active: false, selected: false });
    expect(sessionRowState('s1', undefined, undefined)).toEqual({ active: false, selected: false });
    expect(sessionRowState('s1', null, null)).toEqual({ active: false, selected: false });
  });
});

describe('isEmptyDetailRow (V24 Phase C — vanished-session guard)', () => {
  test('the backend empty sentinel (blank agent + zero started_ms) is detected', () => {
    expect(isEmptyDetailRow({ agent: '', started_ms: 0 })).toBe(true);
  });
  test('a real row is never treated as empty', () => {
    expect(isEmptyDetailRow({ agent: H1, started_ms: 1_700_000_000_000 })).toBe(false);
  });
  test('only-one-field-set is not the sentinel (needs BOTH blank agent and zero ts)', () => {
    // A live session started exactly at epoch 0 would still carry an agent;
    // an agent-less row with a real timestamp shouldn't be a real session.
    expect(isEmptyDetailRow({ agent: H1, started_ms: 0 })).toBe(false);
    expect(isEmptyDetailRow({ agent: '', started_ms: 1 })).toBe(false);
  });
});

describe('isActiveSessionIn (#130 — the one live-session predicate)', () => {
  test('a session named in the list is live', () => {
    expect(isActiveSessionIn(['a', 'b'], 'b')).toBe(true);
  });
  test('a session the list does not name is not', () => {
    expect(isActiveSessionIn(['a'], 'b')).toBe(false);
  });
  test('no snapshot yet (undefined ids) answers false, it does not throw', () => {
    // The Overview owns the snapshot and the Memory section is told about it;
    // before the first `graph_usage` lands BOTH readers hold nothing, and the
    // label they drive must read `last session`, not crash the view.
    expect(isActiveSessionIn(undefined, 'a')).toBe(false);
    expect(isActiveSessionIn([], 'a')).toBe(false);
  });
  test('an absent session id is never live, whatever the list says', () => {
    expect(isActiveSessionIn(['a'], null)).toBe(false);
    expect(isActiveSessionIn(['a'], undefined)).toBe(false);
    expect(isActiveSessionIn([''], '')).toBe(false);
  });
});

describe('decideUsageApply (store_error contract)', () => {
  test('a healthy snapshot applies and clears the error state', () => {
    expect(decideUsageApply({ store_error: null }, true)).toEqual({
      apply: true,
      errored: false,
      flash: false,
    });
  });

  test('empty is not absent: a healthy snapshot with no sessions still applies', () => {
    // The zero-sessions case must reach the UI so it renders "0" — only
    // `store_error` may suppress an apply.
    expect(decideUsageApply({ store_error: null }, false).apply).toBe(true);
  });

  test('a store_error snapshot is not applied — last-good data stays on screen', () => {
    expect(decideUsageApply({ store_error: 'lock timeout' }, false)).toEqual({
      apply: false,
      errored: true,
      flash: true,
    });
  });

  test('a persistent store_error flashes only on the transition into it', () => {
    const first = decideUsageApply({ store_error: 'busy' }, false);
    expect(first.flash).toBe(true);
    // Next 2s tick, same condition → no re-flash (no notice spam).
    expect(decideUsageApply({ store_error: 'busy' }, first.errored).flash).toBe(false);
    // Recovered, then broken again → flashes again.
    const healthy = decideUsageApply({ store_error: null }, true);
    expect(decideUsageApply({ store_error: 'busy' }, healthy.errored).flash).toBe(true);
  });

  test('a failed IPC (null) keeps last-good data and leaves the error state untouched', () => {
    expect(decideUsageApply(null, false)).toEqual({ apply: false, errored: false, flash: false });
    expect(decideUsageApply(null, true)).toEqual({ apply: false, errored: true, flash: false });
  });

  test('a snapshot from an older build without the field is treated as healthy', () => {
    expect(decideUsageApply({}, false).apply).toBe(true);
  });
});

describe('Cost card pricing (V24 Phase D)', () => {
  const rates = { input: 5, cache_write: 10, cache_read: 0.5, output: 25 };
  // Identified rows (provider + model give each a stable key) — the shape the
  // stable-identity resolver keys overrides against.
  const rows = [
    { provider: 'vendor', model: 'opus-4-family', model_prefix: 'vendor-opus-4', ...rates },
    { provider: 'vendor', model: 'opus-4-8', model_prefix: 'vendor-opus-4-8', ...rates },
    { provider: 'vendor', model: 'manual-only', model_prefix: '', ...rates },
  ];

  describe('matchPricingIndex', () => {
    test('returns the index of the longest-prefix match', () => {
      expect(matchPricingIndex('vendor-opus-4-8-20260115', rows)).toBe(1); // exact
      expect(matchPricingIndex('vendor-opus-4-7', rows)).toBe(0); // family
    });
    test('-1 when nothing matches (unknown model, empty/blank prefixes only)', () => {
      expect(matchPricingIndex('gpt-5.5', rows)).toBe(-1);
      expect(matchPricingIndex(null, rows)).toBe(-1);
      expect(matchPricingIndex('anything', [{ model_prefix: '', ...rates }])).toBe(-1);
    });
  });

  describe('matchPricing (row form ≡ index form)', () => {
    test('returns the same row matchPricingIndex points at', () => {
      expect(matchPricing('vendor-opus-4-8-20260115', rows)).toBe(rows[1]);
      expect(matchPricing('vendor-opus-4-7', rows)).toBe(rows[0]);
    });
    test('null when the index form returns -1', () => {
      expect(matchPricing('gpt-5.5', rows)).toBeNull();
      expect(matchPricing(null, rows)).toBeNull();
    });
  });

  describe('pricingRowKey (stable identity)', () => {
    test('is provider + " " + model', () => {
      expect(pricingRowKey(rows[1])).toBe('vendor opus-4-8');
    });
  });

  describe('costSelIdx (stable-key resolution against the CURRENT table)', () => {
    const CUSTOM = rows.length;
    const FREE = rows.length + 1;
    test('no override → auto-match by longest prefix', () => {
      expect(costSelIdx('vendor-opus-4-8-x', undefined, rows)).toBe(1);
      expect(costSelIdx('vendor-opus-4-7', undefined, rows)).toBe(0);
    });
    test('no override + no match → Custom sentinel (never a made-up cost)', () => {
      expect(costSelIdx('gpt-5.5', undefined, rows)).toBe(CUSTOM);
      expect(costSelIdx(null, undefined, rows)).toBe(CUSTOM);
    });
    test('a row override resolves to that row\'s CURRENT index', () => {
      const ov: CostOverride = { kind: 'row', key: pricingRowKey(rows[0]) };
      expect(costSelIdx('vendor-opus-4-8-x', ov, rows)).toBe(0); // wins over auto-match
    });
    test('a row override survives table REORDER (keyed, not positional)', () => {
      const ov: CostOverride = { kind: 'row', key: pricingRowKey(rows[1]) };
      const reordered = [rows[2], rows[1], rows[0]];
      expect(costSelIdx('vendor-opus-4-8-x', ov, reordered)).toBe(1); // still the opus-4-8 row
    });
    test('a VANISHED row key falls back to auto-match, not silently Free', () => {
      const ov: CostOverride = { kind: 'row', key: 'anthropic gone' };
      // auto-match still finds opus-4-8 for this model:
      expect(costSelIdx('vendor-opus-4-8-x', ov, rows)).toBe(1);
      // and Custom when the model itself no longer matches anything:
      expect(costSelIdx('gpt-5.5', ov, rows)).toBe(CUSTOM);
    });
    test('custom / free overrides map to their sentinels', () => {
      expect(costSelIdx('vendor-opus-4-8-x', { kind: 'custom' }, rows)).toBe(CUSTOM);
      expect(costSelIdx('vendor-opus-4-8-x', { kind: 'free' }, rows)).toBe(FREE);
    });
  });

  describe('costRowState (selIdx + rates + matchedRow)', () => {
    const custom: PriceRates = { input: 1, cache_write: 2, cache_read: 3, output: 4 };
    test('auto-matched row: rates are that row, matchedRow set', () => {
      const st = costRowState('vendor-opus-4-8-x', undefined, rows, custom);
      expect(st.selIdx).toBe(1);
      expect(st.rates).toBe(rows[1]);
      expect(st.matchedRow).toBe(rows[1]);
    });
    test('unmatched model: Custom rates, matchedRow null', () => {
      const st = costRowState('gpt-5.5', undefined, rows, custom);
      expect(st.selIdx).toBe(rows.length);
      expect(st.rates).toBe(custom);
      expect(st.matchedRow).toBeNull();
    });
    test('free override: all-zero rates regardless of the model', () => {
      const st = costRowState('vendor-opus-4-8-x', { kind: 'free' }, rows, custom);
      expect(st.rates).toEqual(FREE_RATES);
      // matchedRow still reflects the auto-match (for the provider label):
      expect(st.matchedRow).toBe(rows[1]);
    });
    test('vanished row key: rates fall back to the auto-matched row', () => {
      const st = costRowState('vendor-opus-4-8-x', { kind: 'row', key: 'gone gone' }, rows, custom);
      expect(st.rates).toBe(rows[1]);
    });
  });

  describe('costOverrideForIdx (chosen index → stable override)', () => {
    test('a table-row index records the row by its stable key', () => {
      expect(costOverrideForIdx(1, rows)).toEqual({ kind: 'row', key: pricingRowKey(rows[1]) });
    });
    test('the trailing indices record the Custom / Free sentinels', () => {
      expect(costOverrideForIdx(rows.length, rows)).toEqual({ kind: 'custom' });
      expect(costOverrideForIdx(rows.length + 1, rows)).toEqual({ kind: 'free' });
    });
    test('round-trips through costSelIdx', () => {
      for (let i = 0; i < rows.length + 2; i++) {
        expect(costSelIdx('anything', costOverrideForIdx(i, rows), rows)).toBe(i);
      }
    });
  });

  describe('resolveRates (row / custom / free precedence)', () => {
    const custom: PriceRates = { input: 1, cache_write: 2, cache_read: 3, output: 4 };
    test('an in-range index resolves to that table row', () => {
      expect(resolveRates(1, rows, custom)).toBe(rows[1]);
    });
    test('the Custom sentinel resolves to the hand-typed rates', () => {
      expect(resolveRates(rows.length, rows, custom)).toBe(custom);
    });
    test('the Free option is all-zero rates', () => {
      expect(resolveRates(rows.length + 1, rows, custom)).toEqual(FREE_RATES);
      expect(resolveRates(rows.length + 1, rows, custom)).toEqual({
        input: 0,
        cache_write: 0,
        cache_read: 0,
        output: 0,
      });
    });
    test('a stale/out-of-range index falls back to Free, never a wrong row', () => {
      expect(resolveRates(99, rows, custom)).toEqual(FREE_RATES);
      expect(resolveRates(-1, rows, custom)).toEqual(FREE_RATES);
    });
    test('Free rates price any session at $0', () => {
      const c = sessionCost(
        kinds({ in_tok: 5_000_000, cache_make: 5_000_000, cache_read: 5_000_000, out_tok: 5_000_000 }),
        resolveRates(rows.length + 1, rows, custom),
      );
      expect(c.total).toBe(0);
    });
  });

  describe('costGrandTotal (per-model rows sum)', () => {
    // A Fable-main + Opus-agents session: two model rows, priced independently.
    const opus: PriceRates = { input: 15, cache_write: 18.75, cache_read: 1.5, output: 75 };
    const fable: PriceRates = { input: 1, cache_write: 1.25, cache_read: 0.1, output: 5 };
    const perModel = [
      { model: 'vendor-opus-4-8', totals: kinds({ in_tok: 1_000_000, out_tok: 1_000_000 }) },
      { model: 'vendor-fable-2', totals: kinds({ in_tok: 2_000_000 }) },
    ];

    test('sums each model row at its own rates', () => {
      const total = costGrandTotal(perModel, (i) => (i === 0 ? opus : fable));
      // opus: 1M*15 + 1M*75 = 90 ; fable: 2M*1 = 2 ; grand = 92
      expect(total).toBe(92);
      // ...and the per-row math each row renders is just sessionCost:
      expect(sessionCost(perModel[0].totals, opus).total).toBe(90);
      expect(sessionCost(perModel[1].totals, fable).total).toBe(2);
    });

    test('a single-model session is just that row (Free → $0)', () => {
      expect(costGrandTotal([perModel[0]], () => FREE_RATES)).toBe(0);
    });

    test('no models → $0', () => {
      expect(costGrandTotal([], () => opus)).toBe(0);
    });
  });

  describe('originShareLine (lane share formatting)', () => {
    const TWO = [
      { id: 'session', label: 'main session' },
      { id: 'agent', label: 'sub-agents' },
    ];

    test('formats every declared lane with fmtTok, under the names it is given', () => {
      expect(originShareLine({ session: 12_300, agent: 4_100 }, TWO)).toBe(
        'main session 12k · sub-agents 4.1k tok',
      );
    });

    test('lanes with no declared name fall back to their ids, never to a guess', () => {
      // V40 Phase F: the wording is the harness's declared origins; a build
      // whose registry has not answered renders the bare lane ids rather than
      // one harness's phrasing under another's numbers.
      expect(
        originShareLine({ session: 12_300, agent: 4_100 }, [{ id: 'session' }, { id: 'agent' }]),
      ).toBe('session 12k · agent 4.1k tok');
    });

    test('a session with no fan-out still shows every declared lane', () => {
      expect(
        originShareLine({ session: 500 }, [
          { id: 'session', label: 'main' },
          { id: 'agent', label: 'subs' },
        ]),
      ).toBe('main 500 · subs 0 tok');
    });

    test('ONE lane, and THREE lanes, both print — the pre-G shape could do neither', () => {
      // `OriginSplit { session_tok, agent_tok }` was a closed pair: a harness
      // with no fan-out rendered a fabricated "sub-agents 0", and a harness
      // with a third lane had nowhere to put it.
      expect(originShareLine({ solo: 900 }, [{ id: 'solo', label: 'turns' }])).toBe('turns 900 tok');
      expect(
        originShareLine({ a: 1, b: 2, c: 3 }, [{ id: 'a' }, { id: 'b' }, { id: 'c' }]),
      ).toBe('a 1 · b 2 · c 3 tok');
    });

    test('no declared lanes ⇒ no line at all, rather than an empty "tok"', () => {
      expect(originShareLine({ session: 500 }, [])).toBe('');
    });
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

// ── V28: Overview dashboard donuts ─────────────────────────────────────

describe('originKindTotals', () => {
  const t = (
    origin: string,
    over: Partial<{ in_tok: number; cache_read: number; cache_make: number; out_tok: number }> = {},
  ) => ({ tokens: kinds(over), origin });

  test('splits category sums by lane', () => {
    const out = originKindTotals([
      t('session', { in_tok: 10, cache_read: 100, cache_make: 5, out_tok: 20 }),
      t('agent', { in_tok: 1, cache_read: 50, out_tok: 2 }),
      t('session', { in_tok: 3, out_tok: 4 }),
    ]);
    expect(out.session).toEqual({ input: 13, output: 24, cache_read: 100, cache_write: 5 });
    expect(out.agent).toEqual({ input: 1, output: 2, cache_read: 50, cache_write: 0 });
  });

  test('a declared lane with no turns keeps an (empty) entry so its legend row survives', () => {
    const out = originKindTotals([], ['session', 'agent']);
    expect(kindsTotal(out.session)).toBe(0);
    expect(kindsTotal(out.agent)).toBe(0);
    // Empty, not four zeros: nothing was reported for it.
    expect(out.session).toEqual({});
  });

  test('a lane present only in the DATA is still counted', () => {
    // The declaration is a poll away; a stored row must never be dropped
    // because it has not landed yet.
    const out = originKindTotals([t('review', { in_tok: 7 })], ['session']);
    expect(kindsTotal(out.review)).toBe(7);
    expect(Object.keys(out).sort()).toEqual(['review', 'session']);
  });
});

describe('kindsTotal', () => {
  test('sums every reported category (no tool estimate)', () => {
    expect(kindsTotal({ input: 1, output: 2, cache_read: 3, cache_write: 4 })).toBe(10);
  });

  test('a harness reporting one flat number totals that number', () => {
    expect(kindsTotal({ tokens: 42 })).toBe(42);
    expect(kindsTotal({})).toBe(0);
  });
});

// ── V40 Phase G: the declared-shape refactor moves NO rendered number ───────
//
// The whole risk of turning four fixed token fields into a declared-category
// map, and two fixed lanes into declared ids, is that a figure the dashboard
// prints today quietly changes. This block is the fixture proof it does not:
// one realistic two-lane, two-model, four-category session, fed through the
// NEW math, asserted against the numbers the OLD shape produced — computed
// here by hand from the same inputs, not by calling the new code twice.

describe('V40 Phase G parity: a two-lane four-category session renders unchanged', () => {
  // A `SessionUsageDetail`-shaped payload for a harness declaring the four
  // pricing categories and the two lanes (`session` false / `agent` true) —
  // i.e. exactly what the backend now answers for a session of the first
  // registered harness.
  const LANES = [
    { id: 'session', label: 'main session', subagent: false },
    { id: 'agent', label: 'sub-agents', subagent: true },
  ];
  const SUBAGENT_LANES = LANES.filter((l) => l.subagent).map((l) => l.id);

  // Six turns: four main-session, two sidechain, two models.
  const turns = [
    { origin: 'session', tokens: kinds({ in_tok: 1200, cache_read: 18_000, cache_make: 900, out_tok: 350 }), tool_chars: 4000 },
    { origin: 'session', tokens: kinds({ in_tok: 800, cache_read: 22_000, cache_make: 0, out_tok: 610 }), tool_chars: 0 },
    { origin: 'agent', tokens: kinds({ in_tok: 300, cache_read: 5_000, cache_make: 120, out_tok: 90 }), tool_chars: 250 },
    { origin: 'agent', tokens: kinds({ in_tok: 150, cache_read: 4_400, cache_make: 0, out_tok: 45 }), tool_chars: 0 },
    { origin: 'session', tokens: kinds({ in_tok: 640, cache_read: 26_500, cache_make: 1_500, out_tok: 1_020 }), tool_chars: 12_000 },
    { origin: 'session', tokens: kinds({ in_tok: 90, cache_read: 30_100, cache_make: 0, out_tok: 75 }), tool_chars: 0 },
  ];

  // The whole-session totals the Sessions row shows.
  const totals = kinds({ in_tok: 3180, cache_read: 106_000, cache_make: 2_520, out_tok: 2_190 });

  test('lane totals and their donut shares are the pre-G numbers', () => {
    const byLane = originKindTotals(turns, LANES.map((l) => l.id));
    // Hand-summed from the six turns above.
    expect(kindsTotal(byLane.session)).toBe(
      1200 + 18_000 + 900 + 350 + (800 + 22_000 + 0 + 610) + (640 + 26_500 + 1_500 + 1_020) + (90 + 30_100 + 0 + 75),
    );
    expect(kindsTotal(byLane.session)).toBe(103_785);
    expect(kindsTotal(byLane.agent)).toBe(300 + 5_000 + 120 + 90 + (150 + 4_400 + 0 + 45));
    expect(kindsTotal(byLane.agent)).toBe(10_105);

    // The outer ring: two arcs, in declared order, with the same shares the
    // closed `{ session_tok, agent_tok }` pair produced.
    const arcs = donutArcs(
      LANES.map((l) => ({ key: l.id, value: kindsTotal(byLane[l.id]) })),
      2 / 54,
    );
    expect(arcs.map((a) => a.key)).toEqual(['session', 'agent']);
    expect(arcs[0].value).toBe(103_785);
    expect(arcs[1].value).toBe(10_105);
    const grand = 103_785 + 10_105;
    expect(arcs[0].share).toBeCloseTo(103_785 / grand, 12);
    expect(arcs[1].share).toBeCloseTo(10_105 / grand, 12);
    expect(fmtPct(arcs[0].share)).toBe('91%');
    expect(fmtPct(arcs[1].share)).toBe('9%');
    expect(fmtTok(grand)).toBe('114k');
  });

  test('the inner ring keeps its per-lane category split, in the same order', () => {
    const byLane = originKindTotals(turns, LANES.map((l) => l.id));
    const KIND_ORDER = ['input', 'cache_read', 'cache_write', 'output'];
    const inner = donutArcs(
      LANES.flatMap((l) =>
        KIND_ORDER.map((k) => ({ key: `${l.id}:${k}`, value: byLane[l.id][k] ?? 0 })),
      ),
      2 / 35,
    );
    // `donutArcs` drops zero-valued segments, so this is the same set of arcs
    // the four-field shape produced for the same numbers.
    expect(inner.map((a) => a.key)).toEqual([
      'session:input',
      'session:cache_read',
      'session:cache_write',
      'session:output',
      'agent:input',
      'agent:cache_read',
      'agent:cache_write',
      'agent:output',
    ]);
    expect(inner.map((a) => a.value)).toEqual([
      2730, 96_600, 2_400, 2_055, 450, 9_400, 120, 135,
    ]);
    // Nested rings stay aligned: each lane's inner arcs span its outer arc.
    const outer = donutArcs(
      LANES.map((l) => ({ key: l.id, value: kindsTotal(byLane[l.id]) })),
      2 / 54,
    );
    expect(inner[3].a1).toBeLessThanOrEqual(outer[1].a0 + 1e-9);
    expect(inner[4].a0).toBeGreaterThanOrEqual(outer[0].a1 - 1e-9);
  });

  test('the S/A lane strip segments and badges are unchanged', () => {
    expect(laneSegments(turns)).toEqual([
      { origin: 'session', label: 'S', count: 2 },
      { origin: 'agent', label: 'A', count: 2 },
      { origin: 'session', label: 'S', count: 2 },
    ]);
    expect(turns.map((t) => agentBarClass(t.origin, SUBAGENT_LANES))).toEqual([
      '', '', 'agent', 'agent', '', '',
    ]);
  });

  test('bar heights and the chart denominator are unchanged', () => {
    // turnTotal = every category + round(tool_chars / 4), exactly as before.
    expect(turns.map(turnTotal)).toEqual([
      1200 + 18_000 + 900 + 350 + 1000,
      800 + 22_000 + 610,
      300 + 5_000 + 120 + 90 + 63,
      150 + 4_400 + 45,
      640 + 26_500 + 1_500 + 1_020 + 3_000,
      90 + 30_100 + 75,
    ]);
    expect(maxTurnTotal(turns)).toBe(32_660);
    expect(barHeightPct(turnTotal(turns[0]), maxTurnTotal(turns))).toBeCloseTo(
      (21_450 / 32_660) * 100,
      12,
    );
  });

  test('the Sessions row: totals, cache-hit and cost are the same figures', () => {
    const rates: PriceRates = { input: 15, cache_write: 18.75, cache_read: 1.5, output: 75 };
    // The four cells the row prints.
    expect(fmtTok(totals.input)).toBe('3.2k');
    expect(fmtTok(totals.cache_write)).toBe('2.5k');
    expect(fmtTok(totals.cache_read)).toBe('106k');
    expect(fmtTok(totals.output)).toBe('2.2k');
    // cache_read / (cache_read + input) — the pre-G formula, same inputs.
    expect(cacheHitRatio(totals)).toBeCloseTo(106_000 / (106_000 + 3_180), 12);
    expect(Math.round((cacheHitRatio(totals) ?? 0) * 100)).toBe(97);
    // Cost: the direct key lookup bills each category at the rate the
    // hand-written `in_tok → input, cache_make → cache_write` mapping used to.
    const c = sessionCost(totals, rates);
    expect(c.input).toBeCloseTo((3_180 / 1e6) * 15, 12);
    expect(c.cache_write).toBeCloseTo((2_520 / 1e6) * 18.75, 12);
    expect(c.cache_read).toBeCloseTo((106_000 / 1e6) * 1.5, 12);
    expect(c.output).toBeCloseTo((2_190 / 1e6) * 75, 12);
    expect(c.total).toBeCloseTo(0.0477 + 0.04725 + 0.159 + 0.16425, 9);
    expect(fmtUsd(c.total)).toBe('$0.4182');
  });

  test('the Cost card: per-model rows, their lane-share lines and the grand total', () => {
    const opus: PriceRates = { input: 15, cache_write: 18.75, cache_read: 1.5, output: 75 };
    const haiku: PriceRates = { input: 1, cache_write: 1.25, cache_read: 0.1, output: 5 };
    // What `usage_session_model_totals` now answers: `totals` a declared-
    // category map, `origins` a per-lane id map.
    const perModel = [
      {
        model: 'vendor-opus-4-8',
        totals: kinds({ in_tok: 2_730, cache_read: 96_600, cache_make: 2_400, out_tok: 2_055 }),
        origins: { session: 103_785 } as Record<string, number>,
      },
      {
        model: 'vendor-haiku-4-5',
        totals: kinds({ in_tok: 450, cache_read: 9_400, cache_make: 120, out_tok: 135 }),
        origins: { agent: 10_105 } as Record<string, number>,
      },
    ];
    expect(originShareLine(perModel[0].origins, LANES)).toBe(
      'main session 104k · sub-agents 0 tok',
    );
    expect(originShareLine(perModel[1].origins, LANES)).toBe('main session 0 · sub-agents 10k tok');
    const grand = costGrandTotal(perModel, (i) => (i === 0 ? opus : haiku));
    expect(grand).toBeCloseTo(
      sessionCost(perModel[0].totals, opus).total + sessionCost(perModel[1].totals, haiku).total,
      12,
    );
    expect(fmtUsd(grand)).toBe('$0.3872');
  });
});

describe('donutArcs', () => {
  const TAU = 2 * Math.PI;

  test('spans are proportional and boundaries sit at cumulative shares', () => {
    const arcs = donutArcs(
      [
        { key: 'a', value: 3 },
        { key: 'b', value: 1 },
      ],
      0.04,
    );
    expect(arcs).toHaveLength(2);
    expect(arcs[0].share).toBeCloseTo(0.75);
    expect(arcs[1].share).toBeCloseTo(0.25);
    // Drawn angles are the cumulative boundaries inset by gap/2 each side.
    expect(arcs[0].a0).toBeCloseTo(0.02);
    expect(arcs[0].a1).toBeCloseTo(0.75 * TAU - 0.02);
    expect(arcs[1].a0).toBeCloseTo(0.75 * TAU + 0.02);
    expect(arcs[1].a1).toBeCloseTo(TAU - 0.02);
  });

  test('zero and negative values are dropped, shares stay of the kept total', () => {
    const arcs = donutArcs(
      [
        { key: 'a', value: 0 },
        { key: 'b', value: 2 },
        { key: 'c', value: -5 },
        { key: 'd', value: 2 },
      ],
      0,
    );
    expect(arcs.map((a) => a.key)).toEqual(['b', 'd']);
    expect(arcs[0].share).toBeCloseTo(0.5);
  });

  test('a single nonzero value takes the full circle with no gap', () => {
    const arcs = donutArcs([{ key: 'only', value: 7 }], 0.1);
    expect(arcs).toHaveLength(1);
    expect(arcs[0].a0).toBe(0);
    expect(arcs[0].a1).toBeCloseTo(TAU);
  });

  test('all-zero input yields no arcs (placeholder, never a fabricated ring)', () => {
    expect(donutArcs([{ key: 'a', value: 0 }], 0.04)).toEqual([]);
    expect(donutArcs([], 0.04)).toEqual([]);
  });

  test('a sliver keeps ≥40% of its span (gap clamp never inverts it)', () => {
    const arcs = donutArcs(
      [
        { key: 'big', value: 999 },
        { key: 'tiny', value: 1 },
      ],
      0.1,
    );
    const tiny = arcs[1];
    const span = (1 / 1000) * TAU;
    expect(tiny.a1).toBeGreaterThan(tiny.a0);
    expect(tiny.a1 - tiny.a0).toBeCloseTo(span * 0.4);
  });

  test('non-finite values are dropped, not propagated into angles', () => {
    const arcs = donutArcs(
      [
        { key: 'a', value: Number.NaN },
        { key: 'b', value: 4 },
      ],
      0,
    );
    expect(arcs.map((a) => a.key)).toEqual(['b']);
    expect(arcs[0].a1).toBeCloseTo(TAU);
  });
});

describe('arcPath', () => {
  test('a quarter arc starts at 12 o\'clock and ends at 3 o\'clock', () => {
    const d = arcPath(0, 0, 10, 5, 0, Math.PI / 2);
    // Start point: top of the outer radius (0, -10); the L lands on the
    // inner radius at 3 o'clock (5, 0).
    expect(d.startsWith('M 0.000 -10.000 ')).toBe(true);
    expect(d).toContain('L 5.000 0.000');
    expect(d.endsWith('Z')).toBe(true);
    // Minor arc → large-arc flag 0, outer sweep clockwise (1).
    expect(d).toContain('A 10 10 0 0 1');
    expect(d).toContain('A 5 5 0 0 0');
  });

  test('a span past half the circle sets the large-arc flag', () => {
    const d = arcPath(0, 0, 10, 5, 0, 1.5 * Math.PI);
    expect(d).toContain('A 10 10 0 1 1');
  });

  test('a full circle renders as a two-subpath ring, not a degenerate arc', () => {
    const d = arcPath(66, 66, 62, 46, 0, 2 * Math.PI);
    // Two closed subpaths (outer + inner), four arc commands total.
    expect(d.match(/Z/g)).toHaveLength(2);
    expect(d.match(/A /g)).toHaveLength(4);
  });

  test('zero or negative span renders nothing', () => {
    expect(arcPath(0, 0, 10, 5, 1, 1)).toBe('');
    expect(arcPath(0, 0, 10, 5, 1, 0.5)).toBe('');
  });
});

describe('fmtPct', () => {
  test('whole percents in the middle of the range', () => {
    expect(fmtPct(0.5)).toBe('50%');
    expect(fmtPct(0.334)).toBe('33%');
  });

  test('honest edges: slivers and near-totals never round to 0%/100%', () => {
    expect(fmtPct(0.004)).toBe('<1%');
    expect(fmtPct(0.996)).toBe('>99%');
    expect(fmtPct(0)).toBe('0%');
    expect(fmtPct(1)).toBe('100%');
  });

  test('non-finite guards to 0%', () => {
    expect(fmtPct(Number.NaN)).toBe('0%');
  });
});
