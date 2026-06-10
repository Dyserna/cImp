import { mount } from 'svelte';
import SettingsApp from './SettingsApp.svelte';
import { settings } from './lib/settings/store';
import { themeFromSetting } from './lib/themes/resolve';
import { installReloadBlocker } from './lib/shortcuts/blockReload';
import { getCurrentWindow } from '@tauri-apps/api/window';
import './theme.css';
import './theme.tui-yellow.css';
import './theme.tui-purple.css';
import './theme.tui-orange.css';
import './app.css';

document.documentElement.dataset.theme = 'tui-orange';

// Match the main window: no user-triggered reload in the settings window either.
installReloadBlocker();

// Mirror main.ts: publish the global terminal palette as --term-bg/--term-fg
// so the settings window chrome integrates with the terminal colors too.
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
    void getCurrentWindow().setDecorations(wantDecorations);
  }
});

const target = document.getElementById('app');
if (!target) {
  throw new Error('#app root element not found');
}

const app = mount(SettingsApp, { target });

export default app;
