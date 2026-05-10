import { mount } from 'svelte';
import App from './App.svelte';
import { ttsTest } from './lib/ipc';
import { settings } from './lib/settings/store';
import { getCurrentWindow } from '@tauri-apps/api/window';
import './theme.css';
import './theme.tui-yellow.css';
import './theme.tui-purple.css';
import './app.css';

// Set the active theme synchronously before Svelte mounts so the first
// paint already reflects token values — avoids FOUC. The static
// `data-theme` attribute on <html> (set in index.html) is a defense-in-
// depth fallback. Both default to "tui-yellow" — matches the new-install
// default in defaultSettings(), and existing modern-dark users have a
// persisted setting that overrides this on the first subscribe tick.
document.documentElement.dataset.theme = 'tui-yellow';

// Follow the persisted ui.theme value: the store starts on defaults
// ("tui-yellow"), then reflects the backend value once initSettings()
// runs from inside App.svelte. The subscription survives for the
// lifetime of the window.
//
// Also drives OS chrome: modern-dark uses the platform's native title
// bar; every `tui-*` variant hides it (`setDecorations(false)`) and
// renders the custom `TuiTitleBar` Svelte component instead. The two
// stay in sync via this single subscription.
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
