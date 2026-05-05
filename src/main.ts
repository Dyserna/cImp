import { mount } from 'svelte';
import App from './App.svelte';
import { ttsTest } from './lib/ipc';
import './app.css';

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
