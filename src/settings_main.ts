import { mount } from 'svelte';
import SettingsApp from './SettingsApp.svelte';
import { settings } from './lib/settings/store';
import { installReloadBlocker } from './lib/shortcuts/blockReload';
import { getCurrentWindow } from '@tauri-apps/api/window';
import './theme.css';
import './theme.tui-yellow.css';
import './theme.tui-purple.css';
import './theme.tui-orange.css';
import './app.css';

document.documentElement.dataset.theme = 'tui-yellow';

// Match the main window: no user-triggered reload in the settings window either.
installReloadBlocker();

let lastDecorations: boolean | null = null;
settings.subscribe((s) => {
  const next = s.ui?.theme || 'tui-yellow';
  if (document.documentElement.dataset.theme !== next) {
    document.documentElement.dataset.theme = next;
  }
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
