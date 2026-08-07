import { describe, it, expect } from 'vitest';
import {
  reducedFeaturesFor,
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
