import { describe, it, expect } from 'vitest';
import {
  reducedFeaturesFor,
  isReducedRow,
  reducedSummary,
  recordPoll,
  HEALTHY_POLL,
  UNKNOWN_AFTER_FAILURES,
  isTainted,
  withSignatureHealth,
  SIGNATURE_RULES_FEATURE,
  type FeatureState,
  type InjectionStatus,
} from './latch';
import type { TabId } from './tabs/types';

/// V32 Phase G/H — the tab badge's "what is switched off here?" filter.
///
/// The Phase H case is the one worth a test: the OpenCode native gate ships OFF
/// (locked decision 17), so a fresh install must not raise the muted
/// reduced-protection badge on every tab. The rule lives in the backend's
/// `protection_reduced`; this side must apply the same one, using the
/// `default_on` the backend publishes rather than a second list of defaults.

function feature(over: Partial<FeatureState>): FeatureState {
  return {
    feature: 'taint_latch',
    label: 'Taint latch',
    effective: true,
    decided_by: 'feature',
    override_value: 'inherit',
    in_scope: true,
    default_on: true,
    spawn_baked: false,
    ...over,
  };
}

function status(features: FeatureState[]): InjectionStatus {
  return {
    protection: true,
    reduced: false,
    scopes: [{ scope: 'opencode', label: 'OpenCode', features }],
  };
}

const TAB = 'opencode' as TabId;

describe('reducedFeaturesFor', () => {
  it('reports a feature that ships on and is switched off', () => {
    const s = status([feature({ feature: 'taint_latch', effective: false })]);
    expect(reducedFeaturesFor(s, TAB).map((f) => f.feature)).toEqual(['taint_latch']);
  });

  it('ignores a default-off feature that is simply off (the Phase H baseline)', () => {
    const s = status([
      feature({ feature: 'opencode_native_gate', effective: false, default_on: false }),
    ]);
    expect(reducedFeaturesFor(s, TAB)).toEqual([]);
  });

  it('still ignores a default-off feature that is switched ON — that is more protection', () => {
    const s = status([
      feature({
        feature: 'opencode_native_gate',
        effective: true,
        default_on: false,
        decided_by: 'scope',
        override_value: 'on',
      }),
    ]);
    expect(reducedFeaturesFor(s, TAB)).toEqual([]);
  });

  it('ignores rows the scope does not have', () => {
    const s = status([feature({ feature: 'canary', effective: false, in_scope: false })]);
    expect(reducedFeaturesFor(s, TAB)).toEqual([]);
  });

  it('reports nothing for an unknown tab or a missing status', () => {
    expect(reducedFeaturesFor(null, TAB)).toEqual([]);
    expect(reducedFeaturesFor(status([]), 'claude' as TabId)).toEqual([]);
  });

  /// #48, G-2: the status chip counted `in_scope && !effective` and omitted the
  /// `default_on` clause, so the chip and the tab badge disagreed in the same
  /// viewport — the chip said "N controls switched off" while the badge beside
  /// it named one. The predicate is now exported once and called by both.
  it('is the predicate both surfaces call, so they cannot disagree', () => {
    const rows = [
      feature({ feature: 'taint_latch', effective: false }),
      feature({ feature: 'opencode_native_gate', effective: false, default_on: false }),
      feature({ feature: 'canary', effective: false, in_scope: false }),
      feature({ feature: 'spotlighting' }),
    ];
    expect(rows.filter(isReducedRow).map((f) => f.feature)).toEqual(['taint_latch']);
    expect(reducedFeaturesFor(status(rows), TAB)).toEqual(rows.filter(isReducedRow));
  });
});

/// #48, G-2 — the status chip's tooltip.
describe('reducedSummary', () => {
  it('counts distinct controls rather than (scope, feature) pairs', () => {
    // One app-wide flip lands on every scope's row. The user unticked ONE box.
    const off = feature({ feature: 'taint_latch', effective: false });
    const s: InjectionStatus = {
      protection: true,
      reduced: true,
      scopes: [
        { scope: 'offload-worker', label: 'Offload worker', features: [off] },
        { scope: 'claude', label: 'Claude', features: [off] },
        { scope: 'opencode', label: 'OpenCode', features: [off] },
      ],
    };
    expect(reducedSummary(s)).toBe('1 control switched off');
  });

  it('excludes the default-off Phase H gate, exactly as the badge does', () => {
    const s = status([
      feature({ feature: 'taint_latch', effective: false }),
      feature({ feature: 'opencode_native_gate', effective: false, default_on: false }),
    ]);
    expect(reducedSummary(s)).toBe('1 control switched off');
  });

  /// Spec residual (g): the synthetic signature-health row is a reduction, but
  /// nobody switched it off — counting it as "a control switched off" made the
  /// chip's sentence untrue.
  it('counts a row that carries its own reason separately from the switches', () => {
    const s = withSignatureHealth(
      status([
        feature({ feature: 'detection', label: 'Injection detection' }),
        feature({ feature: 'taint_latch', effective: false }),
      ]),
      { armed: false },
    );
    expect(reducedSummary(s)).toBe('1 control switched off, 1 layer switched on but inert');
    // …and on its own it never claims a switch was flipped.
    const only = withSignatureHealth(status([feature({ feature: 'detection' })]), {
      armed: false,
    });
    expect(reducedSummary(only)).toBe('1 layer switched on but inert');
  });

  it('still says something when the backend reports reduced and no row explains it', () => {
    expect(reducedSummary(null)).toBe('something is off');
    expect(reducedSummary(status([feature({})]))).toBe('something is off');
  });
});

