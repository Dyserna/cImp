import { mount } from 'svelte';
import App from './App.svelte';
import { ttsTest } from './lib/ipc';
import { settings } from './lib/settings/store';
import { themeFromSetting, applyTerminalPaletteVars } from './lib/themes/resolve';
import { installReloadBlocker } from './lib/shortcuts/blockReload';
import { getCurrentWindow } from '@tauri-apps/api/window';
import { invoke } from '@tauri-apps/api/core';
import './theme.css';
import './theme.tui-yellow.css';
import './theme.tui-purple.css';
import './theme.tui-orange.css';
import './app.css';

// Set the active theme synchronously before Svelte mounts so the first
// paint already reflects token values — avoids FOUC. The static
// `data-theme` attribute on <html> (set in index.html) is a defense-in-
// depth fallback. Both default to "tui-orange" — matches the new-install
// default in defaultSettings(), and existing users have a persisted
// setting that overrides this on the first subscribe tick.
document.documentElement.dataset.theme = 'tui-orange';

// Disable the webview's reload accelerators (F5 / Ctrl+R / Ctrl+Shift+R) so a
// stray keystroke can't tear every tab's PTY down and restart sessions.
installReloadBlocker();

// Follow the persisted ui.theme value: the store starts on defaults
// ("tui-yellow"), then reflects the backend value once initSettings()
// runs from inside App.svelte. The subscription survives for the
// lifetime of the window.
//
// Also drives OS chrome: modern-dark uses the platform's native title
// bar; every `tui-*` variant hides it (`setDecorations(false)`) and
// renders the custom `TuiTitleBar` Svelte component instead. The two
// stay in sync via this single subscription.
// The main window is created hidden (`visible: false` in tauri.conf.json) to
// avoid a blank WebView flashing on screen while the bundle loads and Svelte
// mounts. We reveal it exactly once, after the first decorations toggle has
// been applied below — so the user never sees the empty window nor a title-bar
// jump as the TUI themes drop the OS chrome.
let hasShownWindow = false;
function showMainWindowOnce() {
  if (hasShownWindow) return;
  hasShownWindow = true;
  void getCurrentWindow().show().catch(() => {});
}
// Safety net: if the decorations IPC round-trip never resolves, reveal anyway
// so a backend hiccup can't leave the user staring at nothing.
setTimeout(showMainWindowOnce, 3000);

let lastDecorations: boolean | null = null;
settings.subscribe((s) => {
  const next = s.ui?.theme || 'tui-orange';
  if (document.documentElement.dataset.theme !== next) {
    document.documentElement.dataset.theme = next;
  }
  if (s.terminal?.theme) applyTerminalPaletteVars(themeFromSetting(s.terminal.theme));
  const wantDecorations = !next.startsWith('tui-');
  if (wantDecorations !== lastDecorations) {
    lastDecorations = wantDecorations;
    // Square the window corners for the TUI themes (borderless), restore
    // the OS default rounding for modern-dark. Applied after the
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
