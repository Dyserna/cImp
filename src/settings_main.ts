import { mount } from 'svelte';
import SettingsApp from './SettingsApp.svelte';
import { settings } from './lib/settings/store';
import { themeFromSetting, applyTerminalPaletteVars } from './lib/themes/resolve';
import { initThemeRegistry, themeMeta } from './lib/themes/registry';
import { applyTuiAccent, TUI_THEME_ID } from './lib/themes/accent';
import { installReloadBlocker } from './lib/shortcuts/blockReload';
import { getCurrentWindow } from '@tauri-apps/api/window';
import './theme.css';
import './app.css';

document.documentElement.dataset.theme = TUI_THEME_ID;

// Match the main window: no user-triggered reload in the settings window either.
installReloadBlocker();

// The settings window is created hidden (`.visible(false)` in
// `windows.rs::open_or_focus_settings`) to avoid the white WebView flash
// while the bundle loads, plus the title-bar blink as the TUI themes drop
// the OS chrome. Reveal exactly once, after the first decorations pass —
// same pattern as `main.ts`.
let hasShownWindow = false;
function showWindowOnce() {
  if (hasShownWindow) return;
  hasShownWindow = true;
  void getCurrentWindow()
    .show()
    .then(() => getCurrentWindow().setFocus())
    .catch(() => {});
}
// Safety net: if the decorations IPC round-trip never resolves, reveal anyway
// so a backend hiccup can't leave the user staring at nothing.
setTimeout(showWindowOnce, 3000);

let currentThemeId: string = TUI_THEME_ID;
let lastDecorations: boolean | null = null;

function applyChrome() {
  const wantDecorations = themeMeta(currentThemeId).decorations;
  if (wantDecorations !== lastDecorations) {
    lastDecorations = wantDecorations;
    void getCurrentWindow()
      .setDecorations(wantDecorations)
      // First decorations pass is done — the chrome is in its final state,
      // safe to reveal.
      .finally(showWindowOnce);
  } else {
    showWindowOnce();
  }
}

// Load the verified theme/palette registry (injects theme CSS), then re-apply
// the chrome with the real metadata.
void initThemeRegistry().finally(applyChrome);

settings.subscribe((s) => {
  currentThemeId = s.ui?.theme || TUI_THEME_ID;
  if (document.documentElement.dataset.theme !== currentThemeId) {
    document.documentElement.dataset.theme = currentThemeId;
  }
  applyTuiAccent(s.ui?.tui_accent);
  if (s.terminal?.theme) applyTerminalPaletteVars(themeFromSetting(s.terminal.theme));
  applyChrome();
});

const target = document.getElementById('app');
if (!target) {
  throw new Error('#app root element not found');
}

const app = mount(SettingsApp, { target });

export default app;
