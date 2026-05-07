// V1.4-02 background resolver.
//
// Two layers of resolution land here:
//
//   1. Three-state override:
//        background_override === null         → inherit global
//        background_override === 'disabled'   → opt out (theme bg only)
//        background_override === { ...cfg }   → use this config
//
//   2. Four-cell matrix on the resolved config:
//        image: None, color: None  → mode 'none'   (canvas, theme bg)
//        image: None, color: Some  → mode 'color'  (canvas, theme bg = color)
//        image: Some, color: None  → mode 'image'  (DOM, theme bg = rgba(black, opacity))
//        image: Some, color: Some  → mode 'image'  (DOM, theme bg = rgba(color, opacity))
//
// `effectiveBackgroundMode` is the single entry point. The discriminated
// `RenderingMode` it returns drives every downstream decision:
//   - canvas vs DOM renderer at construction (terminals.ts Step 5)
//   - theme background override for the xterm.js theme (composeTheme)
//   - CSS image / blur / size / position on the host element
//     (applyHostBackgroundCss)
//   - whether a settings change triggers a Terminal recreate vs an
//     in-place update (categoryOf)

import { convertFileSrc } from '@tauri-apps/api/core';
import type { ITheme } from '@xterm/xterm';
import {
  isBackgroundDisabled,
  type BackgroundOverrideWire,
  type TerminalBackgroundSettings,
} from '../settings/types';

/// Discriminated rendering mode. Construction-time decision and the
/// shape every downstream call branches on.
export type RenderingMode =
  | { kind: 'none' }
  | { kind: 'color'; color: string }
  | {
      kind: 'image';
      cfg: TerminalBackgroundSettings;
      /// Tint color for the dimming overlay. `null` means "use black"
      /// per the milestone doc; the resolver passes through whatever
      /// the user set.
      tint: string | null;
    };

/// Tab shape consumed by the resolver — minimal contract so both
/// `AiToolTabConfig` and `ShellTabConfig` satisfy it.
export interface TabWithBackgroundOverride {
  background_override: BackgroundOverrideWire | null;
}

/// Resolve a tab's effective rendering mode. Three-state override is
/// applied first, then the four-cell matrix on the resolved config.
export function effectiveBackgroundMode(
  tab: TabWithBackgroundOverride,
  global: TerminalBackgroundSettings,
): RenderingMode {
  // Three-state override.
  if (isBackgroundDisabled(tab.background_override)) return { kind: 'none' };
  const cfg: TerminalBackgroundSettings =
    tab.background_override ?? global;

  // Four-cell matrix. Image presence dominates; color-only is the
  // canvas-fast-path branch.
  if (cfg.image) return { kind: 'image', cfg, tint: cfg.color };
  if (cfg.color) return { kind: 'color', color: cfg.color };
  return { kind: 'none' };
}

/// Renderer category: drives the recreate-vs-update decision in the
/// settings subscriber. Image mode forces DOM; everything else stays
/// on the canvas fast path.
export function categoryOf(mode: RenderingMode): 'fast' | 'image' {
  return mode.kind === 'image' ? 'image' : 'fast';
}

/// Apply the rendering mode to an xterm.js `ITheme`. The original
/// theme is not mutated — a shallow copy is returned with the
/// background field replaced (or left alone for `'none'`).
export function composeTheme(theme: ITheme, mode: RenderingMode): ITheme {
  if (mode.kind === 'none') return theme;
  if (mode.kind === 'color') return { ...theme, background: mode.color };
  // Image mode: the xterm.js theme bg becomes a translucent overlay
  // tinted to either the user's color or black.
  return {
    ...theme,
    background: rgbaFrom(mode.tint ?? '#000000', mode.cfg.opacity),
  };
}

/// CSS surface application. Idempotent — calling repeatedly with the
/// same mode is a no-op (or a cheap reassignment of identical values).
/// Removes the image-mode class and clears custom properties when the
/// mode is not image, so a transition from image → color cleans up
/// after itself without DOM restructuring.
export function applyHostBackgroundCss(
  host: HTMLDivElement,
  mode: RenderingMode,
): void {
  if (mode.kind !== 'image') {
    host.classList.remove('bg-image');
    host.style.removeProperty('--bg-image');
    host.style.removeProperty('--bg-size');
    host.style.removeProperty('--bg-repeat');
    host.style.removeProperty('--bg-position');
    host.style.removeProperty('--bg-blur');
    return;
  }
  const { cfg } = mode;
  host.classList.add('bg-image');
  host.style.setProperty(
    '--bg-image',
    `url("${pathToAssetUrl(cfg.image!)}")`,
  );
  const { size, repeat } = cssSizeFor(cfg.size);
  host.style.setProperty('--bg-size', size);
  host.style.setProperty('--bg-repeat', repeat);
  host.style.setProperty('--bg-position', cfg.position);
  host.style.setProperty('--bg-blur', `${cfg.blur}px`);
}

/// Map our `BackgroundSize` enum to CSS `background-size` +
/// `background-repeat`. `tile` is the only entry that needs the pair —
/// `cover` and `contain` map directly with `background-repeat: no-repeat`.
export function cssSizeFor(size: TerminalBackgroundSettings['size']): {
  size: string;
  repeat: string;
} {
  if (size === 'tile') return { size: 'auto', repeat: 'repeat' };
  return { size, repeat: 'no-repeat' };
}

/// Convert an absolute filesystem path to a webview-safe asset URL.
/// Tauri's `convertFileSrc` produces an `https://asset.localhost/…`
/// URL on Windows / Linux and a `tauri://localhost/…` URL on macOS;
/// the CSP allow-list in `tauri.conf.json` lets these load. Plain
/// `file://` URLs are blocked by the webview's CSP and would fail
/// silently with a console warning.
export function pathToAssetUrl(absolutePath: string): string {
  return convertFileSrc(absolutePath);
}

/// Convert a `#rgb` / `#rrggbb` hex color + alpha (0-1) to an
/// `rgba(r,g,b,a)` CSS string. Invalid input falls back to black —
/// xterm.js tolerates the result either way; we just don't crash on
/// hand-edited settings files.
export function rgbaFrom(hex: string, alpha: number): string {
  let r = 0;
  let g = 0;
  let b = 0;
  const m3 = /^#([0-9a-f])([0-9a-f])([0-9a-f])$/i.exec(hex);
  const m6 = /^#([0-9a-f]{2})([0-9a-f]{2})([0-9a-f]{2})$/i.exec(hex);
  if (m6) {
    r = parseInt(m6[1], 16);
    g = parseInt(m6[2], 16);
    b = parseInt(m6[3], 16);
  } else if (m3) {
    r = parseInt(m3[1] + m3[1], 16);
    g = parseInt(m3[2] + m3[2], 16);
    b = parseInt(m3[3] + m3[3], 16);
  }
  const a = Math.max(0, Math.min(1, alpha));
  return `rgba(${r}, ${g}, ${b}, ${a})`;
}
