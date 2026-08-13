// The Tool Activity feed's poll-merge. `mergeEntries` exists to keep rendered
// rows referentially stable across the 2s poll — a plain `entries = list`
// re-renders the whole (up to ~1.4k row) feed every tick, which is what showed
// up as hover lag once a second agent tab was filling the store.
//
// Its safety rests on ONE backend invariant: `crate::activity` assigns an id at
// record time and never rewrites an entry afterwards (append / delete / clear
// only), so an id already held identifies byte-identical content. These pin the
// reuse and the no-op cases so a regression in either is visible here rather
// than as a stale row on screen.

import { describe, it, expect } from 'vitest';
import {
  attributionId,
  attributionState,
  filterEntries,
  isTabAttribution,
  matchesTabFilter,
  mergeEntries,
  rowStatus,
  tabFilterValue,
  FILTER_ANY,
  NO_FILTER,
  STATUS_TITLE,
  TAB_FILTER_HEADLESS,
  TAB_FILTER_UNATTRIBUTED,
  TAB_FILTER_UNRECOGNIZED,
  type ActivityEntry,
  type Attribution,
  type RowStatus,
} from './activity';

function entry(id: number, over: Partial<ActivityEntry> = {}): ActivityEntry {
  return {
    id,
    ts_ms: 1_000 + id,
    kind: 'graph',
    root: '/p',
    source: 'claude',
    tool: 'graph_outline',
    target: 'src/lib.rs',
    chars: 10,
    ms: 5,
    ok: true,
    tab: 'unattributed',
    session: null,
    ...over,
  };
}

describe('mergeEntries', () => {
  it('returns the SAME array when the feed is unchanged', () => {
    const prev = [entry(3), entry(2), entry(1)];
    // A fresh poll response: equal content, all-new object identities.
    const next = [entry(3), entry(2), entry(1)];
    // Reference equality is the whole point — it is what lets the caller's
    // assignment be a no-op Svelte skips.
    expect(mergeEntries(prev, next)).toBe(prev);
  });

  it('reuses the held object for every id it already has', () => {
    const prev = [entry(2), entry(1)];
    const next = [entry(3), entry(2), entry(1)];
    const merged = mergeEntries(prev, next);

    expect(merged).not.toBe(prev);
    expect(merged.map((e) => e.id)).toEqual([3, 2, 1]);
    // The two carried-over rows are the ORIGINAL objects, so their rendered
    // expressions do not re-evaluate.
    expect(merged[1]).toBe(prev[0]);
    expect(merged[2]).toBe(prev[1]);
    // The genuinely new row is the freshly fetched object.
    expect(merged[0]).toBe(next[0]);
  });

  it('drops entries that are gone (deleted, or aged out of the ring)', () => {
    const prev = [entry(3), entry(2), entry(1)];
    const next = [entry(3), entry(1)];
    const merged = mergeEntries(prev, next);

    expect(merged.map((e) => e.id)).toEqual([3, 1]);
    expect(merged[0]).toBe(prev[0]);
    expect(merged[1]).toBe(prev[2]);
  });

  it('does not report "unchanged" when ids shift at equal length', () => {
    // Same count, different membership — the length check alone would miss it.
    const prev = [entry(3), entry(2)];
    const next = [entry(4), entry(3)];
    const merged = mergeEntries(prev, next);

    expect(merged).not.toBe(prev);
    expect(merged.map((e) => e.id)).toEqual([4, 3]);
    expect(merged[1]).toBe(prev[0]);
  });

  it('handles the empty edges (first load, and a cleared feed)', () => {
    const first = mergeEntries([], [entry(1)]);
    expect(first.map((e) => e.id)).toEqual([1]);

    const emptied = mergeEntries([entry(1)], []);
    expect(emptied).toEqual([]);

    const stayedEmpty: ActivityEntry[] = [];
    expect(mergeEntries(stayedEmpty, [])).toBe(stayedEmpty);
  });
});

// The #51 attribution column. These pin the ONE property the Events tab can't
// get wrong: `{unrecognized: x}` is not the tab `x`, in the classifier, in the
// filter, and in the feed narrowing built on both. The Rust side unit-tests
// the same property against `Attribution::is_tab`; this is its mirror, because
// the rendering and the filtering happen here.

