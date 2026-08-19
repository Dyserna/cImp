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
      // Sandbox lane. `unsandboxed` was missing from this list since V33
      // Phase A added it — the tooltip existed, but nothing checked it did.
      'unsandboxed',
      'boundary',
      // V37 C6 health lane.
      'unhealthy',
      'recovered',
      // V37 C9: a tool withheld by description screening.
      'withheld',
    ];
    for (const s of all) expect(STATUS_TITLE[s].length).toBeGreaterThan(0);
  });
});

// ── V37 C6: mcp_health rows ───────────────────────────────────────────────
//
// Same discipline as the lifecycle feed below: the verb is in `tool`, and `ok`
// is the transition's OUTCOME. Reading `ok` first would render "this server
// came back" and "this call succeeded" as the same word in a lane whose whole
// purpose is telling you a server went away.

/// One `mcp_health` row for transition `tool`.
function mcpHealth(tool: string, ok: boolean, over: Partial<ActivityEntry> = {}): ActivityEntry {
  return entry(1, {
    kind: 'mcp_health',
    source: tool === 'connect_failed' ? 'connect' : 'probe',
    tool,
    ok,
    server: 'ddg',
    category: 'research',
    ...over,
  });
}

describe('rowStatus — mcp health', () => {
  it('gives the down transitions and the recovery different words', () => {
    expect(rowStatus(mcpHealth('unhealthy', false))).toBe('unhealthy');
    expect(rowStatus(mcpHealth('connect_failed', false))).toBe('unhealthy');
    expect(rowStatus(mcpHealth('healthy', true))).toBe('recovered');
  });

  it('never borrows the offload-server vocabulary', () => {
    // `down`/`ready` are written about a process cImp owns and stopped; an MCP
    // server is somebody else's, and the tooltips say so.
    const seen = [rowStatus(mcpHealth('unhealthy', false)), rowStatus(mcpHealth('healthy', true))];
    expect(seen).not.toContain('down');
    expect(seen).not.toContain('ready');
    expect(seen).not.toContain('stopped');
  });

  it('falls back on ok for a transition this build predates', () => {
    expect(rowStatus(mcpHealth('quarantined', false))).toBe('unhealthy');
    expect(rowStatus(mcpHealth('quarantined', true))).toBe('recovered');
  });

  it('does not classify an ordinary mcp CALL row as a health row', () => {
    // The two kinds share a server but not a lane: a failed call is a failed
    // call, not a server going down (that is exactly what the flap guard is
    // for).
    const call = entry(2, { kind: 'mcp', tool: 'ddg__search', ok: false, server: 'ddg' });
    expect(rowStatus(call)).toBe('failed');
  });
});

// ── V37 C9: tools withheld by description screening ───────────────────────
//
// These rows land in the `mcp` lane with `ok: false` and `source: "screen"` —
// the wire value the Rust side pins in one constant (`SCREEN_DROP_SOURCE` in
// `mcp_host.rs`), because a reader has to be able to tell a screening row from a
// call row without parsing prose. With no branch for them the classifier fell
// through to `failed`, whose sentence is "Call failed" — a claim about a call
// that was never made. `flagged` would be worse: its sentence promises "nothing
// was blocked", and this is the one site in cImp where detection really does
// remove something.

/// One `mcp`-lane screening row for the withheld tool `tool`.
function screenDrop(tool: string, over: Partial<ActivityEntry> = {}): ActivityEntry {
  return entry(3, {
    kind: 'mcp',
    source: 'screen',
    tool,
    target:
      'withheld from `evil` advertised tools: the injection screen flagged its name or description',
    ok: false,
    server: 'evil',
    category: 'research',
    ...over,
  });
}

