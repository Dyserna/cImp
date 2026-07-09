import { describe, expect, test } from 'vitest';
import { wordDiff, pairHunkLines } from './diffWords';

describe('wordDiff', () => {
  test('matches unchanged tokens and flags only the changed word', () => {
    const { left, right } = wordDiff('foo bar baz', 'foo qux baz');
    // 'foo', ' ', 'baz' are shared; 'bar'/'qux' differ.
    expect(left.filter((p) => p.kind === 'same').map((p) => p.text)).toEqual(['foo', ' ', ' ', 'baz']);
    expect(left.some((p) => p.kind === 'del' && p.text === 'bar')).toBe(true);
    expect(right.some((p) => p.kind === 'add' && p.text === 'qux')).toBe(true);
  });

  test('identical lines produce only same parts', () => {
    const { left, right } = wordDiff('unchanged line', 'unchanged line');
    expect(left.every((p) => p.kind === 'same')).toBe(true);
    expect(right.every((p) => p.kind === 'same')).toBe(true);
  });

  test('wholly different lines produce del-only left and add-only right', () => {
    const { left, right } = wordDiff('abc', 'xyz');
    expect(left.every((p) => p.kind === 'del')).toBe(true);
    expect(right.every((p) => p.kind === 'add')).toBe(true);
  });

  test('a single-character edit inside an identifier diffs at word granularity', () => {
    const { left, right } = wordDiff('longVariableName', 'longVariableNamee');
    // Whole-token del/add, not a per-character explosion.
    expect(left).toEqual([{ text: 'longVariableName', kind: 'del' }]);
    expect(right).toEqual([{ text: 'longVariableNamee', kind: 'add' }]);
  });

  test('a run of the same character class tokenizes as one token, not one per char', () => {
    // A long run of spaces (common leading indentation) must not explode
    // into one token per space — that would make even a short-looking
    // indentation change look "wholly different" token-for-token. Expect
    // exactly 2 tokens (the whitespace run, the word), not 4 + 8.
    const { left } = wordDiff('    indented', '    indented');
    expect(left).toEqual([
      { text: '    ', kind: 'same' },
      { text: 'indented', kind: 'same' },
    ]);
  });

  test('very long, highly-tokenized lines fall back to whole-line del/add rather than a huge DP table', () => {
    // Many short distinct "words" (not long runs) so tokenization alone
    // doesn't collapse this back down — this is what actually exercises the
    // MAX_DP_CELLS guard.
    const oldLine = Array.from({ length: 150 }, (_, i) => `w${i}`).join(' ');
    const newLine = Array.from({ length: 150 }, (_, i) => `x${i}`).join(' ');
    const { left, right } = wordDiff(oldLine, newLine);
    expect(left).toEqual([{ text: oldLine, kind: 'del' }]);
    expect(right).toEqual([{ text: newLine, kind: 'add' }]);
  });

  test('empty lines produce no parts', () => {
    const { left, right } = wordDiff('', '');
    expect(left).toEqual([]);
    expect(right).toEqual([]);
  });
});

describe('pairHunkLines', () => {
  test('context lines pass through untouched', () => {
    const groups = pairHunkLines([[' ', 'ctx1'], [' ', 'ctx2']]);
    expect(groups).toEqual([
      { type: 'ctx', text: 'ctx1' },
      { type: 'ctx', text: 'ctx2' },
    ]);
  });

  test('a single del immediately followed by a single add pairs for word-diff', () => {
    const groups = pairHunkLines([['-', 'old text'], ['+', 'new text']]);
    expect(groups).toEqual([{ type: 'pair', oldText: 'old text', newText: 'new text' }]);
  });

  test('equal-length multi-line del/add runs pair index-wise', () => {
    const groups = pairHunkLines([
      ['-', 'a1'],
      ['-', 'a2'],
      ['+', 'b1'],
      ['+', 'b2'],
    ]);
    expect(groups).toEqual([
      { type: 'pair', oldText: 'a1', newText: 'b1' },
      { type: 'pair', oldText: 'a2', newText: 'b2' },
    ]);
  });

  test('uneven del/add run lengths fall back to plain del/add (no pairing)', () => {
    const groups = pairHunkLines([
      ['-', 'a1'],
      ['-', 'a2'],
      ['+', 'b1'],
    ]);
    expect(groups).toEqual([
      { type: 'del', text: 'a1' },
      { type: 'del', text: 'a2' },
      { type: 'add', text: 'b1' },
    ]);
  });

  test('a pure addition with no preceding del renders as plain add', () => {
    const groups = pairHunkLines([['+', 'brand new']]);
    expect(groups).toEqual([{ type: 'add', text: 'brand new' }]);
  });

  test('a pure deletion with no following add renders as plain del', () => {
    const groups = pairHunkLines([['-', 'gone']]);
    expect(groups).toEqual([{ type: 'del', text: 'gone' }]);
  });

  test('mixed context/pair/del sequence preserves order', () => {
    const groups = pairHunkLines([
      [' ', 'ctx'],
      ['-', 'old'],
      ['+', 'new'],
      [' ', 'ctx2'],
    ]);
    expect(groups).toEqual([
      { type: 'ctx', text: 'ctx' },
      { type: 'pair', oldText: 'old', newText: 'new' },
      { type: 'ctx', text: 'ctx2' },
    ]);
  });
});