describe('attributionState / attributionId', () => {
  it('classifies the four wire shapes', () => {
    expect(attributionState('unattributed')).toBe('unattributed');
    expect(attributionState('headless')).toBe('headless');
    expect(attributionState({ tab: 'claude' })).toBe('tab');
    expect(attributionState({ unrecognized: 'claude' })).toBe('unrecognized');
  });

  it('returns the id only for the two states that carry one', () => {
    expect(attributionId({ tab: 'claude' })).toBe('claude');
    expect(attributionId({ unrecognized: 'ghost' })).toBe('ghost');
    expect(attributionId('headless')).toBeNull();
    expect(attributionId('unattributed')).toBeNull();
  });

  it('degrades anything unrecognizable to "unattributed", NEVER to "tab"', () => {
    // A variant added later, a malformed row, a missing field: all of them
    // mean "we don't know", and none of them may invent a tab.
    const junk = [
      undefined,
      null,
      'something-new',
      {},
      { tab: '' },
      { tab: 42 },
      { unrecognized: '' },
    ] as unknown as Attribution[];
    for (const a of junk) {
      expect(attributionState(a)).toBe('unattributed');
      expect(attributionId(a)).toBeNull();
    }
  });

  it('isTabAttribution is true ONLY for a real tab of that id', () => {
    expect(isTabAttribution({ tab: 'claude' }, 'claude')).toBe(true);
    expect(isTabAttribution({ tab: 'claude' }, 'opencode')).toBe(false);
    // The load-bearing case: the row merely quoted the id.
    expect(isTabAttribution({ unrecognized: 'claude' }, 'claude')).toBe(false);
    expect(isTabAttribution('headless', 'claude')).toBe(false);
    expect(isTabAttribution('unattributed', 'claude')).toBe(false);
  });
});

describe('matchesTabFilter', () => {
  const all: Attribution[] = [
    'unattributed',
    'headless',
    { tab: 'claude' },
    { unrecognized: 'claude' },
  ];

  it('"any" matches every state', () => {
    for (const a of all) expect(matchesTabFilter(a, FILTER_ANY)).toBe(true);
  });

  it('a tab filter matches {tab:x} and never {unrecognized:x}', () => {
    const f = tabFilterValue('claude');
    expect(matchesTabFilter({ tab: 'claude' }, f)).toBe(true);
    expect(matchesTabFilter({ unrecognized: 'claude' }, f)).toBe(false);
    expect(matchesTabFilter('headless', f)).toBe(false);
    expect(matchesTabFilter('unattributed', f)).toBe(false);
  });

  it('keeps headless and unattributed apart', () => {
    expect(matchesTabFilter('headless', TAB_FILTER_HEADLESS)).toBe(true);
    expect(matchesTabFilter('unattributed', TAB_FILTER_HEADLESS)).toBe(false);
    expect(matchesTabFilter('unattributed', TAB_FILTER_UNATTRIBUTED)).toBe(true);
    expect(matchesTabFilter('headless', TAB_FILTER_UNATTRIBUTED)).toBe(false);
  });

  it('selects unrecognized rows through their own option only', () => {
    expect(matchesTabFilter({ unrecognized: 'ghost' }, TAB_FILTER_UNRECOGNIZED)).toBe(true);
    expect(matchesTabFilter({ tab: 'claude' }, TAB_FILTER_UNRECOGNIZED)).toBe(false);
  });

  it('a tab literally named "headless" does not hijack the state option', () => {
    // This is why the filter value is prefixed rather than the bare id.
    expect(matchesTabFilter({ tab: 'headless' }, TAB_FILTER_HEADLESS)).toBe(false);
    expect(matchesTabFilter({ tab: 'headless' }, tabFilterValue('headless'))).toBe(true);
    expect(matchesTabFilter('headless', tabFilterValue('headless'))).toBe(false);
  });

  it('narrows to nothing on an option this build does not know', () => {
    // A stale selection must not silently widen the feed — showing MORE than
    // was asked for is the failure mode that misleads in an attribution view.
    expect(matchesTabFilter({ tab: 'claude' }, 'tab-claude')).toBe(false);
  });
});

