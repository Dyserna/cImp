import { mount } from 'svelte';
import App from './App.svelte';
import { ttsTest } from './lib/ipc';
import './theme.css';
import './app.css';

// Set the active theme synchronously before Svelte mounts so the first
// paint already reflects token values — avoids FOUC. The static
// `data-theme` attribute on <html> is a defense-in-depth fallback.
document.documentElement.dataset.theme = 'modern-dark';

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
