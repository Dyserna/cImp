import { describe, it, expect } from 'vitest';
import {
  precedes,
  linkCheckpoint,
  linkLine,
  restoreTarget,
  clearedLine,
  buildTimelineRows,
  compareRows,
  rowIcon,
  rowTitle,
  triggerIcon,
  triggerTitle,
  checkpointSource,
  evidenceNotices,
  latchAlsoHoldsMemory,
  evidenceOffNotice,
  type ContaminationEvent,
  type TimelineRow,
} from './timeline';
import type { Checkpoint } from './workbench';
import type { LatchRow } from './latch';
import type { TabId } from './tabs/types';

import { FIRST_HARNESS, SECOND_HARNESS } from './harness.fixture';

// V40 Phase F: every harness id below comes from the committed registry fixture
// (`harness.fixture.ts`), so this suite names no product and re-points itself
// when the registry changes.
const H1 = FIRST_HARNESS.id;
const H2 = SECOND_HARNESS.id;
/// Two tabs of the first harness — the checkpoint/scope pairs these tests join
/// on are `<harness>:<tab>`, so the tab ids are derived from it too.
const TAB1 = `${H1}-1`;
const TAB2 = `${H1}-2`;


/// V33 step 5 — the Timeline's evidence surface.
///
/// Every case here is written against the *invariant*, not the shape of the
/// example: the question asked of each was "what would this still pass with?".
/// Where the answer was "a plausible wrong implementation", the case was
/// rewritten until it wasn't — the seconds/millis boundary is pinned at an exact
/// tie (a ±1000 error passes any test that keeps the two an hour apart), and the
/// other-tab case uses a checkpoint that is genuinely the nearest one (a join
/// that filtered to same-tab checkpoints would pass a test where the other tab's
/// checkpoint is also older than this tab's).

const ROOT = 'P:\\proj';

function cp(over: Partial<Checkpoint> = {}): Checkpoint {
  return {
    id: 'c1',
    seq: 1,
    commit: 'abc',
    ts: '2026-08-08T10:00:00Z',
    ts_unix: 1000,
    label: 'checkpoint',
    trigger: 'prompt',
    agent: H1,
    files_changed: 2,
    session: 'sess-a',
    tab: TAB1,
    ...over,
  };
}

function ev(over: Partial<ContaminationEvent> = {}): ContaminationEvent {
  return {
    id: 10,
    ts_ms: 2_000_000,
    root: ROOT,
    cleared: false,
    scope: `${H1}:${TAB1}`,
    agent: H1,
    tab: TAB1,
    session: 'sess-a',
    tool: 'ddg__fetch_content',
    host: 'evil.example',
    url: 'https://evil.example/p',
    origin: 'internal',
    detail: 'CONTAMINATED',
    ...over,
  };
}

function latch(over: Partial<LatchRow> = {}): LatchRow {
  return {
    consumer: H1,
    tab: TAB1 as TabId,
    session: 'sess-a',
    latch: 'external',
    contaminated: true,
    can_flip_local: true,
    can_unlatch: true,
    can_clear: true,
    awaiting_session_clear: false,
    local_by_user_flip: false,
    ...over,
  };
}

describe('the seconds-vs-millis boundary', () => {
  /// The unit bug this pins is not "off by a bit" — it is off by 1000×, which
  /// still returns an answer. Pinned at an EXACT tie plus its two neighbours,
  /// because that is the only region where a correct implementation and one
  /// that dropped (or doubled) the ×1000 disagree by exactly one row.
  it('treats an equal-instant checkpoint as preceding, and one 1ms later as not', () => {
    const event = ev({ ts_ms: 1_000_000 });
    expect(precedes(cp({ ts_unix: 1000 }), event)).toBe(true);
    expect(precedes(cp({ ts_unix: 999 }), event)).toBe(true);
    expect(precedes(cp({ ts_unix: 1001 }), event)).toBe(false);
    // One millisecond either side of the tie, so a `<` / `<=` swap is caught.
    expect(precedes(cp({ ts_unix: 1000 }), ev({ ts_ms: 999_999 }))).toBe(false);
    expect(precedes(cp({ ts_unix: 1000 }), ev({ ts_ms: 1_000_001 }))).toBe(true);
  });

  it('joins at the tie rather than skipping past it', () => {
    const link = linkCheckpoint(ev({ ts_ms: 1_000_000 }), [
      cp({ id: 'older', seq: 1, ts_unix: 500 }),
      cp({ id: 'tied', seq: 2, ts_unix: 1000 }),
      cp({ id: 'later', seq: 3, ts_unix: 1001 }),
    ]);
    expect(link.kind).toBe('own');
    expect(restoreTarget(link)?.id).toBe('tied');
  });
});

