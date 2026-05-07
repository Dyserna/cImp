import { mount } from 'svelte';
import SettingsApp from './SettingsApp.svelte';
import './theme.css';
import './app.css';

document.documentElement.dataset.theme = 'modern-dark';

const target = document.getElementById('app');
if (!target) {
  throw new Error('#app root element not found');
}

const app = mount(SettingsApp, { target });

export default app;
