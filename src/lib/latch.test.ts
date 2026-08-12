import { describe, it, expect } from 'vitest';
import {
  reducedFeaturesFor,
  isReducedRow,
  reducedSummary,
  reducedCounts,
  reducedTabLine,
  featureStateWord,
  injectionChipState,
  recordPoll,
  recordSignatureRead,
  SIGNATURE_UNREAD,
  HEALTHY_POLL,
  UNKNOWN_AFTER_FAILURES,
  isTainted,
  taintColor,
  withSignatureHealth,
  SIGNATURE_RULES_FEATURE,
  type FeatureState,
  type InjectionStatus,
} from './latch';
import type { RulesHealth } from './offload';
import type { TabId } from './tabs/types';

/// The three readings the backend can hand us, named (#48, M-25).
///
/// `PARTIAL` is the finding: a rules directory of four files where three fail
/// to compile is `armed` — one file loaded, rules > 0, `scan` matches with a
/// quarter of the signatures — and NOT `healthy`. Branching on `armed` rendered
/// it as full protection.
const LIVE: RulesHealth = { armed: true, healthy: true, files_failed: 0 };
const PARTIAL: RulesHealth = { armed: true, healthy: false, files_failed: 3 };
const INERT: RulesHealth = { armed: false, healthy: false, files_failed: 0 };

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
      INERT,
    );
    expect(reducedSummary(s)).toBe('1 control switched off, 1 layer switched on but inert');
    // …and on its own it never claims a switch was flipped.
    const only = withSignatureHealth(status([feature({ feature: 'detection' })]), INERT);
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
    const s = withSignatureHealth(status([detection()]), INERT);
    expect(s?.reduced).toBe(true);
    const rows = reducedFeaturesFor(s, TAB);
    expect(rows.map((f) => f.feature)).toEqual([SIGNATURE_RULES_FEATURE]);
    // It carries its own explanation: the three `decided_by` levels answer
    // "who flipped this switch", which is the wrong question here.
    expect(rows[0].reason).toContain('no usable rules');
  });

  /// #48, M-25 — THE REWRITTEN TEST.
  ///
  /// This read `withSignatureHealth(status([detection()]), { armed: true })` and
  /// was titled "says nothing when the layer is armed". It pinned the defect:
  /// `armed` is the weaker predicate ("can this match ANYTHING?") and the
  /// question these surfaces ask is `healthy` ("is the rule set on disk live?"),
  /// so the test asserted silence for a state that includes a rules directory
  /// three quarters of which failed to compile — tested, green, and rendering
  /// full protection over a quarter of the signatures.
  it('says nothing only when the WHOLE rule set on disk is live', () => {
    const s = withSignatureHealth(status([detection()]), LIVE);
    expect(s?.reduced).toBe(false);
    expect(reducedFeaturesFor(s, TAB)).toEqual([]);
    expect(injectionChipState(s, false).visible).toBe(false);
  });

  /// The finding itself, phrased as the user sees it.
  it('renders 3 of 4 rule files failing as REDUCED, not as full protection', () => {
    const s = withSignatureHealth(status([detection()]), PARTIAL);
    // Silence is what full protection looks like here, and this state is not it.
    expect(s?.reduced).toBe(true);
    expect(injectionChipState(s, false).visible).toBe(true);
    const rows = reducedFeaturesFor(s, TAB);
    expect(rows.map((f) => f.feature)).toEqual([SIGNATURE_RULES_FEATURE]);
    // Nothing anywhere may claim the layer is intact…
    expect(rows[0].effective).toBe(false);
    // …and the row says how much of it is missing, in files the user can open.
    expect(rows[0].reason).toContain('3 rule files failed to compile');
    // The disarmed row's sentence would be false of this one: it IS matching.
    expect(rows[0].reason).not.toContain('no usable rules');
  });

  it('says nothing for a scope where detection is switched off or does not apply', () => {
    for (const off of [detection({ effective: false }), detection({ in_scope: false })]) {
      for (const read of [INERT, PARTIAL]) {
        const s = withSignatureHealth(status([off]), read);
        expect(
          reducedFeaturesFor(s, TAB).some((f) => f.feature === SIGNATURE_RULES_FEATURE),
        ).toBe(false);
      }
    }
    // …and a scope that switched detection off is not "reduced" by a rules
    // directory it never reads, so the chip stays silent too.
    const s = withSignatureHealth(status([detection({ effective: false })]), INERT);
    expect(s?.reduced).toBe(false);
    expect(withSignatureHealth(status([detection({ effective: false })]), PARTIAL)?.reduced).toBe(
      false,
    );
  });

  it('passes the backend status through untouched before the first reading lands', () => {
    // `'pending'` is the old `null`'s LEGITIMATE half: the app may still be
    // starting and there is nothing to say yet. The other half — "the read
    // failed" — used to arrive here as the same value and is now `'unknown'`
    // below.
    const base = status([detection()]);
    expect(withSignatureHealth(base, 'pending')).toBe(base);
    expect(withSignatureHealth(null, INERT)).toBeNull();
    expect(withSignatureHealth(null, 'unknown')).toBeNull();
  });

  /// #48, H-10 — THE INVERTED TEST.
  ///
  /// This assertion used to read `expect(withSignatureHealth(base, null))
  /// .toBe(base)` and was titled "passes the backend status through untouched
  /// when detection status is unavailable". It pinned the defect's shape:
  /// `detectionStatus()` swallowed a failed IPC to `null`, `null` took the same
  /// branch as `armed: true`, and a detection command that was broken for the
  /// life of the process rendered the signature layer as fully protected —
  /// tested, green, and wrong.
  it('renders an UNREADABLE detection status as its own state, never as protected', () => {
    const base = status([detection()]);
    const s = withSignatureHealth(base, 'unknown');
    expect(s).not.toBe(base);
    // The surfaces must speak: silence is what "fully protected" looks like.
    expect(s?.reduced).toBe(true);
    const rows = reducedFeaturesFor(s, TAB);
    expect(rows.map((f) => f.feature)).toEqual([SIGNATURE_RULES_FEATURE]);
    expect(rows[0].unknown).toBe(true);
    expect(rows[0].reason).toContain('could not read');
    // Nothing anywhere may claim the layer is working.
    expect(rows[0].effective).toBe(false);
  });

  /// The other lie the fix must not tell. A change that made unknown merely
  /// `!== armed` would satisfy the test above by collapsing it into "switched
  /// off" — which sends the user to Settings to flip a switch that is already
  /// on, and states as fact something nobody read.
  it('keeps UNREADABLE distinguishable from NOT-ARMED, not just from armed', () => {
    const unread = withSignatureHealth(status([detection()]), 'unknown');
    const disarmed = withSignatureHealth(status([detection()]), INERT);
    const [u] = reducedFeaturesFor(unread, TAB);
    const [d] = reducedFeaturesFor(disarmed, TAB);

    expect(u.unknown).toBe(true);
    expect(d.unknown).toBe(false);
    expect(u.reason).not.toBe(d.reason);
    // The word each one wears in the popover, and the sentence on the tab.
    expect(featureStateWord(u)).toBe('unknown');
    expect(featureStateWord(d)).toBe('off');
    expect(reducedTabLine([u])).not.toBe(reducedTabLine([d]));
    // The counts the chip branches on: a different bucket, not a bigger one.
    expect(reducedCounts(unread)).toEqual({ switched: 0, inert: 0, unreadable: 1, partial: 0 });
    expect(reducedCounts(disarmed)).toEqual({ switched: 0, inert: 1, unreadable: 0, partial: 0 });
    expect(reducedSummary(unread)).toBe('1 layer whose state could not be read');
    expect(reducedSummary(disarmed)).toBe('1 layer switched on but inert');
    // …and the chip says two different words.
    expect(injectionChipState(unread, false).label).toBe('unverified');
    expect(injectionChipState(disarmed, false).label).toBe('reduced');
  });

  /// #48, M-25 — the same argument one state along. A fix that made "not
  /// healthy" reuse the disarmed row would satisfy the finding's test above by
  /// telling the user the layer is doing NOTHING when it is doing a quarter of
  /// it, and one that reused `unknown` would say nobody could read a state we
  /// read perfectly well. Three readings, three sentences, three buckets.
  it('keeps PARTLY-LIVE distinguishable from inert and from unreadable', () => {
    const partial = withSignatureHealth(status([detection()]), PARTIAL);
    const inert = withSignatureHealth(status([detection()]), INERT);
    const unread = withSignatureHealth(status([detection()]), 'unknown');
    const [p] = reducedFeaturesFor(partial, TAB);
    const [i] = reducedFeaturesFor(inert, TAB);
    const [u] = reducedFeaturesFor(unread, TAB);

    expect(p.partial).toBe(true);
    expect(p.unknown).toBe(false);
    expect(i.partial).toBe(false);
    expect(u.partial).toBe(false);
    expect(p.reason).not.toBe(i.reason);
    expect(p.reason).not.toBe(u.reason);

    // The word in the popover: not "off" (nobody switched it, and it is
    // matching), not "unknown" (we read it).
    expect(featureStateWord(p)).toBe('partial');
    // The sentence on the tab, which must not end in the word "off" either.
    expect(reducedTabLine([p])).toBe(
      'Running on only part of what it needs: Signature rules loaded.',
    );
    expect(reducedTabLine([p])).not.toContain(' off');
    expect(reducedTabLine([p])).not.toBe(reducedTabLine([i]));
    expect(reducedTabLine([p])).not.toBe(reducedTabLine([u]));

    // Its own bucket and its own phrase in the chip's tooltip.
    expect(reducedCounts(partial)).toEqual({ switched: 0, inert: 0, unreadable: 0, partial: 1 });
    expect(reducedSummary(partial)).toBe('1 layer only partly loaded');
    const chip = injectionChipState(partial, false);
    // A read fact about a real loss of coverage: the confident word, and never
    // `unverified` — nobody failed to read anything here.
    expect(chip.label).toBe('reduced');
    expect(chip.degraded).toBe(false);
    expect(chip.title).toContain('1 layer only partly loaded');
    expect(chip.title).not.toMatch(/\d+ controls? switched off/);
  });

  /// The mixed case: a switched-off control must not swallow the partial layer,
  /// and vice versa. Two facts, two clauses, one sentence.
  it('states a switched-off control and a partly-loaded layer as separate claims', () => {
    const s = withSignatureHealth(
      status([detection(), feature({ feature: 'taint_latch', effective: false })]),
      PARTIAL,
    );
    expect(reducedCounts(s)).toEqual({ switched: 1, inert: 0, unreadable: 0, partial: 1 });
    expect(reducedSummary(s)).toBe('1 control switched off, 1 layer only partly loaded');
    const chip = injectionChipState(s, false);
    expect(chip.label).toBe('reduced');
    // Nothing here is unread, so the chip stays a confident claim.
    expect(chip.degraded).toBe(false);
    // …and the tab tooltip keeps them in separate sentences.
    expect(reducedTabLine(reducedFeaturesFor(s, TAB))).toBe(
      'Injection protection reduced for this tab: Taint latch off. ' +
        'Running on only part of what it needs: Signature rules loaded.',
    );
  });

  /// The cross-module invariant from the brief: "off by configuration" is state
  /// 2 and belongs to the detection feature's OWN row. A scope that does not
  /// screen is not made uncertain by a status read it never consults — and
  /// conflating the two would be this finding's own family of bug.
  it('says nothing about scopes where detection is off or absent, unreadable or not', () => {
    for (const off of [detection({ effective: false }), detection({ in_scope: false })]) {
      const s = withSignatureHealth(status([off]), 'unknown');
      expect(reducedFeaturesFor(s, TAB).some((f) => f.feature === SIGNATURE_RULES_FEATURE)).toBe(
        false,
      );
      expect(s?.reduced).toBe(false);
      expect(injectionChipState(s, false).visible).toBe(false);
    }
  });
});

