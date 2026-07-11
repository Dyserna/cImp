import { mount } from 'svelte';
import App from './App.svelte';
import { ttsTest } from './lib/ipc';
import { settings } from './lib/settings/store';
import { themeFromSetting, applyTerminalPaletteVars } from './lib/themes/resolve';
import { initThemeRegistry, themeMeta } from './lib/themes/registry';
import { installReloadBlocker } from './lib/shortcuts/blockReload';
import { getCurrentWindow } from '@tauri-apps/api/window';
import { invoke } from '@tauri-apps/api/core';
import './theme.css';
import './app.css';

// Set the active theme synchronously before Svelte mounts so the first
// paint already reflects token values — avoids FOUC. The static
// `data-theme` attribute on <html> (set in index.html) is a defense-in-
// depth fallback. Both default to "tui-blue" — matches the new-install
// default in defaultSettings(), and existing users have a persisted
// setting that overrides this on the first subscribe tick. The per-theme
// CSS itself is no longer bundled here: it's fetched from the backend and
// injected at runtime by initThemeRegistry() below, leaving only the base
// `:root` design tokens in theme.css.
document.documentElement.dataset.theme = 'tui-blue';

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
// metadata wins over the tui-blue fallback used before the fetch resolves.
let currentThemeId = 'tui-blue';
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

// Load the verified theme/palette registry, inject theme CSS, then re-apply the
// chrome with the real metadata now available.
void initThemeRegistry().finally(applyChrome);

// Follow the persisted ui.theme value: the store starts on defaults
// ("tui-blue"), then reflects the backend value once initSettings() runs
// from inside App.svelte. The subscription survives for the lifetime of the
// window. data-theme + the terminal palette vars apply immediately (FOUC
// avoidance); the decorations toggle is delegated to applyChrome().
settings.subscribe((s) => {
  currentThemeId = s.ui?.theme || 'tui-blue';
  if (document.documentElement.dataset.theme !== currentThemeId) {
    document.documentElement.dataset.theme = currentThemeId;
  }
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

const app = mount(App, { target });

export default app;
