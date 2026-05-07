import { describe, expect, test } from 'vitest';
import {
  BUNDLED_THEMES,
  BUNDLED_THEME_NAMES,
  REQUIRED_THEME_KEYS,
  resolveBundledTheme,
} from './index';

describe('bundled theme registry', () => {
  test('every named theme has an entry', () => {
    for (const name of BUNDLED_THEME_NAMES) {
      expect(BUNDLED_THEMES[name], `missing entry for ${name}`).toBeDefined();
    }
  });

  test('every theme populates all 22 required keys', () => {
    for (const name of BUNDLED_THEME_NAMES) {
      const theme = BUNDLED_THEMES[name];
      for (const key of REQUIRED_THEME_KEYS) {
        const val = (theme as Record<string, unknown>)[key];
        expect(
          val,
          `${name} is missing or has empty value for "${key}"`,
        ).toMatch(/^#[0-9a-fA-F]{3,8}$/);
      }
    }
  });

  test('Default theme preserves the v1.3 hardcoded foreground/background', () => {
    // Phase 5's "invisibility test": once the resolver wires through to
    // terminals.ts, picking Default must produce the same fg/bg the
    // user saw in v1.3. Don't change these without thinking carefully.
    expect(BUNDLED_THEMES.Default.foreground).toBe('#e0e0e0');
    expect(BUNDLED_THEMES.Default.background).toBe('#000000');
  });

  test('resolveBundledTheme falls back to Default for unknown names', () => {
    expect(resolveBundledTheme('NonExistent')).toBe(BUNDLED_THEMES.Default);
    expect(resolveBundledTheme('')).toBe(BUNDLED_THEMES.Default);
  });

  test('resolveBundledTheme returns the right entry for known names', () => {
    expect(resolveBundledTheme('Dracula')).toBe(BUNDLED_THEMES.Dracula);
    expect(resolveBundledTheme('Solarized Dark')).toBe(BUNDLED_THEMES['Solarized Dark']);
  });
});
