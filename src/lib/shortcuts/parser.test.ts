// Regression tests from the 2026-07 legacy review (session 6): the literal
// '+' and Space keys were unrepresentable in the shortcut string format —
// `formatShortcut` emitted "Ctrl++" / "Ctrl+ ", which `parseShortcut`'s
// split-on-'+' collapsed into a modifier-only, never-matching predicate
// (or `null` for a bare Space). The capture UI now emits "Ctrl+Plus" /
// "Ctrl+Space", and the parser additionally understands the legacy
// trailing-'+' shape so previously stored strings keep working.

import { describe, expect, test } from 'vitest';
import { parseShortcut, matches, formatShortcut } from './parser';

function ev(over: Partial<KeyboardEvent>): KeyboardEvent {
  return {
    key: 'a',
    ctrlKey: false,
    shiftKey: false,
    altKey: false,
    metaKey: false,
    ...over,
  } as KeyboardEvent;
}

describe('parseShortcut', () => {
  test('plain modifier+letter', () => {
    expect(parseShortcut('Ctrl+Shift+E')).toEqual({
      key: 'e',
      ctrl: true,
      shift: true,
      alt: false,
      meta: false,
    });
  });

  test('named keys and short aliases map to event.key values', () => {
    expect(parseShortcut('Ctrl+Alt+Left')?.key).toBe('arrowleft');
    expect(parseShortcut('Ctrl+Space')?.key).toBe(' ');
    expect(parseShortcut('Esc')?.key).toBe('escape');
  });

  test('punctuation keys that are not the separator', () => {
    expect(parseShortcut('Ctrl+,')).toEqual({
      key: ',',
      ctrl: true,
      shift: false,
      alt: false,
      meta: false,
    });
  });

  test('the Plus name parses to the literal + key', () => {
    expect(parseShortcut('Ctrl+Plus')).toEqual({
      key: '+',
      ctrl: true,
      shift: false,
      alt: false,
      meta: false,
    });
  });

  test('legacy trailing-plus strings parse to the literal + key with modifiers intact', () => {
    expect(parseShortcut('Ctrl++')).toEqual({
      key: '+',
      ctrl: true,
      shift: false,
      alt: false,
      meta: false,
    });
    expect(parseShortcut('Ctrl+Shift++')).toEqual({
      key: '+',
      ctrl: true,
      shift: true,
      alt: false,
      meta: false,
    });
    expect(parseShortcut('+')).toEqual({
      key: '+',
      ctrl: false,
      shift: false,
      alt: false,
      meta: false,
    });
  });

  test('empty / null / whitespace stay unbound', () => {
    expect(parseShortcut(null)).toBeNull();
    expect(parseShortcut(undefined)).toBeNull();
    expect(parseShortcut('   ')).toBeNull();
  });
});

describe('matches', () => {
  test('modifiers compare strictly (Ctrl+E does not fire on Ctrl+Shift+E)', () => {
    const p = parseShortcut('Ctrl+E')!;
    expect(matches(ev({ key: 'e', ctrlKey: true }), p)).toBe(true);
    expect(matches(ev({ key: 'e', ctrlKey: true, shiftKey: true }), p)).toBe(false);
  });

  test('a parsed Ctrl+Plus predicate matches a real Ctrl and + keydown', () => {
    const p = parseShortcut('Ctrl+Plus')!;
    expect(matches(ev({ key: '+', ctrlKey: true }), p)).toBe(true);
  });

  test('a parsed Ctrl+Space predicate matches the raw space key', () => {
    const p = parseShortcut('Ctrl+Space')!;
    expect(matches(ev({ key: ' ', ctrlKey: true }), p)).toBe(true);
  });
});

describe('formatShortcut ⇄ parseShortcut round-trip', () => {
  test('the + key formats as Plus, not a bare separator collision', () => {
    const e = ev({ key: '+', ctrlKey: true });
    expect(formatShortcut(e)).toBe('Ctrl+Plus');
    const p = parseShortcut(formatShortcut(e))!;
    expect(matches(e, p)).toBe(true);
  });

  test('the Space key formats as Space, not a trimmed-away raw space', () => {
    const withMod = ev({ key: ' ', ctrlKey: true });
    expect(formatShortcut(withMod)).toBe('Ctrl+Space');
    expect(matches(withMod, parseShortcut(formatShortcut(withMod))!)).toBe(true);

    const bare = ev({ key: ' ' });
    expect(formatShortcut(bare)).toBe('Space');
    expect(matches(bare, parseShortcut(formatShortcut(bare))!)).toBe(true);
  });

  test('letters, arrows, and shifted symbols still round-trip', () => {
    for (const e of [
      ev({ key: 'e', ctrlKey: true }),
      ev({ key: 'ArrowUp', ctrlKey: true, altKey: true }),
      ev({ key: '!', ctrlKey: true, shiftKey: true }),
      ev({ key: 'Enter', ctrlKey: true }),
    ]) {
      const p = parseShortcut(formatShortcut(e));
      expect(p, `parse of ${formatShortcut(e)}`).not.toBeNull();
      expect(matches(e, p!), `round-trip of ${formatShortcut(e)}`).toBe(true);
    }
  });
});
