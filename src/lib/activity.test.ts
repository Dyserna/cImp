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
import { mergeEntries, type ActivityEntry } from './activity';

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