/// #48, H-10 — the detection read's own health accounting.
///
/// The failure had nowhere to go: `detectionStatus()` swallowed it, the
/// enclosing try still called `recordPoll(health, true)`, and the tick was
/// recorded as healthy while the layer it reports on was unreadable. This
/// reducer is where a failed read now lands, on the same tick, with the same
/// [`UNKNOWN_AFTER_FAILURES`] debounce the latch surface already uses.
describe('recordSignatureRead', () => {
  it('starts with nothing to say rather than with good news', () => {
    expect(SIGNATURE_UNREAD.rules).toBe('pending');
    expect(withSignatureHealth(status([]), SIGNATURE_UNREAD.rules)).toBeTruthy();
  });

  it('reports what it read — the whole verdict, not one field of it', () => {
    // #48/M-25: the reading travels as the backend's own `RulesHealth`, so a
    // consumer cannot branch on a field this side decided to keep.
    for (const read of [LIVE, PARTIAL, INERT]) {
      expect(recordSignatureRead(SIGNATURE_UNREAD, read).rules).toEqual(read);
    }
  });

  it('a transient failure keeps the last reading instead of alarming anyone', () => {
    let s = recordSignatureRead(SIGNATURE_UNREAD, LIVE);
    for (let i = 1; i < UNKNOWN_AFTER_FAILURES; i++) {
      s = recordSignatureRead(s, null);
      expect(s.rules).toEqual(LIVE);
      expect(s.health.failures).toBe(i);
    }
    // …and the Nth consecutive failure stops claiming.
    s = recordSignatureRead(s, null);
    expect(s.rules).toBe('unknown');
    expect(recordSignatureRead(s, null).rules).toBe('unknown');
  });

  it('holds a DISARMED or PARTIAL reading across the grace window too, not just a live one', () => {
    // The debounce must not be a back door to forgetting bad news either — and
    // a partly-loaded rule set is bad news the grace window used to be unable
    // to carry at all (#48, M-25).
    for (const bad of [INERT, PARTIAL]) {
      let s = recordSignatureRead(SIGNATURE_UNREAD, bad);
      for (let i = 1; i < UNKNOWN_AFTER_FAILURES; i++) {
        s = recordSignatureRead(s, null);
        expect(s.rules).toEqual(bad);
      }
    }
  });

  it('says nothing at all when it has never had a reading to hold', () => {
    // A cold start whose first ticks fail must not paint the app alarming for
    // one hiccup — but it must not stay quiet forever either.
    let s = SIGNATURE_UNREAD;
    for (let i = 1; i < UNKNOWN_AFTER_FAILURES; i++) {
      s = recordSignatureRead(s, null);
      expect(s.rules).toBe('pending');
    }
    expect(recordSignatureRead(s, null).rules).toBe('unknown');
  });

  it('recovers to the truth on the first successful read', () => {
    let s = SIGNATURE_UNREAD;
    for (let i = 0; i < UNKNOWN_AFTER_FAILURES + 4; i++) s = recordSignatureRead(s, null);
    expect(s.rules).toBe('unknown');
    // Whole rule set live again, and the surfaces go quiet.
    s = recordSignatureRead(s, LIVE);
    expect(s).toEqual({ health: HEALTHY_POLL, last: LIVE, rules: LIVE });
    expect(withSignatureHealth(status([feature({ feature: 'detection' })]), s.rules)?.reduced).toBe(
      false,
    );
    // …and recovery to a DISARMED or PARTLY-LOADED layer reports its row rather
    // than silence: the surfaces must not go quiet on the strength of having
    // been quiet before.
    for (const bad of [INERT, PARTIAL]) {
      const next = recordSignatureRead(s, bad);
      expect(next.rules).toEqual(bad);
      expect(
        withSignatureHealth(status([feature({ feature: 'detection' })]), next.rules)?.reduced,
      ).toBe(true);
    }
  });
});