describe('rowStatus — screen-withheld tools', () => {
  it('gives a withheld tool its own status, not a failed call', () => {
    const s = rowStatus(screenDrop('exfiltrate'));
    expect(s).toBe('withheld');
    expect(s).not.toBe('failed');
  });

  it('never reports a withheld tool as merely flagged', () => {
    // `flagged` means delivered-anyway, and its tooltip says so out loud. This
    // row is the opposite fact.
    expect(rowStatus(screenDrop('exfiltrate'))).not.toBe('flagged');
    expect(STATUS_TITLE.flagged).toContain('nothing was blocked');
    expect(STATUS_TITLE.withheld).not.toContain('nothing was blocked');
  });

  it('says the tool was withheld and that the server is unaffected', () => {
    const t = STATUS_TITLE.withheld;
    expect(t).toContain('WITHHELD');
    expect(t).toContain('unaffected');
    expect(t).toContain('re-screened');
  });

  it('keys on the exact wire source, not on kind alone', () => {
    // An ordinary failed CALL row on the same lane and the same server stays
    // `failed`: only `source === "screen"` marks a screening row, and matching
    // it loosely (a prefix, a substring) would relabel real failures.
    const call = entry(4, { kind: 'mcp', tool: 'ddg__search', ok: false, server: 'ddg' });
    expect(rowStatus(call)).toBe('failed');
    expect(rowStatus(screenDrop('x', { source: 'screening' }))).toBe('failed');
    // …and the source alone does not hijack another lane.
    expect(rowStatus(entry(5, { kind: 'graph', source: 'screen', ok: false }))).toBe('failed');
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

// ── sandbox rows (V33 Phase A) ─────────────────────────────────────────────
//
// Locked decision 17 requires "off (user choice)" and "unavailable
// (prerequisite missing)" to stay DISTINCT states — collapsing them is how a
// broken prerequisite hides behind a deliberate setting. The frontend half of
// that guarantee is here: both render as `unsandboxed`, and the row's own text
// carries which one, so nothing about the chip may start reading `ok` as the
// verb.

/// One `sandbox` row. `ok` mirrors "was this state chosen?" — true for the
/// user's switch being off, false for a missing prerequisite.
function sbx(tool: string, ok: boolean, over: Partial<ActivityEntry> = {}): ActivityEntry {
  return entry(1, { kind: 'sandbox', source: 'run_command', tool, ok, ...over });
}

describe('rowStatus — sandbox', () => {
  it('reports both negative states as unsandboxed, never as a blocked call', () => {
    const off = sbx('unsandboxed', true, { target: 'off (user choice) — git.exe' });
    const unavailable = sbx('unsandboxed', false, { target: 'unavailable — git.exe' });
    expect(rowStatus(off)).toBe('unsandboxed');
    expect(rowStatus(unavailable)).toBe('unsandboxed');
    // The command ran fine in both cases: this must never wear the words that
    // mean "we stopped something" or "the call errored".
    expect(rowStatus(off)).not.toBe('denied');
    expect(rowStatus(unavailable)).not.toBe('failed');
  });

  it('keeps the two states distinguishable in the row itself', () => {
    // The chip is one word by design; decision 17's distinctness lives in
    // `target`, which the detail popup shows. If a future change moves the
    // distinction into the chip, this test is where that gets noticed.
    const off = sbx('unsandboxed', true, { target: 'off (user choice) — git.exe' });
    const unavailable = sbx('unsandboxed', false, { target: 'unavailable — git.exe' });
    expect(off.target).not.toEqual(unavailable.target);
    expect(off.ok).not.toEqual(unavailable.ok);
  });

  it('does not read a grant event as an unsandboxed run', () => {
    // Other tools in this lane (grants, drive mappings) are ordinary rows.
    expect(rowStatus(sbx('grant', true, { target: 'C:/tools' }))).toBe('ok');
    expect(rowStatus(sbx('grant', false, { target: 'C:/tools' }))).toBe('failed');
  });

  it('reads a sandboxed run as ordinary traffic, not as an alarm', () => {
    // The confirmation row exists to answer "is this actually sandboxed?" —
    // an empty lane used to mean either "everything was" or "nothing ran".
    // Answering it must not cost the lane its signal-to-noise: the expected
    // case stays quiet, and the program name lives in `target`.
    const run = sbx('sandboxed', true, { target: 'sandboxed — git.exe' });
    expect(rowStatus(run)).toBe('ok');
    expect(run.target).toContain('git.exe');
  });

  it('gives a suspected boundary hit its own word — not denied, not failed', () => {
    const hit = sbx('denied', false, {
      target: 'filesystem/OS access denied — git.exe',
    });
    expect(rowStatus(hit)).toBe('boundary');
    // `denied` is this app's one "we stopped it", and it is filled red. cImp
    // cannot see the OS's ACL decision — the backend words this row as a
    // heuristic, so the chip must not assert more than the row does.
    expect(rowStatus(hit)).not.toBe('denied');
    // `failed` is wrong the other way: the tool call itself returned output.
    expect(rowStatus(hit)).not.toBe('failed');
    // The program is in the scannable column, like every other row in the lane.
    expect(hit.target).toContain('git.exe');
  });

  it('keeps every sandbox row type visibly distinct', () => {
    // One lane, four row types. If two of them ever render as the same word,
    // the lane stops answering the question it was added to answer.
    const words = [
      rowStatus(sbx('unsandboxed', true, { target: 'off (user choice) — git.exe' })),
      rowStatus(sbx('sandboxed', true, { target: 'sandboxed — git.exe' })),
      rowStatus(sbx('denied', false, { target: 'socket access denied — curl.exe' })),
      rowStatus(sbx('grant', false, { target: 'C:/tools' })),
    ];
    expect(new Set(words).size).toBe(words.length);
  });
});

// ── plugin discovery rows (V38 Phase A) ───────────────────────────────────
//
// This lane deliberately adds NO new status word: a plugin definition either
// loaded or it did not, and `ok`/`failed` say that exactly. What it must not do
// is fall into any of the words that carry a security claim — a rejected
// manifest is a malformed FILE, not a blocked call.

/// One `plugin` row. `tool` is the verb (`rejected` / `conflict` / `rescan`).
function plug(tool: string, ok: boolean, over: Partial<ActivityEntry> = {}): ActivityEntry {
  return entry(1, { kind: 'plugin', source: 'acme@1.0.0', tool, ok, ...over });
}

describe('rowStatus — plugin discovery', () => {
  it('reads a rejected manifest as a plain failure, never as a blocked call', () => {
    const rejected = plug('rejected', false, { target: 'identity: `name` must be…' });
    expect(rowStatus(rejected)).toBe('failed');
    // The security vocabulary belongs to rows where something was STOPPED.
    expect(rowStatus(rejected)).not.toBe('denied');
    expect(rowStatus(rejected)).not.toBe('flagged');
    expect(rowStatus(rejected)).not.toBe('boundary');
  });

  it('reads an identity conflict as a failure too', () => {
    expect(rowStatus(plug('conflict', false, { source: 'acme@1.0.0' }))).toBe('failed');
  });

  it('lets the scan summary report the folder at a glance', () => {
    // The backend sets `ok` on the summary to the FOLDER’s health, so a clean
    // folder is one green row and a folder with a rejected plugin is not.
    expect(rowStatus(plug('rescan', true, { source: 'plugins', target: 'loaded 2 · rejected 0' }))).toBe(
      'ok',
    );
    expect(rowStatus(plug('rescan', false, { source: 'plugins', target: 'loaded 1 · rejected 1' }))).toBe(
      'failed',
    );
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
