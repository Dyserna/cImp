import { mount } from 'svelte';
import App from './App.svelte';
import { ttsTest } from './lib/ipc';
import { settings } from './lib/settings/store';
import { themeFromSetting, applyTerminalPaletteVars } from './lib/themes/resolve';
import { initThemeRegistry, themeMeta } from './lib/themes/registry';
import { applyTuiAccent, TUI_THEME_ID } from './lib/themes/accent';
import { installReloadBlocker } from './lib/shortcuts/blockReload';
import { getCurrentWindow } from '@tauri-apps/api/window';
import { invoke } from '@tauri-apps/api/core';
import './theme.css';
import './app.css';
import { loadHarnesses } from './lib/harness';
import { hydrateUiState } from './lib/uiState';
import { hydrateHiddenTabs } from './lib/tabs/visibility';

// Set the active theme synchronously before Svelte mounts so the first
// paint already reflects token values — avoids FOUC. The static
// `data-theme` attribute on <html> (set in index.html) is a defense-in-
// depth fallback. Both default to "tui" — the built-in theme and the
// new-install default in defaultSettings(); existing users have a
// persisted setting that overrides this on the first subscribe tick. The
// per-theme CSS itself is no longer bundled here: it's fetched from the
// backend and injected at runtime by initThemeRegistry() below, leaving
// only the base `:root` design tokens in theme.css.
document.documentElement.dataset.theme = TUI_THEME_ID;

// Disable the webview's reload accelerators (F5 / Ctrl+R / Ctrl+Shift+R) so a
// stray keystroke can't tear every tab's PTY down and restart sessions.
installReloadBlocker();

// The main window is created hidden (`visible: false` in tauri.conf.json) to
// avoid a blank WebView flashing on screen while the bundle loads and Svelte
// mounts. We reveal it exactly once, after the first decorations pass has been
// applied — so the user never sees the empty window nor a title-bar jump as
// the TUI themes drop the OS chrome.
let hasShownWindow = false;
function showMainWindowOnce() {
  if (hasShownWindow) return;
  hasShownWindow = true;
  void getCurrentWindow().show().catch(() => {});
}
// Safety net: if the decorations IPC round-trip never resolves, reveal anyway
// so a backend hiccup can't leave the user staring at nothing.
setTimeout(showMainWindowOnce, 3000);

// Whether the active theme uses the OS-native window chrome is now read from
// the theme's metadata (`decorations`) rather than its name. The chrome is
// (re)applied both when the settings theme changes and when the theme registry
// finishes loading — the latter so a non-default persisted theme's real
// metadata wins over the built-in fallback used before the fetch resolves.
let currentThemeId: string = TUI_THEME_ID;
let lastDecorations: boolean | null = null;

function applyChrome() {
  const wantDecorations = themeMeta(currentThemeId).decorations;
  if (wantDecorations !== lastDecorations) {
    lastDecorations = wantDecorations;
    // Square the window corners for the TUI themes (borderless), restore
    // the OS default rounding for native-chrome themes. Applied after the
    // decorations toggle since changing decorations can reset DWM attrs.
    void getCurrentWindow()
      .setDecorations(wantDecorations)
      .finally(() => {
        void invoke('set_window_square_corners', { square: !wantDecorations })
          .catch(() => {})
          // First decorations pass is done — the window is now in its final
          // chrome state, so it's safe to reveal it.
          .finally(() => showMainWindowOnce());
      });
  } else {
    showMainWindowOnce();
  }
}

// V40 Phase F (locked decision 7): the harness roster, fetched once, as early
// as the window can ask. Everything downstream — which tab ids are AI builtins,
// what each harness is called, its accent, its install hint — is this answer.
// Started here rather than from a component so the gap between mount and the
// first registry-aware paint is as short as the IPC allows; until it lands the
// window renders neutral rather than guessing (`harness.ts` documents the one
// synchronous fallback).
void loadHarnesses();

// Load the verified theme/palette registry, inject theme CSS, then re-apply the
// chrome with the real metadata now available.
void initThemeRegistry().finally(applyChrome);

// Follow the persisted ui.theme value: the store starts on defaults
// ("tui"), then reflects the backend value once initSettings() runs
// from inside App.svelte. The subscription survives for the lifetime of the
// window. data-theme + the terminal palette vars + the TUI accent apply
// immediately (FOUC avoidance); the decorations toggle is delegated to
// applyChrome().
settings.subscribe((s) => {
  currentThemeId = s.ui?.theme || TUI_THEME_ID;
  if (document.documentElement.dataset.theme !== currentThemeId) {
    document.documentElement.dataset.theme = currentThemeId;
  }
  applyTuiAccent(s.ui?.tui_accent);
  if (s.terminal?.theme) applyTerminalPaletteVars(themeFromSetting(s.terminal.theme));
  applyChrome();
});

// Dev-only TTS test handle: `window.ttsTest("hello")` from DevTools synth-
// esizes through the active tab's pipeline. The backend routes the request
// to whichever tab is currently active.
// @ts-expect-error - dev-only debug surface
window.ttsTest = (text: string) => ttsTest(text).catch(console.error);

const target = document.getElementById('app');
if (!target) {
  throw new Error('#app root element not found');
}

// V42 Phase C: per-view UI state (last-selected sections, expanded cards,
// Events column widths, audit filters, the UI-hidden tab set) lives in the
// per-project `.cimp/ui_state.json`. Its consumers all read synchronously
// from `$state(...)` initialisers, before the first paint — so the one
// asynchronous read happens HERE, blocking, and everything downstream reads a
// filled cache. Two ordering guarantees hang off these two lines:
//
//   1. `mount(App)` must not run first, or every view would paint its default
//      section/collapsed card and snap to the saved one a frame later.
//   2. `hydrateHiddenTabs()` must not run later than App's `onMount`, where
//      `stripHiddenTabsFromLayout()` re-establishes "hidden ⇔ absent from the
//      layout tree". An empty set at that moment would un-hide every hidden
//      tab — and the popover's next write would then persist the empty set,
//      losing the user's choice for good.
//
// Neither call rejects: a backend that cannot answer leaves the window
// unhydrated, which renders defaults and writes nothing (see `uiState.ts`).
//
// And neither HANGS (V42 review, RV-3). Gating the mount on an unbounded file
// read meant a stalled network share, a locked file or a backend wedged before
// managed state came up produced a window that was revealed by
// `showMainWindowOnce`'s 3 s net and then never mounted anything into it.
// `hydrateUiState` carries its own 2 s budget and gives up on defaults; the
// window it gives up on is write-INERT, which is what keeps guarantee 2 above
// intact — an empty hidden-tab set can un-hide tabs for that session, but the
// popover's write cannot persist the emptiness back over the user's choice.
await hydrateUiState();
hydrateHiddenTabs();

const app = mount(App, { target });

export default app;