describe('filterEntries', () => {
  const feed = [
    entry(5, { kind: 'mcp', source: 'claude', tab: { tab: 'claude' }, session: 's1' }),
    entry(4, { kind: 'graph', source: 'claude', tab: { unrecognized: 'claude' } }),
    entry(3, { kind: 'graph', source: 'offload', tab: 'headless' }),
    entry(2, { kind: 'offload', source: 'offload', tab: 'unattributed' }),
    entry(1, { kind: 'injection_flag', source: 'ssrf', tab: { tab: 'opencode' } }),
  ];

  it('returns the SAME array when nothing is constrained', () => {
    // Referential stability matters here for the same reason it does in
    // mergeEntries: a fresh array every 2s poll re-renders the whole feed.
    expect(filterEntries(feed, NO_FILTER)).toBe(feed);
  });

  it('filters by kind and by source independently', () => {
    expect(filterEntries(feed, { ...NO_FILTER, kind: 'graph' }).map((e) => e.id)).toEqual([4, 3]);
    expect(filterEntries(feed, { ...NO_FILTER, source: 'offload' }).map((e) => e.id)).toEqual([
      3, 2,
    ]);
  });

  it('filtering by tab "claude" excludes the row that only quoted that id', () => {
    const got = filterEntries(feed, { ...NO_FILTER, tab: tabFilterValue('claude') });
    expect(got.map((e) => e.id)).toEqual([5]);
  });

  it('ANDs the three axes together', () => {
    const got = filterEntries(feed, {
      kind: 'graph',
      source: 'offload',
      tab: TAB_FILTER_HEADLESS,
    });
    expect(got.map((e) => e.id)).toEqual([3]);
  });
});

// ── rowStatus (#48, M-24) ─────────────────────────────────────────────────
//
// The finding: `Unscreened`, the detector flags, `MemoryQuarantine` and
// `LatchOverride` all collapsed into ONE red chip, so "we did not look at all of
// it" read as "we blocked something" — the opposite of the truth — and a latch
// override the USER applied to hand capability back read as containment firing.
//
// These pin the distinctions rather than the current words: what must not
// regress is that no two of these screens share a status, and that the only
// status meaning "we stopped it" is reached by the screens that actually did.

/// One `injection_flag` row for `screen`.
function flag(screen: string, ok = true): ActivityEntry {
  return entry(1, { kind: 'injection_flag', source: screen, ok, tool: 'WebFetch' });
}

describe('rowStatus', () => {
  it('gives every containment screen its OWN status — no two collapse', () => {
    const byScreen: Record<string, RowStatus> = {
      // `is_denial` screens: the backend publishes these as ok:false.
      ssrf: rowStatus(flag('ssrf', false)),
      budget: rowStatus(flag('budget', false)),
      canary: rowStatus(flag('canary', false)),
      latch_refusal: rowStatus(flag('latch_refusal', false)),
      // Everything below denied NOTHING.
      signature: rowStatus(flag('signature')),
      unscreened: rowStatus(flag('unscreened')),
      memory_quarantine: rowStatus(flag('memory_quarantine')),
      latch_override: rowStatus(flag('latch_override')),
      latch_beacon: rowStatus(flag('latch_beacon')),
    };
    // The four denials share `denied`, which is correct — they all stopped a
    // call. The five that stopped nothing must each differ from that AND from
    // one another.
    expect(byScreen.ssrf).toBe('denied');
    expect(byScreen.budget).toBe('denied');
    expect(byScreen.canary).toBe('denied');
    expect(byScreen.latch_refusal).toBe('denied');
    const nonDenials = [
      byScreen.signature,
      byScreen.unscreened,
      byScreen.memory_quarantine,
      byScreen.latch_override,
      byScreen.latch_beacon,
    ];
    expect(new Set(nonDenials).size).toBe(nonDenials.length);
    expect(nonDenials).not.toContain('denied');
  });

  it('never reports an unscreened result as a denial or as clean', () => {
    // The whole finding in one assertion: an absent verdict is neither a
    // verdict of absence nor an alarm.
    const s = rowStatus(flag('unscreened'));
    expect(s).toBe('unscreened');
    expect(s).not.toBe('denied');
    expect(s).not.toBe('ok');
    expect(STATUS_TITLE[s]).toContain('nothing was blocked');
  });

  it('reports a user latch override as a GRANT, not as containment firing', () => {
    expect(rowStatus(flag('latch_override'))).toBe('granted');
    expect(rowStatus(flag('contamination_cleared'))).toBe('granted');
    // A grant and a block must not share a word — a release that reads as a
    // refusal is the inverted half of the same defect.
    expect(rowStatus(flag('latch_override'))).not.toBe('denied');
  });

  it('reports a held memory write as held, not as a refusal', () => {
    expect(rowStatus(flag('memory_quarantine'))).toBe('held');
    expect(STATUS_TITLE.held).toContain('Nothing was blocked');
  });

  it('does NOT read a rejected updater bundle as a blocked call', () => {
    // `updater` is documented as the one source written outside `record_flag`:
    // its `ok` is the bundle OUTCOME, not `Screen::is_denial`. Reading `!ok` as
    // "denied" there reported a refused rules bundle as a blocked tool call.
    expect(rowStatus(flag('updater', true))).toBe('update');
    expect(rowStatus(flag('updater', false))).toBe('rejected');
    expect(rowStatus(flag('updater', false))).not.toBe('denied');
  });

  it('gives an unknown screen no category rather than a borrowed one', () => {
    // A screen added backend-side after this build. Delivered ⇒ we have no word
    // for it; refused ⇒ `Screen::is_denial` is a claim we can still make.
    expect(rowStatus(flag('some_future_screen', true))).toBe('recorded');
    expect(rowStatus(flag('some_future_screen', false))).toBe('denied');
  });

  it('keeps the three plain call outcomes intact', () => {
    expect(rowStatus(entry(1))).toBe('ok');
    expect(rowStatus(entry(1, { ok: false }))).toBe('failed');
    // Telemetry channels record ok:false to mean "this signal fired".
    expect(rowStatus(entry(1, { ok: false, source: 'read_advisor' }))).toBe('signal');
    expect(rowStatus(entry(1, { ok: false, source: 'harness' }))).toBe('signal');
  });

  it('has a tooltip for every status it can return', () => {
    // A status with no sentence would render a bare word in a security feed.
    const all: RowStatus[] = [
      'ok',
      'failed',
      'signal',
      'denied',
      'flagged',
      'unscreened',
      'held',
      'engaged',
      'granted',
      'update',
      'rejected',
      'recorded',
      'started',
      'ready',
      'stopped',
      'down',
    ];
    for (const s of all) expect(STATUS_TITLE[s].length).toBeGreaterThan(0);
  });
});

