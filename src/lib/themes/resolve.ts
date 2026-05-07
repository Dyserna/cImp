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

import { BUNDLED_THEMES, resolveBundledTheme, type ThemeColors } from './index';
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
    return { ...BUNDLED_THEMES.Default, ...(t.custom as Partial<ThemeColors>) };
  }
  return resolveBundledTheme(t.name);
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