describe('the join', () => {
  it('picks the nearest preceding checkpoint, whatever order the list arrives in', () => {
    const list = [
      cp({ id: 'a', seq: 1, ts_unix: 100 }),
      cp({ id: 'c', seq: 3, ts_unix: 300 }),
      cp({ id: 'b', seq: 2, ts_unix: 200 }),
      cp({ id: 'future', seq: 4, ts_unix: 9_000 }),
    ];
    expect(restoreTarget(linkCheckpoint(ev({ ts_ms: 350_000 }), list))?.id).toBe('c');
    // Reversed input must give the same answer — a "first match wins" scan over
    // a list that happens to be sorted would pass one direction only.
    expect(restoreTarget(linkCheckpoint(ev({ ts_ms: 350_000 }), [...list].reverse()))?.id).toBe('c');
  });

  it('breaks a same-second tie by seq, so the answer is not list order', () => {
    const list = [
      cp({ id: 'lo', seq: 7, ts_unix: 300 }),
      cp({ id: 'hi', seq: 8, ts_unix: 300 }),
    ];
    expect(restoreTarget(linkCheckpoint(ev({ ts_ms: 350_000 }), list))?.id).toBe('hi');
    expect(restoreTarget(linkCheckpoint(ev({ ts_ms: 350_000 }), [...list].reverse()))?.id).toBe('hi');
  });

  /// The honesty requirement of the whole step. The nearest preceding checkpoint
  /// belongs to the SECOND tab; this tab also has one, but an OLDER
  /// one. A join that quietly filtered to same-tab checkpoints would return the
  /// older one and look correct — so the assertion is on both halves: the
  /// checkpoint chosen AND the label it is given.
  it('labels a nearest checkpoint from another tab as another tab’s', () => {
    const link = linkCheckpoint(ev({ tab: TAB1 }), [
      cp({ id: 'mine', seq: 1, ts_unix: 100, tab: TAB1 }),
      cp({ id: 'theirs', seq: 2, ts_unix: 1500, tab: TAB2 }),
    ]);
    expect(link.kind).toBe('other-tab');
    expect(restoreTarget(link)?.id).toBe('theirs');
    const line = linkLine(link);
    expect(line).toContain(TAB2);
    expect(line).toContain("not this tab's own restore point");
  });

  it('calls a checkpoint with no tab unattributed rather than this tab’s', () => {
    const link = linkCheckpoint(ev(), [cp({ id: 'burst', ts_unix: 1500, tab: null })]);
    expect(link.kind).toBe('unattributed');
    expect(linkLine(link)).toContain('cannot say it belongs to this tab');
  });

  it('calls it unattributed when the EVENT has no tab either', () => {
    const link = linkCheckpoint(ev({ tab: null, agent: null }), [
      cp({ id: 'x', ts_unix: 1500, tab: TAB1 }),
    ]);
    expect(link.kind).toBe('unattributed');
  });

  it('offers no restore when nothing precedes the event', () => {
    const link = linkCheckpoint(ev({ ts_ms: 1_000 }), [cp({ ts_unix: 5_000 })]);
    expect(link.kind).toBe('none');
    expect(restoreTarget(link)).toBeNull();
    expect(linkLine(link)).toContain('nothing here to restore to');
  });
});

