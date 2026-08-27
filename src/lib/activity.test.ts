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

import { FIRST_HARNESS, SECOND_HARNESS } from './harness.fixture';

// V40 Phase F: the tab/source ids below come from the committed registry
// fixture. These tests are about ATTRIBUTION — which tab, recognized or not —
// so what matters is that the ids are real ones, not which products they name.
const H1 = FIRST_HARNESS.id;
const H2 = SECOND_HARNESS.id;

function entry(id: number, over: Partial<ActivityEntry> = {}): ActivityEntry {
  return {
    id,
    ts_ms: 1_000 + id,
    kind: 'graph',
    root: '/p',
    source: H1,
    tool: 'graph_outline',
    target: 'src/lib.rs',
    chars: 10,
    ms: 5,
    ok: true,
    // The backend classifies this column; a fixture states the word the
    // row would carry, and the cases that care override it.
    status: 'ok',
    tab: 'unattributed',
    session: null,
    server: null,
    category: null,
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
    expect(attributionState({ tab: H1 })).toBe('tab');
    expect(attributionState({ unrecognized: H1 })).toBe('unrecognized');
  });

  it('returns the id only for the two states that carry one', () => {
    expect(attributionId({ tab: H1 })).toBe(H1);
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
    expect(isTabAttribution({ tab: H1 }, H1)).toBe(true);
    expect(isTabAttribution({ tab: H1 }, H2)).toBe(false);
    // The load-bearing case: the row merely quoted the id.
    expect(isTabAttribution({ unrecognized: H1 }, H1)).toBe(false);
    expect(isTabAttribution('headless', H1)).toBe(false);
    expect(isTabAttribution('unattributed', H1)).toBe(false);
  });
});

describe('matchesTabFilter', () => {
  const all: Attribution[] = [
    'unattributed',
    'headless',
    { tab: H1 },
    { unrecognized: H1 },
  ];

  it('"any" matches every state', () => {
    for (const a of all) expect(matchesTabFilter(a, FILTER_ANY)).toBe(true);
  });

  it('a tab filter matches {tab:x} and never {unrecognized:x}', () => {
    const f = tabFilterValue(H1);
    expect(matchesTabFilter({ tab: H1 }, f)).toBe(true);
    expect(matchesTabFilter({ unrecognized: H1 }, f)).toBe(false);
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
    expect(matchesTabFilter({ tab: H1 }, TAB_FILTER_UNRECOGNIZED)).toBe(false);
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
    expect(matchesTabFilter({ tab: H1 }, `tab-${H1}`)).toBe(false);
  });
});

