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

export async function closeSettingsWindow(): Promise<void> {
  await invoke('close_settings_window');
}

export async function requestTabRestart(tab: TabId): Promise<void> {
  await invoke('request_tab_restart', { tab });
}

/// Per-AI-tab default config from the backend. Used by the Settings
/// window's "Reset to default" buttons to match the Rust-side defaults
/// exactly (notably the embedded RUNTIME_SYSTEM_PROMPT for Claude's TTS
/// instructions). Returns an error from the backend for non-AI ids.
export async function aiToolTabDefaults(tab: TabId): Promise<AiToolTabConfig> {
  return invoke('ai_tool_tab_defaults', { tab });
}

/// Activate a tab AND persist its id as `session.active_tab_id` so it's
/// restored on next launch. Backend debounces the save.
export async function setActiveTab(tab: TabId): Promise<void> {
  await invoke('set_active_tab', { tab });
}