// ── offload_server lifecycle rows ─────────────────────────────────────────
//
// Mirror of the injection_flag discipline above, for the feed added with the
// server-lifecycle events. The trap here is `ok`: it is true for a healthy
// start AND for a deliberate stop, so any classifier that consults it before
// the transition renders "the server is up" and "the server is gone" as the
// same word.

/// One `offload_server` row for transition `tool`.
function srv(tool: string, ok = true, over: Partial<ActivityEntry> = {}): ActivityEntry {
  return entry(1, { kind: 'offload_server', source: 'big-local', tool, ok, ...over });
}

describe('rowStatus — offload server lifecycle', () => {
  it('gives every transition its own status', () => {
    const seen = [
      rowStatus(srv('start')),
      rowStatus(srv('ready')),
      rowStatus(srv('stop')),
      rowStatus(srv('fail', false)),
    ];
    expect(seen).toEqual(['started', 'ready', 'stopped', 'down']);
    expect(new Set(seen).size).toBe(4);
  });

  it('never reads a deliberate stop as a failure', () => {
    // The backend records a stop as ok:true precisely so this holds; pinned
    // here because the frontend must not re-derive it from `ok` either way.
    expect(rowStatus(srv('stop'))).toBe('stopped');
    expect(rowStatus(srv('stop'))).not.toBe('down');
    expect(rowStatus(srv('stop'))).not.toBe('failed');
  });

  it('does not let a healthy start and a stop collapse via ok', () => {
    // Both are ok:true. A classifier keyed on `ok` would return one word here.
    expect(rowStatus(srv('start'))).not.toBe(rowStatus(srv('stop')));
  });

  it('separates a server failure from a failed tool call', () => {
    // `failed` is a call that errored; `down` is a backend that is not running.
    // Sharing a word would put a crashed llama-server in the same bucket as a
    // graph query that threw.
    expect(rowStatus(srv('fail', false))).toBe('down');
    expect(rowStatus(entry(1, { ok: false }))).toBe('failed');
  });

  it('degrades an unknown transition without inventing a claim', () => {
    // A verb added backend-side that this build predates.
    expect(rowStatus(srv('paused'))).toBe('ok');
    expect(rowStatus(srv('paused', false))).toBe('down');
  });

  it('does not misclassify an offload TASK row as a lifecycle row', () => {
    // The two kinds are one underscore apart and both carry a backend name in
    // `source`; a prefix match instead of an equality check would swallow the
    // task feed whole.
    expect(rowStatus(entry(1, { kind: 'offload', source: 'big-local', tool: 'offload_task' }))).toBe(
      'ok'
    );
  });
});
