import { mount } from 'svelte';
import SettingsApp from './SettingsApp.svelte';
import { settings } from './lib/settings/store';
import { getCurrentWindow } from '@tauri-apps/api/window';
import './theme.css';
import './theme.tui.css';
import './app.css';

document.documentElement.dataset.theme = 'tui';

let lastDecorations: boolean | null = null;
settings.subscribe((s) => {
  const next = s.ui?.theme || 'tui';
  if (document.documentElement.dataset.theme !== next) {
    document.documentElement.dataset.theme = next;
  }
  const wantDecorations = next !== 'tui';
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
