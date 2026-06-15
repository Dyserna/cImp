import { describe, expect, test } from 'vitest';
import {
  FALLBACK_PALETTES,
  REQUIRED_THEME_KEYS,
  defaultPalette,
  paletteNames,
  resolveBundledTheme,
  setPalettes,
  type ThemeColors,
} from './index';

describe('fallback palettes', () => {
  test('the two compiled-in fallbacks exist', () => {
    expect(FALLBACK_PALETTES.Default).toBeDefined();
    expect(FALLBACK_PALETTES['Tomorrow Night']).toBeDefined();
  });

  test('every fallback palette populates all 22 required keys', () => {
    for (const [name, palette] of Object.entries(FALLBACK_PALETTES)) {
      for (const key of REQUIRED_THEME_KEYS) {
        const val = (palette as Record<string, unknown>)[key];
        expect(val, `${name} is missing or has empty value for "${key}"`).toMatch(
          /^#[0-9a-fA-F]{3,8}$/,
        );
      }
    }
  });

  test('Default palette preserves the v1.3 hardcoded foreground/background', () => {
    // The "invisibility test": picking Default must produce the same fg/bg the
    // user saw in v1.3. Don't change these without thinking carefully.
    expect(FALLBACK_PALETTES.Default.foreground).toBe('#e0e0e0');
    expect(FALLBACK_PALETTES.Default.background).toBe('#000000');
  });
});

describe('resolveBundledTheme / registry', () => {
  test('falls back to Default for unknown names', () => {
    expect(resolveBundledTheme('NonExistent')).toBe(defaultPalette());
    expect(resolveBundledTheme('')).toBe(defaultPalette());
  });

  test('setPalettes makes a backend palette resolvable and lists it', () => {
    const sunset: ThemeColors = { ...FALLBACK_PALETTES['Tomorrow Night'], red: '#ff0000' };
    setPalettes([{ name: 'Sunset', colors: sunset }]);
    expect(resolveBundledTheme('Sunset')).toBe(sunset);
    expect(paletteNames()).toContain('Sunset');
  });

  test('Default survives even if the backend list omits it', () => {
    // setPalettes always layers the backend list over the fallback, so the
    // Custom merge base (Default) is never lost.
    setPalettes([{ name: 'Only', colors: FALLBACK_PALETTES['Tomorrow Night'] }]);
    expect(resolveBundledTheme('Default')).toBe(FALLBACK_PALETTES.Default);
  });
});
