import { mount } from 'svelte';
import SettingsApp from './SettingsApp.svelte';
import { settings } from './lib/settings/store';
import { themeFromSetting, applyTerminalPaletteVars } from './lib/themes/resolve';
import { initThemeRegistry, themeMeta } from './lib/themes/registry';
import { installReloadBlocker } from './lib/shortcuts/blockReload';
import { getCurrentWindow } from '@tauri-apps/api/window';
import './theme.css';
import './app.css';

document.documentElement.dataset.theme = 'tui-red';

// Match the main window: no user-triggered reload in the settings window either.
installReloadBlocker();

let currentThemeId = 'tui-red';
let lastDecorations: boolean | null = null;

function applyChrome() {
  const wantDecorations = themeMeta(currentThemeId).decorations;
  if (wantDecorations !== lastDecorations) {
    lastDecorations = wantDecorations;
    void getCurrentWindow().setDecorations(wantDecorations);
  }
}

// Load the verified theme/palette registry (injects theme CSS), then re-apply
// the chrome with the real metadata.
void initThemeRegistry().finally(applyChrome);

settings.subscribe((s) => {
  currentThemeId = s.ui?.theme || 'tui-red';
  if (document.documentElement.dataset.theme !== currentThemeId) {
    document.documentElement.dataset.theme = currentThemeId;
  }
  if (s.terminal?.theme) applyTerminalPaletteVars(themeFromSetting(s.terminal.theme));
  applyChrome();
});

const target = document.getElementById('app');
if (!target) {
  throw new Error('#app root element not found');
}

const app = mount(SettingsApp, { target });

export default app;
