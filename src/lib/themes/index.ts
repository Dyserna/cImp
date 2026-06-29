// Terminal-palette registry.
//
// Palettes are no longer hardcoded here. They live as one JSON file per
// palette under `<exe-dir>/palettes/`, are verified by the Rust backend, and
// fetched once at startup via the `palettes_list` IPC (see ./registry.ts),
// which calls `setPalettes` to populate the module-level map below.
//
// A few palettes are compiled into this bundle as a fallback (`FALLBACK_PALETTES`)
// so the resolver returns a complete xterm.js `ITheme` *synchronously* — before
// the backend registry has loaded, and after any IPC failure: `Default` (the
// merge base for "Custom" palettes; its fg/bg preserves the pre-V1.4 hardcoded
// look), `GitHub Dark` (the default terminal palette, paired with the default
// tui-orange theme and embedded as the backend fallback), and `Tomorrow Night`.

import type { ITheme } from '@xterm/xterm';

export type ThemeColors = ITheme;

/// Palettes compiled into the frontend bundle as the always-present fallback.
export const FALLBACK_PALETTES: Record<string, ThemeColors> = {
  Default: {
    foreground: '#e0e0e0',
    background: '#000000',
    cursor: '#e0e0e0',
    cursorAccent: '#000000',
    selectionBackground: '#3a3a3a',
    selectionForeground: '#ffffff',
    black: '#000000',
    red: '#cd3131',
    green: '#0dbc79',
    yellow: '#e5e510',
    blue: '#2472c8',
    magenta: '#bc3fbc',
    cyan: '#11a8cd',
    white: '#e5e5e5',
    brightBlack: '#666666',
    brightRed: '#f14c4c',
    brightGreen: '#23d18b',
    brightYellow: '#f5f543',
    brightBlue: '#3b8eea',
    brightMagenta: '#d670d6',
    brightCyan: '#29b8db',
    brightWhite: '#ffffff',
  },
  'Tomorrow Night': {
    foreground: '#c5c8c6',
    background: '#1d1f21',
    cursor: '#c5c8c6',
    cursorAccent: '#1d1f21',
    selectionBackground: '#373b41',
    selectionForeground: '#c5c8c6',
    black: '#1d1f21',
    red: '#cc6666',
    green: '#b5bd68',
    yellow: '#f0c674',
    blue: '#81a2be',
    magenta: '#b294bb',
    cyan: '#8abeb7',
    white: '#c5c8c6',
    brightBlack: '#969896',
    brightRed: '#cc6666',
    brightGreen: '#b5bd68',
    brightYellow: '#f0c674',
    brightBlue: '#81a2be',
    brightMagenta: '#b294bb',
    brightCyan: '#8abeb7',
    brightWhite: '#ffffff',
  },
  'GitHub Dark': {
    foreground: '#c9d1d9',
    background: '#0d1117',
    cursor: '#c9d1d9',
    cursorAccent: '#0d1117',
    selectionBackground: '#264f78',
    selectionForeground: '#c9d1d9',
    black: '#484f58',
    red: '#ff7b72',
    green: '#3fb950',
    yellow: '#d29922',
    blue: '#58a6ff',
    magenta: '#bc8cff',
    cyan: '#39c5cf',
    white: '#b1bac4',
    brightBlack: '#6e7681',
    brightRed: '#ffa198',
    brightGreen: '#56d364',
    brightYellow: '#e3b341',
    brightBlue: '#79c0ff',
    brightMagenta: '#d2a8ff',
    brightCyan: '#56d4dd',
    brightWhite: '#f0f6fc',
  },
};

// Live palette map. Seeded with the fallback and replaced wholesale once the
// backend `palettes_list` resolves. Read synchronously by `resolveBundledTheme`
// (the resolver runs on the terminal hot path and can't await an IPC).
let paletteMap: Record<string, ThemeColors> = { ...FALLBACK_PALETTES };
// Display order for the Settings dropdown — the backend's order (sorted by
// name), or the fallback keys before it has loaded.
let paletteOrder: string[] = Object.keys(FALLBACK_PALETTES);

/// Replace the live palette map from the backend registry. Fallback palettes
/// are always retained underneath so `Default` (the Custom merge base),
/// `GitHub Dark` (the default terminal palette), and `Tomorrow Night` survive
/// even if the disk folder somehow omits them.
export function setPalettes(palettes: { name: string; colors: ThemeColors }[]): void {
  const map: Record<string, ThemeColors> = { ...FALLBACK_PALETTES };
  for (const p of palettes) map[p.name] = p.colors;
  paletteMap = map;
  paletteOrder = palettes.length ? palettes.map((p) => p.name) : Object.keys(FALLBACK_PALETTES);
}

/// Names of the currently-available palettes, in display order. Used by the
/// Settings palette dropdown.
export function paletteNames(): string[] {
  return paletteOrder;
}

/// Look up a palette by name. Falls back to `Default` for an unrecognised name
/// — protects against a settings.json palette name from a deleted/renamed file.
export function resolveBundledTheme(name: string): ThemeColors {
  return paletteMap[name] ?? paletteMap.Default ?? FALLBACK_PALETTES.Default;
}

/// The `Default` palette — the merge base a "Custom" palette layers over.
export function defaultPalette(): ThemeColors {
  return paletteMap.Default ?? FALLBACK_PALETTES.Default;
}

/// The 22 keys every palette is required to populate. Mirrors the Rust-side
/// `REQUIRED_PALETTE_KEYS`; used by `CustomThemeEditor` and the frontend tests.
export const REQUIRED_THEME_KEYS: readonly (keyof ThemeColors)[] = [
  'foreground',
  'background',
  'cursor',
  'cursorAccent',
  'selectionBackground',
  'selectionForeground',
  'black',
  'red',
  'green',
  'yellow',
  'blue',
  'magenta',
  'cyan',
  'white',
  'brightBlack',
  'brightRed',
  'brightGreen',
  'brightYellow',
  'brightBlue',
  'brightMagenta',
  'brightCyan',
  'brightWhite',
];
