import { invoke } from '@tauri-apps/api/core';
import type { AiToolTabConfig, Settings } from './types';
import type { TabId } from '../tabs/types';

export async function settingsGet(): Promise<Settings> {
  return invoke('settings_get');
}

export async function settingsUpdate(settings: Settings): Promise<void> {
  await invoke('settings_update', { settings });
}

export async function listVoices(): Promise<string[]> {
  return invoke('list_voices');
}

export async function openSettingsWindow(): Promise<void> {
  await invoke('open_settings_window');
}

/// V1.4-07 A: open the Settings window scrolled to a specific tab's
/// section. Used by the right-click "Configure tab" entry on AI tabs
/// (shell tabs continue to use ConfigureTabDialog.svelte).
export async function openSettingsWindowToTab(tab: TabId): Promise<void> {
  await invoke('open_settings_window_to_tab', { tab });
}

/// V1.4-07 A: read+clear any pending deep-link target stored by
/// `open_settings_window_to_tab`. The Settings window calls this on
/// mount to handle the cold-open case (window not yet listening for
/// the `settings-deep-link` event when the IPC fires).
export async function consumeSettingsDeepLink(): Promise<string | null> {
  return invoke('consume_settings_deep_link');
}

export async function closeSettingsWindow(): Promise<void> {
  await invoke('close_settings_window');
}

export async function requestTabRestart(tab: TabId): Promise<void> {
  await invoke('request_tab_restart', { tab });
}

/// Per-AI-tab default config from the backend. Used by the Settings
/// window's "Reset to default" buttons to match the Rust-side defaults
/// exactly. Returns an error from the backend for non-AI ids.
export async function aiToolTabDefaults(tab: TabId): Promise<AiToolTabConfig> {
  return invoke('ai_tool_tab_defaults', { tab });
}

/// Activate a tab AND persist its id as `session.active_tab_id` so it's
/// restored on next launch. Backend debounces the save.
export async function setActiveTab(tab: TabId): Promise<void> {
  await invoke('set_active_tab', { tab });
}