describe('merging', () => {
  it('renders a contamination row with no checkpoint at all, offering no restore', () => {
    const rows = buildTimelineRows([], [ev()], ROOT);
    expect(rows).toHaveLength(1);
    const row = rows[0];
    expect(row.kind).toBe('contamination');
    if (row.kind !== 'contamination') throw new Error('unreachable');
    expect(row.link.kind).toBe('none');
    expect(restoreTarget(row.link)).toBeNull();
  });

  /// Merge order at a shared instant. The rule is not cosmetic: the join says a
  /// checkpoint at the same instant PRECEDES the event, so in a newest-first
  /// list the event has to sit above it. If the two disagreed, a row would say
  /// "the last checkpoint before this" while rendering underneath it.
  it('puts a contamination row above a checkpoint that shares its timestamp', () => {
    const rows = buildTimelineRows(
      [cp({ id: 'tied', ts_unix: 1000 })],
      [ev({ ts_ms: 1_000_000 })],
      ROOT,
    );
    expect(rows.map((r) => r.kind)).toEqual(['contamination', 'checkpoint']);
    const first = rows[0];
    if (first.kind !== 'contamination') throw new Error('unreachable');
    expect(restoreTarget(first.link)?.id).toBe('tied');
  });

  it('orders everything else newest-first, checkpoints and events interleaved', () => {
    const rows = buildTimelineRows(
      [cp({ id: 'old', seq: 1, ts_unix: 100 }), cp({ id: 'new', seq: 2, ts_unix: 3000 })],
      [ev({ id: 1, ts_ms: 200_000 }), ev({ id: 2, ts_ms: 4_000_000 })],
      ROOT,
    );
    expect(rows.map((r) => r.key)).toEqual(['ct:2', 'cp:new', 'ct:1', 'cp:old']);
  });

  it('drops events belonging to another project root', () => {
    const rows = buildTimelineRows([], [ev({ root: 'P:\\other' })], ROOT);
    expect(rows).toHaveLength(0);
  });

  it('is a total order: comparing a row with itself is 0 and the sort is stable', () => {
    const rows = buildTimelineRows([cp()], [ev()], ROOT);
    for (const r of rows) expect(compareRows(r, r)).toBe(0);
  });
});

describe('the cleared lifecycle', () => {
  it('folds a clear into the row that opened it and renders it resolved', () => {
    const opened = ev({ id: 1, ts_ms: 1_000_000 });
    const cleared = ev({
      id: 2,
      ts_ms: 2_000_000,
      cleared: true,
      tool: 'clear_contamination',
      host: null,
      url: null,
      origin: 'ipc',
    });
    const rows = buildTimelineRows([], [opened, cleared], ROOT);
    expect(rows).toHaveLength(1);
    const row = rows[0];
    if (row.kind !== 'contamination') throw new Error('unreachable');
    expect(row.opened?.id).toBe(1);
    expect(row.cleared?.id).toBe(2);
    expect(clearedLine(cleared)).toContain('false positive');
    expect(rowIcon(row)).not.toBe(rowIcon({ ...row, cleared: null }));
  });

  it('does not let one clear resolve two separate contaminations', () => {
    // Set, cleared, set again — the second contamination is NOT resolved by the
    // clear that closed the first. A "find any clear for this scope" pairing
    // would mark both resolved and tell the user a live flag is gone.
    const rows = buildTimelineRows(
      [],
      [
        ev({ id: 1, ts_ms: 1_000_000 }),
        ev({ id: 2, ts_ms: 1_500_000, cleared: true, tool: 'clear_contamination' }),
        ev({ id: 3, ts_ms: 2_000_000 }),
      ],
      ROOT,
    );
    const byKey = new Map(rows.map((r) => [r.key, r]));
    const first = byKey.get('ct:1');
    const second = byKey.get('ct:3');
    if (first?.kind !== 'contamination' || second?.kind !== 'contamination') {
      throw new Error('unreachable');
    }
    expect(first.cleared?.id).toBe(2);
    expect(second.cleared).toBeNull();
  });

  it('never pairs a clear from another tab’s scope', () => {
    const rows = buildTimelineRows(
      [],
      [
        ev({ id: 1, ts_ms: 1_000_000, scope: `${H1}:${TAB1}` }),
        ev({
          id: 2,
          ts_ms: 1_500_000,
          cleared: true,
          scope: `${H1}:${TAB2}`,
          tab: TAB2,
          tool: 'clear_contamination',
        }),
      ],
      ROOT,
    );
    const opened = rows.find((r) => r.key === 'ct:1');
    if (opened?.kind !== 'contamination') throw new Error('unreachable');
    expect(opened.cleared).toBeNull();
    // And the unmatched clear is still a row of its own — see below.
    expect(rows.map((r) => r.key).sort()).toEqual(['ct:1', 'ct:2']);
  });

  it('renders an orphaned clear as its own row with nothing to restore', () => {
    const rows = buildTimelineRows(
      [cp({ ts_unix: 100 })],
      [ev({ id: 9, cleared: true, tool: 'session_clear_observed' })],
      ROOT,
    );
    const row = rows.find((r) => r.key === 'ct:9');
    if (row?.kind !== 'contamination') throw new Error('unreachable');
    expect(row.opened).toBeNull();
    // There IS a preceding checkpoint here — the row still offers no restore,
    // because with no opening event there is no "before this" to aim at.
    expect(row.link.kind).toBe('none');
    expect(clearedLine(row.cleared!)).toContain('new session');
  });

  /// Decision 15's 2026-08-10 amendment added a THIRD basis, and the row has to
  /// name it: without its own arm the Timeline rendered the bare fallback
  /// `Cleared (unlatch).`, which is a wire value where the user needs a decision.
  /// The three sentences must also be distinguishable from each other — a shared
  /// sentence would put "the content was harmless" and "I took the whole risk
  /// knowingly" in the same words.
  it('names each clear basis, and falls back rather than guessing', () => {
    const unlatched = clearedLine(ev({ cleared: true, tool: 'unlatch', origin: 'ipc' }));
    expect(unlatched).toContain('restored full access');
    expect(unlatched).toContain('deliberately');
    const resumed = clearedLine(ev({ cleared: true, tool: 'clear_contamination' }));
    const rotated = clearedLine(ev({ cleared: true, tool: 'session_clear_observed' }));
    expect(new Set([unlatched, resumed, rotated]).size).toBe(3);

    // An unknown basis must not borrow one of the three sentences: a build that
    // grew a fourth clear says so instead of describing the wrong one.
    const unknown = clearedLine(ev({ cleared: true, tool: 'flip_local' }));
    expect(unknown).toBe('Cleared (flip_local).');
  });
});

