import { mount } from 'svelte';
import App from './App.svelte';
import { ttsTest } from './lib/ipc';
import { settings } from './lib/settings/store';
import { themeFromSetting } from './lib/themes/resolve';
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
// Publish the active GLOBAL terminal palette as CSS custom properties so the
// chrome (tab/bar backgrounds via --surface-*, body text via --text-primary)
// integrates with the terminal colors. Theme CSS references these with the
// per-theme value as a fallback, so an unset var leaves the original look.
// Per-tab palette overrides intentionally do NOT drive the chrome — there's
// one tab bar / status bar shared across tabs, so it follows the global pick.
function applyTerminalPaletteVars(theme: ReturnType<typeof themeFromSetting>) {
  const root = document.documentElement.style;
  if (theme.background) root.setProperty('--term-bg', theme.background);
  if (theme.foreground) root.setProperty('--term-fg', theme.foreground);
}

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
        void invoke('set_window_square_corners', { square: !wantDecorations }).catch(() => {});
      });
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
