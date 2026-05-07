import { describe, expect, test, vi } from 'vitest';

// Tauri's convertFileSrc isn't available in vitest's jsdom env; stub it.
vi.mock('@tauri-apps/api/core', () => ({
  convertFileSrc: (p: string) => `asset://${p}`,
}));

import {
  categoryOf,
  composeTheme,
  cssSizeFor,
  effectiveBackgroundMode,
  recreateDebounceDelay,
  rgbaFrom,
  type RenderingMode,
  type TabWithBackgroundOverride,
} from './background';
import type { TerminalBackgroundSettings } from '../settings/types';

const cfgNone: TerminalBackgroundSettings = {
  image: null,
  color: null,
  opacity: 0.4,
  blur: 0,
  size: 'cover',
  position: 'center',
  snapshot_lines: 2000,
  presets: [],
  preview_category_flips: true,
};
const cfgColorOnly: TerminalBackgroundSettings = {
  ...cfgNone,
  color: '#1a2b3c',
};
const cfgImageOnly: TerminalBackgroundSettings = {
  ...cfgNone,
  image: '/tmp/bg.png',
};
const cfgImageAndColor: TerminalBackgroundSettings = {
  ...cfgNone,
  image: '/tmp/bg.png',
  color: '#abcdef',
};

const tabInherit: TabWithBackgroundOverride = { background_override: null };
const tabDisabled: TabWithBackgroundOverride = { background_override: 'disabled' };

describe('effectiveBackgroundMode — three-state override × four-cell matrix', () => {
  // --- Inheriting global (background_override === null) -------------------

  test('inherit + global none → none', () => {
    expect(effectiveBackgroundMode(tabInherit, cfgNone)).toEqual({ kind: 'none' });
  });

  test('inherit + global color-only → color', () => {
    expect(effectiveBackgroundMode(tabInherit, cfgColorOnly)).toEqual({
      kind: 'color',
      color: '#1a2b3c',
    });
  });

  test('inherit + global image-only → image with null tint', () => {
    const m = effectiveBackgroundMode(tabInherit, cfgImageOnly);
    expect(m.kind).toBe('image');
    if (m.kind === 'image') {
      expect(m.cfg).toBe(cfgImageOnly);
      expect(m.tint).toBe(null);
    }
  });

  test('inherit + global image+color → image with tint', () => {
    const m = effectiveBackgroundMode(tabInherit, cfgImageAndColor);
    expect(m.kind).toBe('image');
    if (m.kind === 'image') {
      expect(m.tint).toBe('#abcdef');
    }
  });

  // --- Disabled override (always wins, regardless of global) --------------

  test.each([
    ['none', cfgNone],
    ['color-only', cfgColorOnly],
    ['image-only', cfgImageOnly],
    ['image+color', cfgImageAndColor],
  ] as const)('disabled override + global %s → none', (_, global) => {
    expect(effectiveBackgroundMode(tabDisabled, global)).toEqual({ kind: 'none' });
  });

  // --- Custom override (replaces global wholesale) ------------------------

  test('custom override wins over global', () => {
    const tab: TabWithBackgroundOverride = { background_override: cfgImageOnly };
    // Even with a color-only global, the tab's image override drives the mode.
    const m = effectiveBackgroundMode(tab, cfgColorOnly);
    expect(m.kind).toBe('image');
    if (m.kind === 'image') {
      expect(m.cfg).toBe(cfgImageOnly);
    }
  });

  test('custom color override against image global → color path on this tab', () => {
    const tab: TabWithBackgroundOverride = { background_override: cfgColorOnly };
    expect(effectiveBackgroundMode(tab, cfgImageOnly)).toEqual({
      kind: 'color',
      color: '#1a2b3c',
    });
  });
});

describe('categoryOf', () => {
  test('image → image', () => {
    expect(categoryOf({ kind: 'image', cfg: cfgImageOnly, tint: null })).toBe('image');
  });
  test('color → fast', () => {
    expect(categoryOf({ kind: 'color', color: '#000000' })).toBe('fast');
  });
  test('none → fast', () => {
    expect(categoryOf({ kind: 'none' })).toBe('fast');
  });
});

describe('composeTheme', () => {
  const baseTheme = { background: '#000000', foreground: '#ffffff', red: '#ff0000' };

  test("'none' returns the theme unchanged (same reference)", () => {
    const out = composeTheme(baseTheme, { kind: 'none' });
    expect(out).toBe(baseTheme);
  });

  test("'color' replaces background only", () => {
    const out = composeTheme(baseTheme, { kind: 'color', color: '#1a2b3c' });
    expect(out.background).toBe('#1a2b3c');
    expect(out.foreground).toBe(baseTheme.foreground);
    expect(out.red).toBe(baseTheme.red);
    expect(baseTheme.background).toBe('#000000'); // input not mutated
  });

  test("'image' with explicit tint produces rgba from tint+opacity", () => {
    const mode: RenderingMode = {
      kind: 'image',
      cfg: { ...cfgImageAndColor, opacity: 0.5 },
      tint: '#ff0000',
    };
    const out = composeTheme(baseTheme, mode);
    expect(out.background).toBe('rgba(255, 0, 0, 0.5)');
  });

  test("'image' with null tint defaults to black", () => {
    const mode: RenderingMode = {
      kind: 'image',
      cfg: { ...cfgImageOnly, opacity: 0.4 },
      tint: null,
    };
    const out = composeTheme(baseTheme, mode);
    expect(out.background).toBe('rgba(0, 0, 0, 0.4)');
  });
});

describe('cssSizeFor', () => {
  test('cover and contain map directly with no-repeat', () => {
    expect(cssSizeFor('cover')).toEqual({ size: 'cover', repeat: 'no-repeat' });
    expect(cssSizeFor('contain')).toEqual({ size: 'contain', repeat: 'no-repeat' });
  });
  test('tile becomes auto + repeat', () => {
    expect(cssSizeFor('tile')).toEqual({ size: 'auto', repeat: 'repeat' });
  });
});

describe('recreateDebounceDelay (V1.4-04 A.3)', () => {
  test('idx 0 → base 180 ms', () => {
    expect(recreateDebounceDelay(0)).toBe(180);
  });
  test('staggers by 30 ms per tab', () => {
    expect(recreateDebounceDelay(1)).toBe(210);
    expect(recreateDebounceDelay(2)).toBe(240);
    expect(recreateDebounceDelay(5)).toBe(330);
  });
  test('clamps stagger at idx=5 so high-tab counts cap at 330 ms', () => {
    expect(recreateDebounceDelay(6)).toBe(330);
    expect(recreateDebounceDelay(20)).toBe(330);
  });
  test('negative / unknown indices fall back to base', () => {
    expect(recreateDebounceDelay(-1)).toBe(180);
    expect(recreateDebounceDelay(-100)).toBe(180);
  });
});

describe('rgbaFrom', () => {
  test('6-digit hex parses correctly', () => {
    expect(rgbaFrom('#ff8000', 0.75)).toBe('rgba(255, 128, 0, 0.75)');
  });
  test('3-digit hex expands to 6-digit', () => {
    expect(rgbaFrom('#f80', 1)).toBe('rgba(255, 136, 0, 1)');
  });
  test('alpha is clamped into [0,1]', () => {
    expect(rgbaFrom('#000000', 1.5)).toBe('rgba(0, 0, 0, 1)');
    expect(rgbaFrom('#000000', -0.5)).toBe('rgba(0, 0, 0, 0)');
  });
  test('garbage hex falls back to black', () => {
    expect(rgbaFrom('not-a-color', 0.5)).toBe('rgba(0, 0, 0, 0.5)');
  });
});