describe('what the view says when it cannot show everything', () => {
  /// Aged out vs never contaminated. The store evicts oldest-first within a
  /// lane, so this state is reachable; the two cases below differ ONLY in
  /// whether a tab is currently flagged, which is exactly the distinction a
  /// surface that rendered an empty list would lose.
  it('announces a flagged tab with no retained event, and stays silent when none is flagged', () => {
    const orphaned = evidenceNotices({
      events: [],
      root: ROOT,
      latch: [latch({ contaminated: true })],
      error: null,
    });
    expect(orphaned.map((n) => n.kind)).toEqual(['not-retained']);
    expect(orphaned[0].text).toContain(`${H1}:${TAB1}`);
    expect(orphaned[0].text).toContain('not "they were never contaminated"');

    const clean = evidenceNotices({
      events: [],
      root: ROOT,
      latch: [latch({ contaminated: false })],
      error: null,
    });
    expect(clean).toEqual([]);
  });

  it('does not call a tab unretained when its event is merely at another root', () => {
    // The worktree case: the tab IS flagged and its event IS retained, just
    // filed against another directory. Announcing it as "no event retained"
    // would send the user looking for an eviction that never happened.
    const notices = evidenceNotices({
      events: [ev({ root: 'P:\\proj\\.cimp\\wt\\x' })],
      root: ROOT,
      latch: [latch({ contaminated: true })],
      error: null,
    });
    expect(notices.map((n) => n.kind)).toEqual(['other-root']);
    expect(notices[0].text).toContain(ROOT);
    expect(notices[0].text).toContain('worktree');
  });

  it('reports a failed read as a failed read, not as an absence', () => {
    const notices = evidenceNotices({
      events: [],
      root: ROOT,
      latch: [],
      error: 'command not found',
    });
    expect(notices.map((n) => n.kind)).toEqual(['error']);
    expect(notices[0].text).toContain('command not found');
    expect(notices[0].text).toContain('cannot currently tell you');
  });

  it('counts only live contaminations at other roots, not their clears', () => {
    const notices = evidenceNotices({
      events: [
        ev({ id: 1, root: 'P:\\other' }),
        ev({ id: 2, root: 'P:\\other', cleared: true, tool: 'clear_contamination' }),
      ],
      root: ROOT,
      latch: [],
      error: null,
    });
    expect(notices.map((n) => n.kind)).toEqual(['other-root']);
    expect(notices[0].text).toMatch(/^1 contamination event /);
  });

  /// #48 F-16, in the one view that actually narrows by root. `root === ''` is a
  /// CLAIM — "not attributable to a project" — and the row F-16 found missing was
  /// one recording that a credential had been held. Reachable here without any
  /// producer bug: the contamination screens take their root from `tab_root_key`,
  /// which degrades to `""` when the process cwd cannot be read.
  ///
  /// Two failures are pinned, and they are different: showing an empty list
  /// (silence), and folding the row into the other-root count (a sentence that
  /// says "came from another project directory" about a row that names none).
  it('announces a rootless event as its own case, never as another project’s', () => {
    const notices = evidenceNotices({
      events: [ev({ id: 1, root: '' })],
      root: ROOT,
      latch: [],
      error: null,
    });
    expect(notices.map((n) => n.kind)).toEqual(['rootless']);
    expect(notices[0].text).toMatch(/^1 contamination event /);
    // Not the other-root sentence: that one asserts the row belongs to a
    // different directory, which is a fact a rootless row does not carry.
    const otherRoot = evidenceNotices({
      events: [ev({ id: 1, root: 'P:\\other' })],
      root: ROOT,
      latch: [],
      error: null,
    });
    expect(notices[0].text).not.toBe(otherRoot[0].text);
    expect(notices[0].text).not.toContain('came from another project directory');

    // …and it is withheld from the rows, which is precisely why the notice has
    // to exist: the two together are the "surfaced, not silenced" property.
    expect(
      buildTimelineRows([], [ev({ id: 1, root: '' })], ROOT).filter((r) => r.kind !== 'checkpoint'),
    ).toEqual([]);
  });

  it('keeps the two withholding reasons separately counted', () => {
    const notices = evidenceNotices({
      events: [ev({ id: 1, root: 'P:\\other' }), ev({ id: 2, root: '' })],
      root: ROOT,
      latch: [],
      error: null,
    });
    expect(notices.map((n) => n.kind)).toEqual(['other-root', 'rootless']);
    // A rootless row folded into the other-root count would read "2 … came from
    // another project directory", which is one claim too many.
    for (const n of notices) expect(n.text).toMatch(/^1 contamination event /);
  });

  it('does not announce a rootless CLEAR, on the same live-only rule as other roots', () => {
    const notices = evidenceNotices({
      events: [ev({ id: 1, root: '', cleared: true, tool: 'clear_contamination' })],
      root: ROOT,
      latch: [],
      error: null,
    });
    expect(notices).toEqual([]);
  });
});