describe('filterEntries', () => {
  const feed = [
    entry(5, { kind: 'mcp', source: H1, tab: { tab: H1 }, session: 's1' }),
    entry(4, { kind: 'graph', source: H1, tab: { unrecognized: H1 } }),
    entry(3, { kind: 'graph', source: 'offload', tab: 'headless' }),
    entry(2, { kind: 'offload', source: 'offload', tab: 'unattributed' }),
    entry(1, { kind: 'injection_flag', source: 'ssrf', tab: { tab: H2 } }),
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

  it('filtering by one tab id excludes the row that only quoted that id', () => {
    const got = filterEntries(feed, { ...NO_FILTER, tab: tabFilterValue(H1) });
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

// ── The status vocabulary the feeds render (#48, M-24) ────────────────────
//
// The finding: `unscreened`, the detector flags, `memory_quarantine` and
// `latch_override` all collapsed into ONE red chip, so "we did not look at all
// of it" read as "we blocked something" — the opposite of the truth — and a
// latch override the USER applied to hand capability back read as containment
// firing.
//
// **V42: the classifier moved to `crate::activity`.** A row arrives carrying
// its `status`, and the distinctions — no two containment screens sharing a
// word, a grant never reading as a block, an unscreened result reading as
// neither — are pinned in Rust, beside `Screen::is_denial` itself. What is
// still testable here is the rendering half: every word the backend can publish
// has a sentence, and (at the bottom of this file) a chip rule to draw it with.
// The two halves are held together across the language boundary by the Rust
// guard `the_status_vocabulary_matches_the_frontends`, which reads the union in
// `activity.ts` and refuses to let either side hold a word the other does not.

describe('STATUS_TITLE', () => {
  it('has a tooltip for every status a row can carry', () => {
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
      // Sandbox lane. `unsandboxed` was missing from this list since V33
      // Phase A added it — the tooltip existed, but nothing checked it did.
      'unsandboxed',
      'boundary',
      // V37 C6 health lane.
      'unhealthy',
      'recovered',
      // V37 C9: a tool withheld by description screening.
      'withheld',
      // V39 delegation transitions that are not call outcomes.
      'driving',
      'takeover',
      'moved',
      // #153: overlay keys a settings save discarded. Minted `ok: true`, so
      // without its own word it renders "Call succeeded".
      'dropped',
    ];
    for (const s of all) expect(STATUS_TITLE[s].length).toBeGreaterThan(0);
  });

  it('says out loud what was NOT blocked', () => {
    // Every word ADDED by M-24 exists because it was being read as containment
    // firing; the sentence is where that gets corrected for a reader who does
    // not know the vocabulary. An absent verdict is not a verdict of absence.
    expect(STATUS_TITLE.unscreened).toContain('nothing was blocked');
    expect(STATUS_TITLE.held).toContain('Nothing was blocked');
    expect(STATUS_TITLE.engaged).toContain('Nothing was blocked');
    expect(STATUS_TITLE.granted).toContain('not a block');
    // `flagged` means delivered-anyway, and says so.
    expect(STATUS_TITLE.flagged).toContain('nothing was blocked');
  });

  it('never promises "nothing was blocked" for the one word that means something WAS', () => {
    // V37 C9 is the single place in cImp where detection actually REMOVES
    // something. Its sentence has to be the opposite of `flagged`'s, and it has
    // to say the blast radius: the server and its other tools are unaffected,
    // and the tool comes back if the screen stops matching.
    const t = STATUS_TITLE.withheld;
    expect(t).not.toContain('nothing was blocked');
    expect(t).toContain('WITHHELD');
    expect(t).toContain('unaffected');
    expect(t).toContain('re-screened');
  });
});

describe('mcp identity columns', () => {
  it('carries server and category through the poll merge untouched', () => {
    // `mergeEntries` reuses row objects by id; the identity columns must ride
    // along rather than be recomputed anywhere on this side.
    const row = entry(9, { kind: 'mcp', tool: 'git__extra__log', server: 'git__extra', category: 'vcs' });
    const merged = mergeEntries([], [row]);
    expect(merged[0].server).toBe('git__extra');
    expect(merged[0].category).toBe('vcs');
    // The `__` split the backend refuses to do would have said `git`.
    expect(merged[0].server).not.toBe('git');
  });

  it('treats a null server as absent, not as an empty name', () => {
    const row = entry(10, { kind: 'graph' });
    expect(row.server).toBeNull();
    expect(row.category).toBeNull();
  });
});

// Every RowStatus word must have pixels. The chip's class IS the status word
// (`<span class="schip {status}">` in StatusChip.svelte), so a status missing
// from that component's scoped <style> renders as the bare base chip — which
// is how "a server went down" and "a server came back" briefly drew identically
// (the V37 close-out review found `unhealthy`/`recovered` styleless: the
// F-V37-1 defect class, one layer down). STATUS_TITLE's Record type is the
// tooltip-completeness guard; this is its CSS twin. Raw-source mechanism per
// settingsPointers.test.ts: Vite's glob, not node:fs.
const STATUS_CHIP_SOURCE = import.meta.glob('/src/lib/StatusChip.svelte', {
  query: '?raw',
  import: 'default',
  eager: true,
}) as Record<string, string>;

describe('StatusChip covers every RowStatus', () => {
  it('has a .schip.<status> rule for each STATUS_TITLE key', () => {
    const css = Object.values(STATUS_CHIP_SOURCE)[0] ?? '';
    expect(css.length).toBeGreaterThan(0);
    for (const status of Object.keys(STATUS_TITLE)) {
      expect(css, `StatusChip.svelte has no .schip.${status} rule`).toMatch(
        new RegExp(String.raw`\.schip\.${status}\b`),
      );
    }
  });
});
