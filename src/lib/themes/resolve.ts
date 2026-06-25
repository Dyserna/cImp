// Theme resolution for V1.4-01.
//
// Two functions, both pure:
//
//   themeFromSetting(t)        — turn a `TerminalThemeSettings` (a name
//                                 plus optional Custom block) into a
//                                 concrete xterm.js `ITheme`.
//   effectiveTheme(tab, global) — apply per-tab override semantics:
//                                 if the tab has a `theme_override`,
//                                 use it; otherwise fall back to the
//                                 global setting.
//
// The Custom path merges over the bundled "Default" so partial overrides
// (e.g., the user only changed `red`) still produce a complete palette
// instead of leaving xterm.js with a half-undefined theme. The user's
// custom hex values win for any keys they set.

import { defaultPalette, resolveBundledTheme, type ThemeColors } from './index';
import type { TerminalThemeSettings } from '../settings/types';

// Re-exported under the historical name so the test file (and any
// future callers) don't need to chase the type around. The settings
// `TerminalThemeSettings` is the canonical definition.
export type TerminalThemeSettingsLike = TerminalThemeSettings;

// Minimal tab shape — the resolver only needs the override field. Both
// `AiToolTabConfig` and `ShellTabConfig` satisfy this.
export interface TabWithThemeOverride {
  theme_override: TerminalThemeSettings | null;
}

/// Resolve a `TerminalThemeSettings` into a concrete xterm.js `ITheme`.
/// "Custom" merges over `Default` so omitted keys fall back gracefully.
/// Any other name looks up the bundled registry; an unknown name falls
/// back to Default rather than throwing.
export function themeFromSetting(t: TerminalThemeSettingsLike): ThemeColors {
  if (t.name === 'Custom' && t.custom) {
    // Project `custom` through the KNOWN palette keys, accepting only valid
    // `#hex` values. `custom` comes from settings.json (hand-editable) and its
    // wire type is an open string map, so a blanket spread would leak
    // arbitrary/misspelled keys — and, worse, a non-color string on a real key
    // — straight into the xterm.js ITheme. Anything invalid falls back to the
    // default channel.
    // Clone — `defaultPalette()` returns the shared bundled Default object, so
    // mutating it in place would corrupt every later resolve.
    const base = { ...defaultPalette() };
    const custom = t.custom as Record<string, unknown>;
    // Write through a string-valued view: every palette channel we set is a
    // color string, but `ThemeColors` (xterm `ITheme`) also carries the
    // `string[]` `extendedAnsi` key, which widens an indexed write to
    // `string & string[]`. We only ever assign validated `#hex` strings here.
    const writable = base as Record<string, string>;
    for (const key of Object.keys(base) as (keyof ThemeColors)[]) {
      const v = custom[key as string];
      if (typeof v === 'string' && isHexColor(v)) {
        writable[key as string] = v;
      }
    }
    return base;
  }
  return resolveBundledTheme(t.name);
}

/// A valid CSS hex color: `#` followed by exactly 3, 4, 6, or 8 hex digits.
/// Mirrors the backend `theming::valid_hex` gate.
function isHexColor(s: string): boolean {
  return /^#(?:[0-9a-fA-F]{3}|[0-9a-fA-F]{4}|[0-9a-fA-F]{6}|[0-9a-fA-F]{8})$/.test(s);
}

/// Tab-aware resolver. If the tab carries an explicit `theme_override`,
/// the override wins; otherwise the global theme applies. Override is the
/// *whole* TerminalThemeSettings — not per-field — so a tab that opts in
/// supplies its own complete palette decision.
export function effectiveTheme(
  tab: TabWithThemeOverride,
  global: TerminalThemeSettingsLike,
): ThemeColors {
  return themeFromSetting(tab.theme_override ?? global);
}

/// Publish the active GLOBAL terminal palette as `--term-bg` / `--term-fg`
/// CSS custom properties on <html> so the app chrome (tab/bar backgrounds,
/// body text) integrates with the terminal colors. Theme CSS references
/// these with the per-theme value as a fallback, so an unset var leaves the
/// original look. Shared by both window entry points (main + settings).
export function applyTerminalPaletteVars(theme: ThemeColors): void {
  const root = document.documentElement.style;
  if (theme.background) root.setProperty('--term-bg', theme.background);
  if (theme.foreground) root.setProperty('--term-fg', theme.foreground);
}