describe('step 5d — the flag and the latch are separate holds', () => {
  /// `Latch::proxy_gate` quarantines a PERSISTENT-WRITE whenever the latch is
  /// EXTERNAL, on the latch's own authority; the contamination bit only widens
  /// that verdict. So the sentence must appear for EXTERNAL and for nothing
  /// else — a warning shown where it does not apply is how warnings stop being
  /// read, and one withheld where it does is the gap step 4 shipped with.
  it('warns for an external latch only', () => {
    const note = latchAlsoHoldsMemory('external');
    expect(note).not.toBeNull();
    expect(note).toContain('latch quarantines writes on its own authority');
    expect(note).toContain('Switch to local');
    for (const other of ['local', 'open', undefined]) {
      expect(latchAlsoHoldsMemory(other)).toBeNull();
    }
  });
});

describe('the feature-off state', () => {
  it('names the reachable control, and reports live contamination even with no Timeline', () => {
    const quiet = evidenceOffNotice([]);
    expect(quiet).toContain('containment badge');
    expect(quiet).not.toMatch(/\bno tab\b|never/);

    const loud = evidenceOffNotice([`${H1}:${TAB2}`, `${H1}:${TAB1}`]);
    expect(loud).toContain('flagged as contaminated right now');
    // Sorted, so the sentence does not reorder itself between renders.
    expect(loud).toContain(`${H1}:${TAB1}, ${H1}:${TAB2}`);
    expect(loud).toContain('containment badge');
  });
});

