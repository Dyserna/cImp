import { beforeAll, describe, expect, test } from 'vitest';
import { defaultPalette, resolveBundledTheme, setPalettes } from './index';
import {
  effectiveTheme,
  themeFromSetting,
  type TerminalThemeSettingsLike,
  type TabWithThemeOverride,
} from './resolve';

// The palette registry is populated at runtime from the backend. Seed a couple
// of named palettes so the resolver has something beyond the built-in fallback
// to look up. `Default` is always retained by setPalettes (it's the Custom
// merge base), so we don't need to register it here.
beforeAll(() => {
  setPalettes([
    {
      name: 'Dracula',
      colors: {
        foreground: '#f8f8f2',
        background: '#282a36',
        cursor: '#f8f8f2',
        cursorAccent: '#282a36',
        selectionBackground: '#44475a',
        selectionForeground: '#f8f8f2',
        black: '#21222c',
        red: '#ff5555',
        green: '#50fa7b',
        yellow: '#f1fa8c',
        blue: '#bd93f9',
        magenta: '#ff79c6',
        cyan: '#8be9fd',
        white: '#f8f8f2',
        brightBlack: '#6272a4',
        brightRed: '#ff6e6e',
        brightGreen: '#69ff94',
        brightYellow: '#ffffa5',
        brightBlue: '#d6acff',
        brightMagenta: '#ff92df',
        brightCyan: '#a4ffff',
        brightWhite: '#ffffff',
      },
    },
    {
      name: 'Solarized Light',
      colors: {
        foreground: '#657b83',
        background: '#fdf6e3',
        cursor: '#657b83',
        cursorAccent: '#fdf6e3',
        selectionBackground: '#eee8d5',
        selectionForeground: '#586e75',
        black: '#073642',
        red: '#dc322f',
        green: '#859900',
        yellow: '#b58900',
        blue: '#268bd2',
        magenta: '#d33682',
        cyan: '#2aa198',
        white: '#eee8d5',
        brightBlack: '#002b36',
        brightRed: '#cb4b16',
        brightGreen: '#586e75',
        brightYellow: '#657b83',
        brightBlue: '#839496',
        brightMagenta: '#6c71c4',
        brightCyan: '#93a1a1',
        brightWhite: '#fdf6e3',
      },
    },
  ]);
});

const globalDracula: TerminalThemeSettingsLike = { name: 'Dracula', custom: null };
const globalDefault: TerminalThemeSettingsLike = { name: 'Default', custom: null };

describe('themeFromSetting', () => {
  test('returns the registered palette for a named entry', () => {
    expect(themeFromSetting({ name: 'Dracula', custom: null })).toBe(
      resolveBundledTheme('Dracula'),
    );
  });

  test('falls back to Default for an unknown name', () => {
    expect(themeFromSetting({ name: 'NotARealTheme', custom: null })).toBe(defaultPalette());
  });

  test('Custom with no custom payload still returns Default', () => {
    expect(themeFromSetting({ name: 'Custom', custom: null })).toBe(defaultPalette());
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
    expect(result.foreground).toBe(defaultPalette().foreground);
    expect(result.green).toBe(defaultPalette().green);
  });

  test('Custom merge does not mutate the bundled Default', () => {
    const before = defaultPalette().red;
    themeFromSetting({ name: 'Custom', custom: { red: '#abcdef' } });
    expect(defaultPalette().red).toBe(before);
  });
});

describe('effectiveTheme', () => {
  test('null override inherits the global theme', () => {
    const tab: TabWithThemeOverride = { theme_override: null };
    expect(effectiveTheme(tab, globalDracula)).toBe(resolveBundledTheme('Dracula'));
  });

  test('explicit override wins over global', () => {
    const tab: TabWithThemeOverride = {
      theme_override: { name: 'Solarized Light', custom: null },
    };
    expect(effectiveTheme(tab, globalDracula)).toBe(resolveBundledTheme('Solarized Light'));
  });

  test('Custom override is merged the same way as the global', () => {
    const tab: TabWithThemeOverride = {
      theme_override: { name: 'Custom', custom: { red: '#123456' } },
    };
    const result = effectiveTheme(tab, globalDefault);
    expect(result.red).toBe('#123456');
    expect(result.green).toBe(defaultPalette().green);
  });
});