/// #48, G-3 — the reduced-protection indicator must not fail silent.
///
/// Both poll `catch` blocks were empty, so a permanently failing
/// `injection_status` left the chip hidden and every tab badge absent: the app
/// rendered as fully protected, indefinitely, with a clean console. The doc
/// comment's intent ("an INDIVIDUAL failed poll is swallowed") is the right one
/// and is what this reducer finally lets the code express.
describe('recordPoll', () => {
  it('swallows an individual failure but not a permanent one', () => {
    let h = HEALTHY_POLL;
    for (let i = 1; i < UNKNOWN_AFTER_FAILURES; i++) {
      h = recordPoll(h, false);
      expect(h.unknown).toBe(false);
      expect(h.failures).toBe(i);
    }
    h = recordPoll(h, false);
    expect(h).toEqual({ failures: UNKNOWN_AFTER_FAILURES, unknown: true });
    // It stays unknown while it stays broken.
    expect(recordPoll(h, false).unknown).toBe(true);
  });

  it('clears on the first success — the state is "we cannot see", not "we saw something bad"', () => {
    let h = HEALTHY_POLL;
    for (let i = 0; i < UNKNOWN_AFTER_FAILURES + 5; i++) h = recordPoll(h, false);
    expect(h.unknown).toBe(true);
    expect(recordPoll(h, true)).toEqual(HEALTHY_POLL);
  });

  it('a single failure between successes never shows the user anything', () => {
    let h = HEALTHY_POLL;
    for (let i = 0; i < 20; i++) {
      h = recordPoll(recordPoll(h, false), true);
      expect(h.unknown).toBe(false);
    }
  });
});

/// V32 Phase C / #48 D-2 — a disarmed signature layer is reduced protection.
///
/// The finding: every reduced-protection surface is derived from settings
/// toggles, so a rules directory that compiled to nothing rendered FULL
/// protection while `scan` returned empty for every page. The layer being on
/// and doing nothing is the exact state decision 16's indicator exists for.
describe('withSignatureHealth', () => {
  const detection = (over: Partial<FeatureState> = {}) =>
    feature({ feature: 'detection', label: 'Injection detection', ...over });

  it('adds a reduced row where detection applies and is on, and raises `reduced`', () => {
    const s = withSignatureHealth(status([detection()]), { armed: false });
    expect(s?.reduced).toBe(true);
    const rows = reducedFeaturesFor(s, TAB);
    expect(rows.map((f) => f.feature)).toEqual([SIGNATURE_RULES_FEATURE]);
    // It carries its own explanation: the three `decided_by` levels answer
    // "who flipped this switch", which is the wrong question here.
    expect(rows[0].reason).toContain('no usable rules');
  });

  it('says nothing when the layer is armed', () => {
    const s = withSignatureHealth(status([detection()]), { armed: true });
    expect(s?.reduced).toBe(false);
    expect(reducedFeaturesFor(s, TAB)).toEqual([]);
  });

  it('says nothing for a scope where detection is switched off or does not apply', () => {
    for (const off of [detection({ effective: false }), detection({ in_scope: false })]) {
      const s = withSignatureHealth(status([off]), { armed: false });
      expect(
        reducedFeaturesFor(s, TAB).some((f) => f.feature === SIGNATURE_RULES_FEATURE),
      ).toBe(false);
    }
    // …and a scope that switched detection off is not "reduced" by a rules
    // directory it never reads, so the chip stays silent too.
    const s = withSignatureHealth(status([detection({ effective: false })]), { armed: false });
    expect(s?.reduced).toBe(false);
  });

  it('passes the backend status through untouched when detection status is unavailable', () => {
    const base = status([detection()]);
    expect(withSignatureHealth(base, null)).toBe(base);
    expect(withSignatureHealth(null, { armed: false })).toBeNull();
  });
});

describe('isTainted', () => {
  it('is true when latched either way or contaminated, false for a clean row', () => {
    const row = {
      consumer: 'opencode',
      tab: TAB,
      session: null,
      latch: 'open',
      contaminated: false,
      can_flip_local: false,
      can_unlatch: false,
    };
    expect(isTainted(undefined)).toBe(false);
    expect(isTainted(row)).toBe(false);
    expect(isTainted({ ...row, latch: 'external' })).toBe(true);
    expect(isTainted({ ...row, latch: 'local' })).toBe(true);
    // Contamination survives an override, so the badge must too.
    expect(isTainted({ ...row, contaminated: true })).toBe(true);
  });
});