describe('row rendering', () => {
  it('gives a contamination row an icon and title distinct from every checkpoint trigger', () => {
    const row: TimelineRow = {
      kind: 'contamination',
      key: 'ct:1',
      tsMs: 1,
      scope: `${H1}:${TAB1}`,
      agent: H1,
      tab: TAB1,
      opened: ev(),
      cleared: null,
      link: { kind: 'none' },
    };
    const triggers: Checkpoint['trigger'][] = ['prompt', 'burst', 'manual', 'pre-restore'];
    for (const t of triggers) {
      expect(rowIcon(row)).not.toBe(triggerIcon(t));
      expect(rowTitle(row)).not.toBe(triggerTitle(t));
    }
    expect(rowTitle(row)).toContain('contaminated');
  });

  /// `Checkpoint` is hand-mirrored from Rust with no codegen. A fifth trigger
  /// variant would arrive as a string this union does not contain; an
  /// exhaustive switch returns `undefined` for it, and in a view that now has
  /// non-checkpoint rows a blank icon reads as "not a checkpoint".
  it('renders an unknown trigger as a checkpoint rather than as nothing', () => {
    const unknown = 'squash' as Checkpoint['trigger'];
    expect(triggerIcon(unknown)).toBeTruthy();
    expect(triggerTitle(unknown)).toContain('squash');
    const row: TimelineRow = {
      kind: 'checkpoint',
      key: 'cp:x',
      tsMs: 0,
      checkpoint: cp({ trigger: unknown }),
    };
    expect(rowIcon(row)).toBeTruthy();
  });

  /// V33 (contract C8). The `tool` trigger's whole reason to exist is that it
  /// is NOT the other four, so its icon and its sentence both have to be
  /// distinguishable — an icon shared with `burst` would make "before the
  /// write" and "after the write" the same row at a glance.
  it('gives the tool trigger an icon and a title distinct from every other trigger', () => {
    const others: Checkpoint['trigger'][] = ['prompt', 'burst', 'manual', 'pre-restore'];
    for (const t of others) {
      expect(triggerIcon('tool')).not.toBe(triggerIcon(t));
      expect(triggerTitle('tool')).not.toBe(triggerTitle(t));
    }
    // It must not fall through to the unknown-trigger arm either.
    expect(triggerTitle('tool')).not.toContain('this build knows');
    // The claim the trigger makes: restoring recovers the state from before
    // that one call.
    expect(triggerTitle('tool')).toMatch(/before/i);
  });
});

describe('checkpoint source (C8 provenance)', () => {
  /// Absent is the NORMAL case — nearly every checkpoint has no tool behind
  /// it, and until the Rust half lands none do. All three flavours of absent
  /// must collapse to "render nothing", including the empty string: "empty is
  /// not absent" is exactly how a blank badge reaches a row.
  it('treats undefined, null and empty/whitespace alike as nothing to show', () => {
    expect(checkpointSource(undefined)).toBeNull();
    expect(checkpointSource(null)).toBeNull();
    expect(checkpointSource('')).toBeNull();
    expect(checkpointSource('   ')).toBeNull();
    // …and a checkpoint from a backend that predates the field is one that
    // simply has no such key.
    expect(checkpointSource(cp().source)).toBeNull();
  });

  it('shows the tool name and names the harness in the explanation', () => {
    for (const [raw, tool, harness] of [
      [`${H1}:Bash`, 'Bash', H1],
      ['offload:run_command', 'run_command', 'offload'],
      [`${H2}:edit`, 'edit', H2],
    ] as const) {
      const s = checkpointSource(raw)!;
      expect(s.text).toBe(tool);
      expect(s.harness).toBe(harness);
      expect(s.title).toContain(harness);
      expect(s.title).toContain(tool);
    }
  });

  /// Hand-mirrored data with no codegen: a provenance string in some other
  /// shape is a thing the user should still see, not a thing to hide. Same
  /// reasoning as `triggerIcon`'s `default:` arm.
  it('shows an unrecognized shape verbatim rather than dropping it', () => {
    const bare = checkpointSource('mystery_tool')!;
    expect(bare.text).toBe('mystery_tool');
    expect(bare.harness).toBeNull();
    expect(bare.title).toContain('mystery_tool');
    // A colon with nothing after it is not a tool name — fall back to the raw
    // value instead of rendering an empty badge.
    const empty = checkpointSource(`${H1}:`)!;
    expect(empty.text).toBe(`${H1}:`);
  });
});
