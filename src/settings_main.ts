import { mount } from 'svelte';
import SettingsApp from './SettingsApp.svelte';
import './app.css';

const target = document.getElementById('app');
if (!target) {
  throw new Error('#app root element not found');
}

const app = mount(SettingsApp, { target });

export default app;
