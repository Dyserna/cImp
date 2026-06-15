// Runtime theme/palette registry — the frontend half of the externalized
// theming system.
//
// On startup it fetches the verified registries from the backend (`themes_list`
// + `palettes_list`), pushes the palettes into ./index's synchronous map,
// injects each UI theme's CSS into <head>, and exposes the theme list to the
// Settings UI plus a synchronous `themeMeta(id)` lookup used by the window
// entry points to drive `data-theme` and the OS-chrome (decorations) toggle.
//
// Everything degrades to a built-in `tui-orange` fallback: the backend always
// includes the embedded tui-orange theme even when the on-disk folder is empty,
// and if the IPC itself fails we still hold the metadata fallback below so the
// chrome logic never sees `undefined`.

import { invoke } from '@tauri-apps/api/core';
import { writable, type Readable } from 'svelte/store';
import { FALLBACK_PALETTES, setPalettes, type ThemeColors } from './index';

/// A UI theme as delivered by the backend: metadata plus the CSS to inject.
export interface ThemeEntry {
  id: string;
  name: string;
  /// true = OS-native window chrome; false = hide it and render TuiTitleBar.
  decorations: boolean;
  /// Terminal palette this theme pairs with on switch (unless palette is Custom).
  palette: string;
  css: string;
}

export interface PaletteWire {
  name: string;
  colors: ThemeColors;
}

/// Metadata fallback used before the registry loads and if the fetch fails.
/// Matches the embedded tui-orange theme.json on the Rust side. `css` is empty
/// here — the real CSS arrives from the backend (or, in the empty-folder case,
/// from the backend's embedded copy); a fetch failure leaves the base
/// `theme.css` :root tokens in effect, which is a usable degraded state.
const FALLBACK_THEME: ThemeEntry = {
  id: 'tui-orange',
  name: 'TUI - Orange',
  decorations: false,
  palette: 'Tomorrow Night',
  css: '',
};

const themesStore = writable<ThemeEntry[]>([FALLBACK_THEME]);
/// The available UI themes, for the Settings → Theme dropdown.
export const themeRegistry: Readable<ThemeEntry[]> = themesStore;

// The available terminal palettes, for the palette dropdowns + swatches.
// Seeded from ./index's FALLBACK_PALETTES so the dropdown is never empty, then
// replaced once `palettes_list` resolves. Components read this reactively so a
// swatch / option list re-renders when the registry finishes loading.
const palettesStore = writable<PaletteWire[]>(
  Object.entries(FALLBACK_PALETTES).map(([name, colors]) => ({ name, colors })),
);
export const paletteRegistry: Readable<PaletteWire[]> = palettesStore;

let byId = new Map<string, ThemeEntry>([[FALLBACK_THEME.id, FALLBACK_THEME]]);

/// Synchronous metadata lookup. Unknown ids fall back to tui-orange so the
/// chrome logic (decorations toggle, palette pairing) always has an answer.
export function themeMeta(id: string): ThemeEntry {
  return byId.get(id) ?? byId.get(FALLBACK_THEME.id) ?? FALLBACK_THEME;
}

let initPromise: Promise<void> | null = null;

/// Fetch + apply the backend registries. Idempotent: subsequent calls return
/// the same promise. Resolves once palettes are in the resolver map and theme
/// CSS is injected, so callers can re-apply chrome afterwards.
export function initThemeRegistry(): Promise<void> {
  if (initPromise) return initPromise;
  initPromise = (async () => {
    try {
      const [themes, palettes] = await Promise.all([
        invoke<ThemeEntry[]>('themes_list'),
        invoke<PaletteWire[]>('palettes_list'),
      ]);
      if (palettes?.length) {
        setPalettes(palettes);
        palettesStore.set(palettes);
      }
      if (themes?.length) {
        byId = new Map(themes.map((t) => [t.id, t]));
        injectThemeCss(themes);
        themesStore.set(themes);
      }
    } catch (e) {
      console.warn('theme registry load failed; using built-in fallback', e);
    }
  })();
  return initPromise;
}

/// Inject one <style> per theme into <head>. All themes coexist; the active one
/// is selected by the `data-theme` attribute on <html> (the per-theme
/// `[data-theme="<id>"]` selector only matches when it's active) — exactly the
/// mechanism the old static CSS imports relied on. Idempotent: re-injecting
/// updates the existing element in place.
function injectThemeCss(themes: ThemeEntry[]): void {
  for (const t of themes) {
    if (!t.css) continue;
    const elementId = `cctts-theme-${t.id}`;
    let el = document.getElementById(elementId) as HTMLStyleElement | null;
    if (!el) {
      el = document.createElement('style');
      el.id = elementId;
      el.dataset.ccttsTheme = t.id;
      document.head.appendChild(el);
    }
    el.textContent = t.css;
  }
}