/// #48, G-3 + H-10 — the status chip, as a value.
///
/// The chip is the one surface a user sees without opening anything, so each of
/// its words is asserted here rather than left to a `.svelte` file no harness
/// can render.
describe('injectionChipState', () => {
  const detection = (over: Partial<FeatureState> = {}) =>
    feature({ feature: 'detection', label: 'Injection detection', ...over });

  it('is silent only when everything is on AND we can see that it is', () => {
    const healthy = withSignatureHealth(status([detection()]), LIVE);
    const chip = injectionChipState(healthy, false);
    expect(chip.visible).toBe(false);
    expect(chip.degraded).toBe(false);
    // …and "everything is on" means the whole rule set, not merely a rule set
    // that can match something (#48, M-25).
    expect(injectionChipState(withSignatureHealth(status([detection()]), PARTIAL), false).visible)
      .toBe(true);
  });

  it('shows UNVERIFIED — not silence, not "reduced" — when only the read failed', () => {
    const chip = injectionChipState(withSignatureHealth(status([detection()]), 'unknown'), false);
    expect(chip.visible).toBe(true);
    expect(chip.label).toBe('unverified');
    expect(chip.degraded).toBe(true);
    expect(chip.title).toContain('cannot be verified');
    expect(chip.title).toContain('could not be read');
    // It must not tell the user a control was switched off — it explicitly
    // says the opposite, and sends them to Settings to LOOK, not to flip.
    expect(chip.title).toContain('It is not switched off');
    expect(chip.title).not.toMatch(/\d+ controls? switched off/);
  });

  it('says "reduced" when something really is off, and still admits the blind spot', () => {
    const s = withSignatureHealth(
      status([detection(), feature({ feature: 'taint_latch', effective: false })]),
      'unknown',
    );
    const chip = injectionChipState(s, false);
    expect(chip.label).toBe('reduced');
    expect(chip.degraded).toBe(true);
    expect(chip.title).toContain('1 control switched off');
    expect(chip.title).toContain('cannot be verified');
  });

  it('a blind hierarchy poll outranks everything — we cannot see any of it', () => {
    for (const s of [null, status([]), withSignatureHealth(status([detection()]), 'unknown')]) {
      const chip = injectionChipState(s, true);
      expect(chip).toMatchObject({ visible: true, label: 'unknown', degraded: true });
    }
  });

  it('renders the master switch as OFF rather than as one reduction among many', () => {
    const s: InjectionStatus = { ...status([detection()]), protection: false, reduced: true };
    expect(injectionChipState(s, false)).toMatchObject({
      visible: true,
      label: 'off',
      degraded: false,
    });
  });
});

