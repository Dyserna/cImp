// Unit tests for the SpritePlayer's pure state-machine logic (group parsing,
// rotation-list selection, state->group resolution, alpha-bbox crop math).
// The canvas/timer half of the player is DOM-bound and exercised manually;
// these cover every branch of the transition rules it feeds from.

import { describe, it, expect } from 'vitest';
import {
  parseGroups,
  resolveRotationList,
  resolveStateGroup,
  squareCropFromData,
  type SpriteGroup,
} from './spritePlayer';

describe('parseGroups', () => {
  it('returns an empty map for a manifest without groups', () => {
    expect(parseGroups(undefined)).toEqual({});
    expect(parseGroups([])).toEqual({});
  });

  it('maps each valid group to its animation list', () => {
    const groups: SpriteGroup[] = [
      { state: 'Idle', animations: ['idle_blink', 'idle_tail'] },
      { state: 'Speaking', animations: ['talk'] },
    ];
    expect(parseGroups(groups)).toEqual({
      Idle: ['idle_blink', 'idle_tail'],
      Speaking: ['talk'],
    });
  });

  it('drops malformed entries and non-string members', () => {
    const groups = [
      null,
      { state: 42, animations: ['x'] },
      { state: 'Thinking', animations: 'not-a-list' },
      { state: 'Idle', animations: ['ok', 7, null, 'also_ok'] },
    ] as unknown as SpriteGroup[];
    expect(parseGroups(groups)).toEqual({ Idle: ['ok', 'also_ok'] });
  });

  it('lets a later duplicate state override an earlier one', () => {
    const groups: SpriteGroup[] = [
      { state: 'Idle', animations: ['a'] },
      { state: 'Idle', animations: ['b'] },
    ];
    expect(parseGroups(groups)).toEqual({ Idle: ['b'] });
  });
});

describe('resolveRotationList', () => {
  const available = ['idle', 'wave', 'talk'];

  it('filters the requested names to those present, preserving order', () => {
    expect(resolveRotationList(['talk', 'missing', 'idle'], available)).toEqual([
      'talk',
      'idle',
    ]);
  });

  it('falls back to every available animation when nothing survives', () => {
    expect(resolveRotationList(['nope'], available)).toEqual(available);
    expect(resolveRotationList([], available)).toEqual(available);
  });

  it('returns a copy, not the available array itself', () => {
    const out = resolveRotationList([], available);
    expect(out).not.toBe(available);
  });
});

describe('resolveStateGroup', () => {
  const groups: Record<string, string[]> = {
    Idle: ['idle_blink'],
    Listening: ['ears_up'],
  };
  const groupFor = (s: string) => groups[s] ?? [];

  it('keeps the state as key when the manifest defines its group', () => {
    expect(resolveStateGroup(groupFor, 'Listening')).toEqual({
      key: 'Listening',
      names: ['ears_up'],
    });
  });

  it('falls back to the Idle group under the Idle key for undefined states', () => {
    expect(resolveStateGroup(groupFor, 'Thinking')).toEqual({
      key: 'Idle',
      names: ['idle_blink'],
    });
  });

  it('gives all fallback states the same rotation identity (no restart between them)', () => {
    // Thinking -> Speaking -> Idle all resolve to key 'Idle', so the player's
    // setAnims key check treats the sequence as one uninterrupted rotation.
    const a = resolveStateGroup(groupFor, 'Thinking');
    const b = resolveStateGroup(groupFor, 'Speaking');
    const c = resolveStateGroup(groupFor, 'Idle');
    expect(a.key).toBe('Idle');
    expect(b.key).toBe('Idle');
    expect(c.key).toBe('Idle');
    expect(a.names).toEqual(c.names);
  });

  it('resolves to an empty list under the Idle key when even Idle is undefined', () => {
    // setAnims then falls back to every available animation.
    expect(resolveStateGroup(() => [], 'Error')).toEqual({ key: 'Idle', names: [] });
  });
});

describe('squareCropFromData', () => {
  const SIZE = 8;

  /// RGBA buffer with alpha=255 at the given (x, y) points.
  function frame(points: Array<[number, number]>): Uint8ClampedArray {
    const data = new Uint8ClampedArray(SIZE * SIZE * 4);
    for (const [x, y] of points) data[(y * SIZE + x) * 4 + 3] = 255;
    return data;
  }

  it('falls back to the full tile when every frame is transparent', () => {
    expect(squareCropFromData([frame([])], SIZE)).toEqual({ x: 0, y: 0, w: SIZE, h: SIZE });
    expect(squareCropFromData([], SIZE)).toEqual({ x: 0, y: 0, w: SIZE, h: SIZE });
  });

  it('crops a single pixel to a 1x1 box at its position', () => {
    expect(squareCropFromData([frame([[2, 3]])], SIZE)).toEqual({ x: 2, y: 3, w: 1, h: 1 });
  });

  it('expands a wide bbox to a centered square', () => {
    // Content x in [1,6], y in [3,4]: w=6, h=2 -> side 6, centered on y.
    const data = frame([
      [1, 3],
      [6, 4],
    ]);
    expect(squareCropFromData([data], SIZE)).toEqual({ x: 1, y: 1, w: 6, h: 6 });
  });

  it('clamps the square inside the tile at the edges', () => {
    // Full-width single row at the bottom: side = 8, y would center at 4
    // but clamps to 0 so the box stays inside the tile.
    const row: Array<[number, number]> = [];
    for (let x = 0; x < SIZE; x++) row.push([x, 7]);
    expect(squareCropFromData([frame(row)], SIZE)).toEqual({ x: 0, y: 0, w: SIZE, h: SIZE });
  });

  it('unions the bbox across all frames', () => {
    const a = frame([[0, 0]]);
    const b = frame([[5, 5]]);
    expect(squareCropFromData([a, b], SIZE)).toEqual({ x: 0, y: 0, w: 6, h: 6 });
  });

  it('never exceeds the tile when content spans it fully', () => {
    const c = squareCropFromData([frame([[0, 0], [7, 7]])], SIZE);
    expect(c).toEqual({ x: 0, y: 0, w: SIZE, h: SIZE });
  });
});
