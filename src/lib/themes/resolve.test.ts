import { describe, expect, test } from 'vitest';
import { BUNDLED_THEMES } from './index';
import {
  effectiveTheme,
  themeFromSetting,
  type TerminalThemeSettingsLike,
  type TabWithThemeOverride,
} from './resolve';

const globalDracula: TerminalThemeSettingsLike = { name: 'Dracula', custom: null };
const globalDefault: TerminalThemeSettingsLike = { name: 'Default', custom: null };

describe('themeFromSetting', () => {
  test('returns the bundled theme for a named entry', () => {
    expect(themeFromSetting({ name: 'Dracula', custom: null })).toBe(
      BUNDLED_THEMES.Dracula,
    );
  });

  test('falls back to Default for an unknown name', () => {
    expect(themeFromSetting({ name: 'NotARealTheme', custom: null })).toBe(
      BUNDLED_THEMES.Default,
    );
  });

  test('Custom with no custom payload still returns Default', () => {
    expect(themeFromSetting({ name: 'Custom', custom: null })).toBe(
      BUNDLED_THEMES.Default,
    );
  });

  test('Custom merges user values over Default', () => {
    const result = themeFromSetting({
      name: 'Custom',
      custom: { red: '#ff00ff', background: '#1a2b3c' },
    });
    // Overridden keys win.
    expect(result.red).toBe('#ff00ff');
    expect(result.background).toBe('#1a2b3c');
    // Untouched keys keep Default's values.
    expect(result.foreground).toBe(BUNDLED_THEMES.Default.foreground);
    expect(result.green).toBe(BUNDLED_THEMES.Default.green);
  });

  test('Custom merge does not mutate the bundled Default', () => {
    const before = BUNDLED_THEMES.Default.red;
    themeFromSetting({ name: 'Custom', custom: { red: '#abcdef' } });
    expect(BUNDLED_THEMES.Default.red).toBe(before);
  });
});

describe('effectiveTheme', () => {
  test('null override inherits the global theme', () => {
    const tab: TabWithThemeOverride = { theme_override: null };
    expect(effectiveTheme(tab, globalDracula)).toBe(BUNDLED_THEMES.Dracula);
  });

  test('explicit override wins over global', () => {
    const tab: TabWithThemeOverride = {
      theme_override: { name: 'Solarized Light', custom: null },
    };
    expect(effectiveTheme(tab, globalDracula)).toBe(BUNDLED_THEMES['Solarized Light']);
  });

  test('Custom override is merged the same way as the global', () => {
    const tab: TabWithThemeOverride = {
      theme_override: { name: 'Custom', custom: { red: '#123456' } },
    };
    const result = effectiveTheme(tab, globalDefault);
    expect(result.red).toBe('#123456');
    expect(result.green).toBe(BUNDLED_THEMES.Default.green);
  });
});