/// #48, H-10 — the tab badge's tooltip and the popover's word for one row.
describe('reducedTabLine / featureStateWord', () => {
  const row = (over: Partial<FeatureState>) =>
    feature({ feature: SIGNATURE_RULES_FEATURE, label: 'Signature rules loaded', ...over });

  it('is empty for a tab with nothing to report', () => {
    expect(reducedTabLine([])).toBe('');
  });

  it('says "off" only of rows that were actually read as off', () => {
    const line = reducedTabLine([feature({ feature: 'taint_latch', label: 'Taint latch' })]);
    expect(line).toBe('Injection protection reduced for this tab: Taint latch off.');
  });

  it('never says "off" of a row whose state could not be read', () => {
    const line = reducedTabLine([row({ unknown: true })]);
    expect(line).toBe('cImp could not read the state of: Signature rules loaded.');
    expect(line).not.toContain(' off');
  });

  it('keeps the two claims in separate sentences when a tab has both', () => {
    const line = reducedTabLine([
      feature({ feature: 'taint_latch', label: 'Taint latch' }),
      row({ unknown: true }),
    ]);
    expect(line).toBe(
      'Injection protection reduced for this tab: Taint latch off. ' +
        'cImp could not read the state of: Signature rules loaded.',
    );
  });

  it('gives the popover the same word the tooltip uses', () => {
    expect(featureStateWord(row({ unknown: true }))).toBe('unknown');
    expect(featureStateWord(row({ unknown: false }))).toBe('off');
    // A backend row carries no `unknown` field at all.
    expect(featureStateWord(feature({}))).toBe('off');
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
      can_clear: false,
      awaiting_session_clear: false,
      local_by_user_flip: false,
    };
    expect(isTainted(undefined)).toBe(false);
    expect(isTainted(row)).toBe(false);
    expect(isTainted({ ...row, latch: 'external' })).toBe(true);
    expect(isTainted({ ...row, latch: 'local' })).toBe(true);
    // Contamination survives an override, so the badge must too.
    expect(isTainted({ ...row, contaminated: true })).toBe(true);
  });
});

describe('taintColor', () => {
  const row = {
    consumer: 'claude',
    tab: TAB,
    session: null,
    latch: 'open',
    contaminated: false,
    can_flip_local: false,
    can_unlatch: false,
    can_clear: false,
    awaiting_session_clear: false,
    local_by_user_flip: false,
  };
  const LATCHED = '#112233';
  const CONTAM = '#445566';

  it('is null exactly where the badge/frame must not render an event color', () => {
    expect(taintColor(undefined, LATCHED, CONTAM)).toBeNull();
    expect(taintColor(null, LATCHED, CONTAM)).toBeNull();
    // An open + uncontaminated row is not tainted — same predicate as the
    // badge's, through `isTainted`, so the two can't drift.
    expect(taintColor(row, LATCHED, CONTAM)).toBeNull();
  });

  it('picks the latched color for a latch, and contamination wins over it', () => {
    expect(taintColor({ ...row, latch: 'external' }, LATCHED, CONTAM)).toBe(LATCHED);
    expect(taintColor({ ...row, latch: 'local' }, LATCHED, CONTAM)).toBe(LATCHED);
    expect(taintColor({ ...row, contaminated: true }, LATCHED, CONTAM)).toBe(CONTAM);
    // Contamination outlives the latch — a latched AND contaminated row wears
    // the stronger color.
    expect(taintColor({ ...row, latch: 'external', contaminated: true }, LATCHED, CONTAM)).toBe(
      CONTAM,
    );
  });

  it('falls back to the historical badge colors on an invalid setting', () => {
    // A hand-edited settings.json must not blank the containment surfaces.
    expect(taintColor({ ...row, latch: 'external' }, 'not-a-color', CONTAM)).toBe('#fabd2f');
    expect(taintColor({ ...row, contaminated: true }, LATCHED, '')).toBe('#fb4934');
    // Shorthand hex is not what <input type=color> produces — rejected too.
    expect(taintColor({ ...row, contaminated: true }, LATCHED, '#f00')).toBe('#fb4934');
  });
});
